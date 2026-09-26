//! Client for IQAir's undocumented cloud API.
//!
//! Protocol details adapted from https://github.com/ThioJoe/HA-IQAir-Integration and
//! checked against the `grpc.klr.v1` messages in the IQAir dashboard's JS bundle.

use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const DASHBOARD_URL: &str = "https://dashboard.iqair.com/";
const SIGNIN_URL: &str = "https://website-api.airvisual.com/v2/auth/signin/by/email";
const DEVICES_URL: &str = "https://website-api.airvisual.com/v2/users/{user_id}/devices";
const GRPC_BASE: &str = "https://cloud-api.iqair.io/";

/// The Atem X speaks the KLR service; serial numbers look like `KLR_xxxx`.
const KLR_SERVICE: &str = "grpc.klr.v1.KLRService";
const KLR_PREFIX: &str = "KLR_";

/// Protobuf tags: field 2 varint, field 3 varint.
const TAG_F2: u8 = 0x10;
const TAG_F3: u8 = 0x18;

/// `grpc.klr.v1.PowerMode`: OFF=1, ON=2, STANDBY=3. The unit's own button goes to standby.
const POWER_ON: u8 = 2;
const POWER_STANDBY: u8 = 3;

#[derive(Debug)]
pub enum ApiError {
    /// The login token was rejected; sign in again.
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized => write!(f, "IQAir rejected the login token"),
            ApiError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        ApiError::Other(format!("HTTP error: {e}"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Credentials {
    pub email: String,
    pub password: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub user_id: String,
    pub login_token: String,
    /// Bearer token for the gRPC API, embedded in the dashboard's JS bundle.
    pub auth_token: String,
}

/// A device control the cloud accepts.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(tag = "action", content = "value", rename_all = "snake_case")]
pub enum Control {
    Power(bool),
    Speed(u8),
    Auto(bool),
    Profile(u8),
    Light(bool),
    Brightness(u8),
    Lock(bool),
}

/// The purifier state we care about, parsed from the devices endpoint.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct DeviceState {
    pub id: String,
    pub serial: String,
    pub name: String,
    pub model: String,
    pub online: bool,
    pub last_seen: Option<String>,
    pub power: bool,
    /// Manual speed setting, 1..=max_speed (0 when the cloud doesn't report one).
    pub speed_level: u8,
    /// The speed the fan is actually running at (also under auto).
    pub fan_speed: u8,
    pub max_speed: u8,
    pub auto_mode: bool,
    /// 1=quiet 2=balanced 3=power
    pub auto_profile: u8,
    pub light: bool,
    pub lock: bool,
    pub pm25: Option<f64>,
    pub co2: Option<f64>,
    pub temperature_f: Option<f64>,
    pub humidity: Option<f64>,
    pub filter_health: Option<u8>,
    pub filter_type: Option<String>,
}

impl DeviceState {
    fn from_json(d: &Value) -> Self {
        let remote = &d["remote"];
        let current = &d["current"];
        let u8_of = |v: &Value| v.as_u64().map(|n| n.min(255) as u8);
        let reading = |key: &str| current[key]["value"].as_f64();

        let max_speed = u8_of(&remote["maxSpeedLevel"]).filter(|&n| n > 0).unwrap_or(8);

        Self {
            id: d["id"].as_str().unwrap_or_default().to_string(),
            serial: d["serialNumber"].as_str().unwrap_or_default().to_string(),
            name: d["name"].as_str().unwrap_or("Atem X").to_string(),
            model: d["modelLabel"].as_str().unwrap_or("Atem X").to_string(),
            online: d["isConnected"].as_bool().unwrap_or(false),
            last_seen: d["lastSeenAt"].as_str().map(str::to_string),
            power: u8_of(&remote["powerMode"]) == Some(POWER_ON),
            speed_level: u8_of(&remote["speedLevel"]).unwrap_or(0).min(max_speed),
            fan_speed: u8_of(&current["fanSpeed"]).unwrap_or(0).min(max_speed),
            max_speed,
            auto_mode: remote["autoModeEnabled"].as_bool().unwrap_or(false),
            auto_profile: u8_of(&remote["autoModeProfile"]).unwrap_or(2),
            light: remote["lightIndicatorEnabled"].as_bool().unwrap_or(false),
            lock: remote["isLocksEnabled"].as_bool().unwrap_or(false),
            pm25: reading("pm25"),
            co2: reading("co2"),
            temperature_f: reading("temperature"),
            humidity: reading("humidity"),
            filter_health: u8_of(&remote["filters"][0]["healthPercent"]),
            filter_type: d["filterMaintenance"][0]["filterType"].as_str().map(str::to_string),
        }
    }

    /// The manual speed to show: the setting if the cloud reports one, else what it's running at.
    pub fn manual_speed(&self) -> u8 {
        if self.speed_level > 0 {
            self.speed_level
        } else {
            self.fan_speed.max(1)
        }
    }

    /// Reflect a control that the cloud just accepted, ahead of the next poll.
    pub fn apply(&mut self, control: Control) {
        match control {
            Control::Power(on) => self.power = on,
            Control::Speed(level) => {
                self.power = true;
                self.auto_mode = false;
                self.speed_level = level;
                self.fan_speed = level;
            }
            Control::Auto(on) => {
                self.auto_mode = on;
                if on {
                    self.power = true;
                }
            }
            Control::Profile(p) => self.auto_profile = p,
            Control::Light(on) => self.light = on,
            Control::Brightness(_) => self.light = true,
            Control::Lock(on) => self.lock = on,
        }
    }
}

pub struct Client {
    http: reqwest::blocking::Client,
}

impl Client {
    pub fn new() -> Self {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("iqair-matter/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("HTTP client");

        Self { http }
    }

    pub fn login(&self, creds: &Credentials) -> Result<Session, ApiError> {
        let resp = self
            .http
            .post(SIGNIN_URL)
            .json(&serde_json::json!({
                "email": creds.email,
                "password": creds.password,
            }))
            .send()?;

        let status = resp.status().as_u16();
        if status >= 400 {
            let body = resp.text().unwrap_or_default();
            return Err(ApiError::Other(format!(
                "sign-in failed (HTTP {status}): {}",
                truncate(&body, 200)
            )));
        }

        let signin: Value = resp.json()?;
        let user_id = signin["id"].as_str().ok_or_else(|| other("sign-in response had no id"))?;
        let login_token = signin["loginToken"]
            .as_str()
            .ok_or_else(|| other("sign-in response had no loginToken"))?;

        Ok(Session {
            user_id: user_id.to_string(),
            login_token: login_token.to_string(),
            auth_token: self.fetch_grpc_token()?,
        })
    }

    /// The gRPC bearer token is hardcoded in the dashboard's main JS bundle.
    fn fetch_grpc_token(&self) -> Result<String, ApiError> {
        let html = self.get_text(DASHBOARD_URL)?;
        let bundle = between(&html, "src=\"", "\"", |s| {
            let s = s.trim_start_matches('/');
            s.starts_with("main.") && s.ends_with(".js")
        })
        .ok_or_else(|| other("couldn't find the dashboard's main JS bundle"))?;

        let js = self.get_text(&format!("{DASHBOARD_URL}{}", bundle.trim_start_matches('/')))?;
        between(&js, "cloudApiAuthToken:\"Bearer ", "\"", |_| true)
            .map(str::to_string)
            .ok_or_else(|| other("couldn't find cloudApiAuthToken in the dashboard JS"))
    }

    fn get_text(&self, url: &str) -> Result<String, ApiError> {
        let resp = self.http.get(url).send()?;
        if resp.status().as_u16() >= 400 {
            return Err(other(&format!("GET {url} failed: HTTP {}", resp.status())));
        }
        Ok(resp.text()?)
    }

    /// All devices on the account, as raw JSON.
    pub fn devices(&self, session: &Session) -> Result<Vec<Value>, ApiError> {
        let url = DEVICES_URL.replace("{user_id}", &session.user_id);
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("page", "1"),
                ("perPage", "15"),
                ("units.system", "imperial"),
                ("AQI", "US"),
                ("language", "en"),
            ])
            .header("x-login-token", &session.login_token)
            .send()?;

        match resp.status().as_u16() {
            401 | 403 => Err(ApiError::Unauthorized),
            s if s >= 400 => Err(other(&format!("device list failed: HTTP {s}"))),
            _ => Ok(resp.json()?),
        }
    }

    /// Pick the Atem X (or the serial given) out of the account's devices.
    pub fn device_state(
        &self,
        session: &Session,
        serial: Option<&str>,
    ) -> Result<DeviceState, ApiError> {
        let devices = self.devices(session)?;
        let device = devices
            .iter()
            .find(|d| {
                let sn = d["serialNumber"].as_str().unwrap_or_default();
                match serial {
                    Some(want) => sn.eq_ignore_ascii_case(want),
                    None => sn.to_ascii_uppercase().starts_with(KLR_PREFIX),
                }
            })
            .ok_or_else(|| match serial {
                Some(s) => other(&format!("no device with serial {s} on this account")),
                None => other("no Atem X (KLR_…) device on this account"),
            })?;

        Ok(DeviceState::from_json(device))
    }

    pub fn send(&self, session: &Session, serial: &str, control: Control) -> Result<(), ApiError> {
        let (method, tag, value) = match control {
            Control::Power(on) => ("SetPowerMode", TAG_F2, Some(if on { POWER_ON } else { POWER_STANDBY })),
            Control::Speed(level) => ("SetFanSpeed", TAG_F3, Some(level)),
            Control::Auto(on) => ("SetAutoMode", TAG_F2, on.then_some(1)),
            Control::Profile(p) => ("SetAutoModeProfile", TAG_F2, Some(p)),
            Control::Light(on) => ("SetLightIndicator", TAG_F2, on.then_some(1)),
            Control::Brightness(level) => ("SetLightLevel", TAG_F2, Some(level)),
            Control::Lock(on) => ("SetDefaultLocks", TAG_F2, on.then_some(1)),
        };

        let url = format!("{GRPC_BASE}{KLR_SERVICE}/{method}");
        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/grpc-web-text")
            .header("Accept", "application/grpc-web-text")
            .header("X-User-Agent", "grpc-web-javascript/0.1")
            .header("Authorization", format!("Bearer {}", session.auth_token))
            .body(grpc_payload(serial, tag, value))
            .send()?;

        let status = resp.status().as_u16();
        let header_status = resp
            .headers()
            .get("grpc-status")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = resp.text().unwrap_or_default();
        // grpc-web may put the status in the headers or in a trailers frame in the body.
        let grpc_status = header_status.or_else(|| trailer_status(&body));

        match (status, grpc_status.as_deref()) {
            (401 | 403, _) | (_, Some("16")) => Err(ApiError::Unauthorized),
            (s, _) if s >= 400 => Err(other(&format!("{method} failed: HTTP {s}"))),
            (_, Some(code)) if code != "0" => Err(other(&format!("{method} failed: grpc-status {code}"))),
            _ => Ok(()),
        }
    }
}

/// A base64 grpc-web-text frame carrying `{1: serial, <tag>: value}`.
fn grpc_payload(serial: &str, tag: u8, value: Option<u8>) -> String {
    let sn = serial
        .strip_prefix(KLR_PREFIX)
        .or_else(|| serial.strip_prefix("klr_"))
        .unwrap_or(serial)
        .to_ascii_lowercase();

    let mut body = vec![0x0A, sn.len() as u8];
    body.extend_from_slice(sn.as_bytes());
    if let Some(v) = value {
        body.extend_from_slice(&[tag, v]);
    }

    let mut frame = vec![0x00];
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);

