//! Matter bridge + local web UI for the IQAir Atem X.
//!
//! Three threads: the IQAir worker (all cloud traffic), the Matter device, and the web UI.
//!
//! Environment:
//!   IQAIR_EMAIL / IQAIR_PASSWORD   IQAir account (else set them in the web UI)
//!   IQAIR_SERIAL                   pick a device by serial (default: first KLR_… device)
//!   DATA_DIR                       state directory (default /data)
//!   POLL_SECONDS                   cloud poll interval (default 30)
//!   WEB_BIND                       web UI address (default 0.0.0.0:8080; "off" disables)
//!   WEB_PASSWORD                   optional basic-auth password for the web UI
//!   MATTER_DEVICE_TYPE             "purifier" (default) or "fan"
//!   MATTER_INTERFACE               interface name or IPv4 address to advertise on (default: auto)
//!   MATTER_PORT                    Matter UDP port (default 5540)
//!   RUST_LOG                       log level (default info)

mod hub;
mod iqair;
mod matter;
mod mdns;
mod web;

use std::path::PathBuf;
use std::time::Duration;

use log::{error, info};

use hub::{Hub, WorkerConfig};
use iqair::Credentials;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// `iqair-matter healthcheck`: exit 0 if the web UI answers /health with 200.
/// The container image has no curl, so the Docker HEALTHCHECK uses this.
fn healthcheck() -> ! {
    use std::io::{Read, Write};

    let bind = env("WEB_BIND").unwrap_or_else(|| "0.0.0.0:8080".into());
    let port = bind.rsplit(':').next().unwrap_or("8080");
    let ok = std::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .and_then(|mut s| {
            s.set_read_timeout(Some(Duration::from_secs(5)))?;
            s.write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
            let mut resp = String::new();
            s.read_to_string(&mut resp)?;
            Ok(resp.starts_with("HTTP/1.1 200") || resp.starts_with("HTTP/1.0 200"))
        })
        .unwrap_or(false);
    std::process::exit(if ok { 0 } else { 1 });
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        healthcheck();
    }

    env_logger::init_from_env(
        env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
    );

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install TLS crypto provider");

    let data_dir = PathBuf::from(env("DATA_DIR").unwrap_or_else(|| "/data".into()));
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        error!("can't create {}: {e}", data_dir.display());
        std::process::exit(1);
    }

    let purifier = match env("MATTER_DEVICE_TYPE").as_deref() {
        None | Some("purifier") => true,
        Some("fan") => false,
        Some(other) => {
            error!("MATTER_DEVICE_TYPE must be \"purifier\" or \"fan\", got {other:?}");
            std::process::exit(1);
        }
    };

    let (hub, rx) = Hub::new(if purifier { "purifier" } else { "fan" });

    let worker_cfg = WorkerConfig {
        data_dir: data_dir.clone(),
        env_credentials: match (env("IQAIR_EMAIL"), env("IQAIR_PASSWORD")) {
            (Some(email), Some(password)) => Some(Credentials { email, password }),
            _ => None,
        },
        serial: env("IQAIR_SERIAL"),
        poll_interval: Duration::from_secs(
            env("POLL_SECONDS")
                .and_then(|s| s.parse().ok())
                .unwrap_or(30u64)
                .max(10),
        ),
    };
    let worker_hub = hub.clone();
    std::thread::Builder::new()
        .name("iqair".into())
        .spawn(move || hub::run_worker(worker_hub, rx.requests, worker_cfg))
        .expect("spawn worker");

    let bind = env("WEB_BIND").unwrap_or_else(|| "0.0.0.0:8080".into());
    if bind != "off" {
        let web_hub = hub.clone();
        let web_cfg = web::WebConfig {
            bind,
            password: env("WEB_PASSWORD"),
        };
        std::thread::Builder::new()
            .name("web".into())
            .spawn(move || web::serve(web_hub, web_cfg))
            .expect("spawn web");
    }

    let matter_cfg = matter::MatterConfig {
        data_dir,
        purifier,
        port: env("MATTER_PORT").and_then(|p| p.parse().ok()),
        interface: env("MATTER_INTERFACE"),
    };
    let channels = matter::MatterChannels {
        fan: rx.matter_fan,
        power: rx.matter_power,
        open_pairing: rx.open_pairing,
    };

    info!(
        "Starting Matter device ({})",
        if purifier { "air purifier" } else { "fan" }
    );
    // rs-matter's futures are large; give the thread plenty of stack.
    let matter_thread = std::thread::Builder::new()
        .name("matter".into())
        .stack_size(4 * 1024 * 1024)
        .spawn(move || matter::run(hub, channels, matter_cfg))
        .expect("spawn matter");

    match matter_thread.join() {
        Ok(Ok(())) => info!("Matter stopped"),
        Ok(Err(e)) => {
            error!("Matter failed: {e:?}");
            std::process::exit(1);
        }
        Err(_) => std::process::exit(1),
    }
}
