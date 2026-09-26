//! rs-matter's built-in mDNS responder on the LAN interface.
//!
//! Adapted from rs-matter's examples (Apache-2.0).

use std::net::{Ipv4Addr as StdIpv4, Ipv6Addr as StdIpv6, UdpSocket};
use std::time::{Duration, Instant};

use embassy_futures::select::{select, Either};
use log::{info, warn};

use rs_matter::crypto::Crypto;
use rs_matter::error::{Error, ErrorCode};
use rs_matter::transport::network::mdns::builtin::{BuiltinMdns, Host};
use rs_matter::transport::network::mdns::{
    MDNS_IPV4_BROADCAST_ADDR, MDNS_IPV6_BROADCAST_ADDR, MDNS_SOCKET_DEFAULT_BIND_ADDR,
};
use rs_matter::transport::network::Ipv6Addr;
use rs_matter::Matter;
use socket2::{Domain, Protocol, Socket, Type};

/// How long to wait at startup for the interface's IPv6 link-local address. The kernel
/// adds it when the interface comes up, which in a fresh container can be just after we
/// start, and it's unusable ("tentative") until duplicate-address detection finishes.
const LINK_LOCAL_WAIT: Duration = Duration::from_secs(10);
const DAD_SETTLE: Duration = Duration::from_secs(2);
/// How often to re-read the interface's addresses (SLAAC, DHCP renewals) and re-announce.
const RECHECK: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq)]
struct Net {
    name: String,
    ipv4: StdIpv4,
    ipv6: Vec<StdIpv6>,
    index: u32,
}

pub async fn run<C: Crypto + Copy>(
    matter: &Matter<'_>,
    crypto: C,
    hostname: &str,
    interface: Option<&str>,
) -> Result<(), Error> {
    let mut net = settle(interface).await?;

    loop {
        if net.ipv6.is_empty() {
            warn!(
                "{} has no IPv6 address; Matter controllers may not reach it",
                net.name
            );
        }
        info!("mDNS on {}: {} / {:?}", net.name, net.ipv4, net.ipv6);

        let socket = open_socket(&net)?;
        let ipv6: Vec<Ipv6Addr> = net.ipv6.iter().map(|ip| ip.octets().into()).collect();
        let host = Host {
            hostname,
            ip: net.ipv4.octets().into(),
            ipv6: &ipv6,
        };
        let mut responder = BuiltinMdns::new();
        let mdns = responder.run(
            &socket,
            &socket,
            &host,
            Some(net.ipv4.octets().into()),
            Some(net.index),
            matter,
            crypto,
        );

        match select(mdns, changed(interface, &net)).await {
            Either::First(result) => return result,
            Either::Second(next) => {
                info!("network addresses changed; re-announcing");
                net = next;
            }
        }
    }
}

/// Pick the interface, waiting briefly for its link-local address to be usable.
async fn settle(interface: Option<&str>) -> Result<Net, Error> {
    let deadline = Instant::now() + LINK_LOCAL_WAIT;
    loop {
        let net = pick_interface(interface)?;
        if net.ipv6.iter().any(|ip| ip.is_unicast_link_local()) {
            async_io::Timer::after(DAD_SETTLE).await;
            return pick_interface(interface);
        }
        if Instant::now() >= deadline {
            return Ok(net);
        }
        async_io::Timer::after(Duration::from_millis(250)).await;
    }
}

/// Resolve once the interface's addresses differ from `current`.
async fn changed(interface: Option<&str>, current: &Net) -> Net {
    loop {
        async_io::Timer::after(RECHECK).await;
        match pick_interface(interface) {
            Ok(net) if net != *current => return net,
            _ => {}
        }
    }
}

fn open_socket(net: &Net) -> Result<async_io::Async<UdpSocket>, Error> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    // Share 5353 with a system responder (mDNSResponder, avahi) if one is running.
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_only_v6(false)?;
    socket.bind(&MDNS_SOCKET_DEFAULT_BIND_ADDR.into())?;
    let socket = async_io::Async::<UdpSocket>::new_nonblocking(socket.into())?;

    socket
        .get_ref()
        .join_multicast_v6(&MDNS_IPV6_BROADCAST_ADDR, net.index)
        .inspect_err(|e| warn!("mDNS: joining the IPv6 group on {} failed: {e}", net.name))?;
    // Linux lets a dual-stack socket join the IPv4 group; macOS doesn't. Matter
    // discovery works over IPv6 alone, so carry on without it.
    if let Err(e) = socket
        .get_ref()
        .join_multicast_v4(&MDNS_IPV4_BROADCAST_ADDR, &net.ipv4)
    {
        warn!(
            "mDNS: couldn't join the IPv4 group on {} ({e}); advertising over IPv6 only",
            net.ipv4
        );
    }

    Ok(socket)
}

/// The interface to advertise on: `MATTER_INTERFACE` if set (an interface name, or an IPv4
/// address to pick whichever interface holds it), else the first non-loopback interface that
/// has both IPv4 and IPv6 (preferring one with a link-local IPv6 address).
fn pick_interface(wanted: Option<&str>) -> Result<Net, Error> {
    let all = if_addrs::get_if_addrs().map_err(|_| ErrorCode::StdIoError)?;

    let names: Vec<&str> = match wanted {
        // An address: useful when the container sits on several networks and the
        // interface names aren't predictable.
        Some(addr) if addr.parse::<std::net::Ipv4Addr>().is_ok() => all
            .iter()
            .filter(
                |ia| matches!(ia.addr, if_addrs::IfAddr::V4(ref v4) if v4.ip.to_string() == addr),
            )
            .map(|ia| ia.name.as_str())
            .take(1)
            .collect(),
        Some(name) => vec![name],
        None => {
            let mut names: Vec<&str> = all
                .iter()
                .filter(|ia| !ia.is_loopback())
                .map(|ia| ia.name.as_str())
                .collect();
            names.dedup();
            // Prefer interfaces with a link-local IPv6 address.
            names.sort_by_key(|n| {
                !all.iter().any(|ia| {
                    ia.name == *n
                        && matches!(ia.addr, if_addrs::IfAddr::V6(ref v6) if v6.ip.is_unicast_link_local())
                })
            });
            names
        }
    };

    for name in names {
        let on_iface = || all.iter().filter(move |ia| ia.name == name);
        let ipv4 = on_iface().find_map(|ia| match ia.addr {
            if_addrs::IfAddr::V4(ref v4) if !v4.ip.is_loopback() => Some(v4.ip),
            _ => None,
        });
        let all_v6: Vec<std::net::Ipv6Addr> = on_iface()
            .filter_map(|ia| match ia.addr {
                if_addrs::IfAddr::V6(ref v6) if !v6.ip.is_loopback() => Some(v6.ip),
                _ => None,
            })
            .collect();
        // Skip unique-local (fc00::/7) addresses when there's anything else: Docker hands
        // one out when IPv6 is enabled on a macvlan network, but nothing else on the LAN
        // can route to it. Controllers reach us on the link-local address instead.
        let routable: Vec<_> = all_v6
            .iter()
            .copied()
            .filter(|ip| (ip.segments()[0] & 0xfe00) != 0xfc00)
            .collect();
        let ipv6 = if routable.is_empty() {
            all_v6
        } else {
            routable
        };
        let index = on_iface().find_map(|ia| ia.index).unwrap_or(0);

        if let Some(ipv4) = ipv4 {
            return Ok(Net {
                name: name.to_string(),
                ipv4,
                ipv6,
                index,
            });
        }
    }

    warn!("no usable network interface for mDNS (wanted: {wanted:?})");
    Err(ErrorCode::StdIoError.into())
}
