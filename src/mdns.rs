//! rs-matter's built-in mDNS responder on the LAN interface.
//!
//! Adapted from rs-matter's examples (Apache-2.0).

use std::net::UdpSocket;

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

pub async fn run<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    hostname: &str,
    interface: Option<&str>,
) -> Result<(), Error> {
    let (ipv4, ipv6, index) = pick_interface(interface)?;

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
        .join_multicast_v6(&MDNS_IPV6_BROADCAST_ADDR, index)
        .inspect_err(|e| warn!("mDNS: joining the IPv6 group on interface #{index} failed: {e}"))?;
    // Linux lets a dual-stack socket join the IPv4 group; macOS doesn't. Matter
    // discovery works over IPv6 alone, so carry on without it.
    if let Err(e) = socket
        .get_ref()
        .join_multicast_v4(&MDNS_IPV4_BROADCAST_ADDR, &ipv4)
    {
        warn!("mDNS: couldn't join the IPv4 group on {ipv4} ({e}); advertising over IPv6 only");
    }

    BuiltinMdns::new()
        .run(
            &socket,
            &socket,
            &Host {
                hostname,
                ip: ipv4.octets().into(),
                ipv6: &ipv6,
            },
            Some(ipv4.octets().into()),
            Some(index),
            matter,
            crypto,
        )
        .await
}

/// The interface to advertise on: `MATTER_INTERFACE` if set, else the first non-loopback
/// interface that has both IPv4 and IPv6 (preferring one with a link-local IPv6 address).
fn pick_interface(
    wanted: Option<&str>,
) -> Result<(std::net::Ipv4Addr, Vec<Ipv6Addr>, u32), Error> {
    let all = if_addrs::get_if_addrs().map_err(|_| ErrorCode::StdIoError)?;

    let names: Vec<&str> = match wanted {
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
        let ipv6: Vec<Ipv6Addr> = on_iface()
            .filter_map(|ia| match ia.addr {
                if_addrs::IfAddr::V6(ref v6) if !v6.ip.is_loopback() => Some(v6.ip.octets().into()),
                _ => None,
            })
            .collect();
        let index = on_iface().find_map(|ia| ia.index).unwrap_or(0);

        if let Some(ipv4) = ipv4 {
            if ipv6.is_empty() {
                warn!("{name} has no IPv6 address; Matter controllers may not reach it");
            }
            info!("mDNS on {name}: {ipv4} / {ipv6:?}");
            return Ok((ipv4, ipv6, index));
        }
    }

    warn!("no usable network interface for mDNS (wanted: {wanted:?})");
    Err(ErrorCode::StdIoError.into())
}