    base64::engine::general_purpose::STANDARD.encode(frame)
}

/// Pull `grpc-status` out of a trailers frame (type 0x80) in a grpc-web-text body.
fn trailer_status(body: &str) -> Option<String> {
    // Frames are concatenated base64 strings; each ends at its padding.
    let mut frames = Vec::new();
    let mut start = 0;
    let bytes = body.trim().as_bytes();
    for i in 0..bytes.len() {
        let end_of_padding = bytes[i] == b'=' && bytes.get(i + 1).is_some_and(|&c| c != b'=');
        if end_of_padding {
            frames.push(&body.trim()[start..=i]);
            start = i + 1;
        }
    }
    frames.push(&body.trim()[start..]);

    frames.into_iter().find_map(|f| {
        let raw = base64::engine::general_purpose::STANDARD.decode(f).ok()?;
        if raw.first() != Some(&0x80) || raw.len() < 5 {
            return None;
        }
        let text = String::from_utf8_lossy(&raw[5..]).to_string();
        text.lines()
            .find_map(|l| l.trim().strip_prefix("grpc-status:").map(|v| v.trim().to_string()))
    })
}

/// Find the first `start…end` span whose contents pass `accept`.
fn between<'a>(haystack: &'a str, start: &str, end: &str, accept: impl Fn(&str) -> bool) -> Option<&'a str> {
    let mut rest = haystack;
    while let Some(i) = rest.find(start) {
        rest = &rest[i + start.len()..];
        let j = rest.find(end)?;
        let candidate = &rest[..j];
        if accept(candidate) {
            return Some(candidate);
        }
    }
    None
}

