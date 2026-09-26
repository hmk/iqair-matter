//! Local web UI + JSON API.

use std::io::Read;
use std::sync::Arc;

use base64::Engine;
use log::{info, warn};
use qrcodegen::{QrCode, QrCodeEcc};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::hub::Hub;
use crate::iqair::{Control, Credentials};

const INDEX_HTML: &str = include_str!("../web/index.html");
const WORKERS: usize = 4;
const MAX_BODY: u64 = 16 * 1024;

pub struct WebConfig {
    pub bind: String,
    /// Optional HTTP basic-auth password (any username).
    pub password: Option<String>,
}

pub fn serve(hub: Arc<Hub>, cfg: WebConfig) {
    let server = match Server::http(&cfg.bind) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            warn!("web UI disabled: couldn't bind {}: {e}", cfg.bind);
            return;
        }
    };
    info!("Web UI on http://{}", cfg.bind);

    let password = Arc::new(cfg.password);
    let workers: Vec<_> = (0..WORKERS)
        .map(|_| {
            let (server, hub, password) = (server.clone(), hub.clone(), password.clone());
            std::thread::spawn(move || {
                while let Ok(req) = server.recv() {
                    handle(&hub, password.as_deref(), req);
                }
            })
        })
        .collect();

    for w in workers {
        let _ = w.join();
    }
}

fn handle(hub: &Hub, password: Option<&str>, mut req: Request) {
    let path = req.url().split('?').next().unwrap_or("/").to_string();
    let method = req.method().clone();

    if path == "/health" {
        let ok = hub.snapshot().error.is_none();
        let _ = req.respond(text(
            if ok { 200 } else { 503 },
            if ok { "ok" } else { "degraded" },
        ));
        return;
    }

    if let Some(pw) = password {
        if !authorized(&req, pw) {
            let resp = text(401, "unauthorized")
                .with_header(header("WWW-Authenticate", "Basic realm=\"iqair-matter\""));
            let _ = req.respond(resp);
            return;
        }
    }

    // State-changing requests must be JSON: a cross-site form can't send that without a
    // CORS preflight, which we never grant.
    if method == Method::Post && !is_json(&req) {
        let _ = req.respond(json_error(415, "expected application/json"));
        return;
    }

    let resp = match (method, path.as_str()) {
        (Method::Get, "/") => Response::from_string(INDEX_HTML)
            .with_header(header("Content-Type", "text/html; charset=utf-8"))
            .with_header(header("Cache-Control", "no-cache")),
        (Method::Get, "/api/state") => json(200, &hub.snapshot()),
        (Method::Get, "/pairing.svg") => {
            let qr = hub.snapshot().pairing.qr_text;
            if qr.is_empty() {
                json_error(404, "pairing code not ready")
            } else {
                Response::from_string(qr_svg(&qr))
                    .with_header(header("Content-Type", "image/svg+xml"))
                    .with_header(header("Cache-Control", "no-cache"))
            }
        }
        (Method::Post, "/api/control") => match body_json::<Control>(&mut req) {
            Ok(control) => match validate(control) {
                Ok(control) => match hub.send_and_wait(control) {
                    Ok(()) => json(200, &hub.snapshot()),
                    Err(e) => json_error(502, &e),
                },
                Err(e) => json_error(400, &e),
            },
            Err(e) => json_error(400, &e),
        },
        (Method::Post, "/api/credentials") => match body_json::<Credentials>(&mut req) {
            Ok(creds) if creds.email.trim().is_empty() || creds.password.is_empty() => {
                json_error(400, "email and password are required")
            }
            Ok(creds) => match hub.set_credentials(Credentials {
                email: creds.email.trim().to_string(),
                password: creds.password,
            }) {
                Ok(()) => json(200, &hub.snapshot()),
                Err(e) => json_error(400, &e),
            },
            Err(e) => json_error(400, &e),
        },
        (Method::Post, "/api/pairing/open") => {
            let _ = hub.open_pairing.try_send(());
            json(202, &serde_json::json!({ "ok": true }))
        }
        (Method::Post, "/api/refresh") => {
            hub.refresh();
            json(202, &serde_json::json!({ "ok": true }))
        }
        _ => json_error(404, "not found"),
    };

    let _ = req.respond(resp);
}

fn validate(c: Control) -> Result<Control, String> {
    let ok = match c {
        Control::Speed(n) => (1..=8).contains(&n),
        Control::Profile(n) | Control::Brightness(n) => (1..=3).contains(&n),
        _ => true,
    };
    ok.then_some(c)
        .ok_or_else(|| "value out of range".to_string())
}

fn authorized(req: &Request, password: &str) -> bool {
    req.headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .and_then(|h| h.value.as_str().strip_prefix("Basic "))
        .and_then(|b64| {
            base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .ok()
        })
        .and_then(|raw| String::from_utf8(raw).ok())
        .and_then(|userpass| userpass.split_once(':').map(|(_, pw)| pw.to_string()))
        .is_some_and(|pw| constant_time_eq(pw.as_bytes(), password.as_bytes()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn is_json(req: &Request) -> bool {
    req.headers()
        .iter()
        .find(|h| h.field.equiv("Content-Type"))
        .is_some_and(|h| h.value.as_str().starts_with("application/json"))
}

fn body_json<T: serde::de::DeserializeOwned>(req: &mut Request) -> Result<T, String> {
    let mut body = String::new();
    req.as_reader()
        .take(MAX_BODY)
        .read_to_string(&mut body)
        .map_err(|e| format!("couldn't read body: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("bad request: {e}"))
}

fn qr_svg(text: &str) -> String {
    let Ok(qr) = QrCode::encode_text(text, QrCodeEcc::Medium) else {
        return String::new();
    };
    let size = qr.size();
    let border = 2;
    let dim = size + border * 2;

    let mut path = String::new();
    for y in 0..size {
        for x in 0..size {
            if qr.get_module(x, y) {
                path.push_str(&format!("M{},{}h1v1h-1z", x + border, y + border));
            }
        }
    }

    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {dim} {dim}\" shape-rendering=\"crispEdges\">\
         <rect width=\"100%\" height=\"100%\" fill=\"#fff\"/><path d=\"{path}\" fill=\"#000\"/></svg>"
    )
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

type Resp = Response<std::io::Cursor<Vec<u8>>>;

fn json<T: serde::Serialize>(status: u16, value: &T) -> Resp {
    Response::from_string(serde_json::to_string(value).unwrap_or_else(|_| "{}".into()))
        .with_status_code(status)
        .with_header(header("Content-Type", "application/json"))
        .with_header(header("Cache-Control", "no-store"))
}

fn json_error(status: u16, msg: &str) -> Resp {
    json(status, &serde_json::json!({ "error": msg }))
}

fn text(status: u16, body: &str) -> Resp {
    Response::from_string(body).with_status_code(status)
}