fn truncate(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn other(msg: &str) -> ApiError {
    ApiError::Other(msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_matches_python_client() {
        // Same framing as iqair.py: 0x00, u32 len, 0x0A len serial, tag value.
        let p = grpc_payload("KLR_ABC", TAG_F3, Some(3));
        let raw = base64::engine::general_purpose::STANDARD.decode(p).unwrap();
        assert_eq!(raw, vec![0, 0, 0, 0, 7, 0x0A, 3, b'a', b'b', b'c', 0x18, 3]);
    }

    #[test]
    fn payload_without_value_omits_field() {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(grpc_payload("KLR_X", TAG_F2, None))
            .unwrap();
        assert_eq!(raw, vec![0, 0, 0, 0, 3, 0x0A, 1, b'x']);
    }

    #[test]
    fn finds_trailer_status() {
        let data = base64::engine::general_purpose::STANDARD.encode([0u8, 0, 0, 0, 2, 0x08, 1]);
        let trailer_text = b"grpc-status:0\r\ngrpc-message:\r\n";
        let mut t = vec![0x80, 0, 0, 0, trailer_text.len() as u8];
        t.extend_from_slice(trailer_text);
        let trailer = base64::engine::general_purpose::STANDARD.encode(t);
        assert_eq!(trailer_status(&format!("{data}{trailer}")).as_deref(), Some("0"));
    }

    #[test]
    fn parses_device_state() {
        let d = serde_json::json!({
            "id": "klr_1", "serialNumber": "KLR_1", "name": "Bedroom", "modelLabel": "Atem X",
            "isConnected": true,
            "remote": {"powerMode": 2, "speedLevel": 0, "maxSpeedLevel": 8, "autoModeEnabled": false,
                       "autoModeProfile": 2, "lightIndicatorEnabled": false, "isLocksEnabled": false,
                       "filters": [{"healthPercent": 74}]},
            "current": {"fanSpeed": 2, "pm25": {"value": 0}, "co2": {"value": 419},
                        "temperature": {"value": 72.5}, "humidity": {"value": 57.5}}
        });
        let s = DeviceState::from_json(&d);
        assert!(s.power);
        assert_eq!((s.speed_level, s.fan_speed, s.max_speed, s.manual_speed()), (0, 2, 8, 2));
        assert_eq!(s.co2, Some(419.0));
        assert_eq!(s.filter_health, Some(74));
    }
}
