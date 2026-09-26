//! Shared state between the web UI, the Matter device and the IQAir worker thread.
//!
//! All cloud traffic goes through one worker thread: commands come in over a channel,
//! and the worker polls the device between them. Every state change is pushed to the
//! Matter side over async channels so it can report out-of-band changes.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::{info, warn};
use serde::Serialize;

use crate::iqair::{ApiError, Client, Control, Credentials, DeviceState, Session};

/// After a command, polls only refresh sensor readings for this long, so a stale cloud
/// snapshot doesn't bounce the controls back.
const HOLD_AFTER_COMMAND: Duration = Duration::from_secs(10);
/// How soon to re-poll after a command, to pick up the device's confirmed state.
const REPOLL_AFTER_COMMAND: Duration = Duration::from_secs(4);
const RETRY_AFTER_ERROR: Duration = Duration::from_secs(60);

pub enum Request {
    Control(Control, Option<mpsc::Sender<Result<(), String>>>),
    SetCredentials(Credentials, mpsc::Sender<Result<(), String>>),
    Refresh,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Pairing {
    pub qr_text: String,
    pub manual_code: String,
    pub fabrics: usize,
    pub window_open: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Snapshot {
    pub device: Option<DeviceState>,
    pub has_credentials: bool,
    /// Credentials came from the environment; the web UI can't change them.
    pub credentials_from_env: bool,
    pub account_email: Option<String>,
    pub error: Option<String>,
    pub last_poll: Option<u64>,
    pub pairing: Pairing,
    pub device_type: String,
}

pub struct Hub {
    snapshot: Mutex<Snapshot>,
    requests: mpsc::Sender<Request>,
    matter_fan: async_channel::Sender<()>,
    matter_power: async_channel::Sender<()>,
    pub open_pairing: async_channel::Sender<()>,
}

/// The receiving ends, handed to the worker and the Matter thread.
pub struct Receivers {
    pub requests: mpsc::Receiver<Request>,
    pub matter_fan: async_channel::Receiver<()>,
    pub matter_power: async_channel::Receiver<()>,
    pub open_pairing: async_channel::Receiver<()>,
}

impl Hub {
    pub fn new(device_type: &str) -> (Arc<Self>, Receivers) {
        let (req_tx, req_rx) = mpsc::channel();
        // Capacity 1: the Matter side only needs to know "something changed".
        let (fan_tx, fan_rx) = async_channel::bounded(1);
        let (power_tx, power_rx) = async_channel::bounded(1);
        let (pair_tx, pair_rx) = async_channel::bounded(1);

        let hub = Arc::new(Self {
            snapshot: Mutex::new(Snapshot {
                device_type: device_type.to_string(),
                ..Default::default()
            }),
            requests: req_tx,
            matter_fan: fan_tx,
            matter_power: power_tx,
            open_pairing: pair_tx,
        });

        let rx = Receivers {
            requests: req_rx,
            matter_fan: fan_rx,
            matter_power: power_rx,
            open_pairing: pair_rx,
        };

        (hub, rx)
    }

    pub fn snapshot(&self) -> Snapshot {
        self.lock().clone()
    }

    pub fn device(&self) -> Option<DeviceState> {
        self.lock().device.clone()
    }

    pub fn update_pairing(&self, pairing: Pairing) {
        self.lock().pairing = pairing;
    }

    /// Fire-and-forget; used by the Matter side, which can't block.
    pub fn send(&self, control: Control) {
        let _ = self.requests.send(Request::Control(control, None));
    }

    /// Send and wait for the cloud's answer; used by the web UI.
    pub fn send_and_wait(&self, control: Control) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.requests
            .send(Request::Control(control, Some(tx)))
            .map_err(|_| "worker stopped".to_string())?;
        rx.recv_timeout(Duration::from_secs(30))
            .map_err(|_| "timed out waiting for IQAir".to_string())?
    }

    pub fn set_credentials(&self, creds: Credentials) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.requests
            .send(Request::SetCredentials(creds, tx))
            .map_err(|_| "worker stopped".to_string())?;
        rx.recv_timeout(Duration::from_secs(60))
            .map_err(|_| "timed out waiting for IQAir".to_string())?
    }

    pub fn refresh(&self) {
        let _ = self.requests.send(Request::Refresh);
    }

    fn lock(&self) -> MutexGuard<'_, Snapshot> {
        self.snapshot.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn notify_matter(&self) {
        // Full channel means a notification is already pending, which is all we need.
        let _ = self.matter_fan.try_send(());
        let _ = self.matter_power.try_send(());
    }
}

pub struct WorkerConfig {
    pub data_dir: PathBuf,
    pub env_credentials: Option<Credentials>,
    pub serial: Option<String>,
    pub poll_interval: Duration,
}

pub fn run_worker(hub: Arc<Hub>, requests: mpsc::Receiver<Request>, cfg: WorkerConfig) {
    let mut worker = Worker::new(hub, cfg);
    let mut next_poll = Instant::now();

    loop {
        let wait = next_poll.saturating_duration_since(Instant::now());
        match requests.recv_timeout(wait) {
            Ok(Request::Control(control, reply)) => {
                let result = worker.control(control);
                if let Some(reply) = reply {
                    let _ = reply.send(result.map_err(|e| e.to_string()));
                }
                next_poll = Instant::now() + REPOLL_AFTER_COMMAND;
            }
            Ok(Request::SetCredentials(creds, reply)) => {
                let result = worker.set_credentials(creds);
                let _ = reply.send(result.map_err(|e| e.to_string()));
                next_poll = Instant::now();
            }
            Ok(Request::Refresh) => next_poll = Instant::now(),
            Err(RecvTimeoutError::Timeout) => {
                next_poll = Instant::now()
                    + match worker.poll() {
                        Ok(()) => worker.cfg.poll_interval,
                        Err(_) => RETRY_AFTER_ERROR.min(worker.cfg.poll_interval),
                    };
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

struct Worker {
    hub: Arc<Hub>,
    cfg: WorkerConfig,
    client: Client,
    credentials: Option<Credentials>,
    session: Option<Session>,
    hold_until: Option<Instant>,
}

impl Worker {
    fn new(hub: Arc<Hub>, cfg: WorkerConfig) -> Self {
        let from_env = cfg.env_credentials.is_some();
        let credentials = cfg
            .env_credentials
            .clone()
            .or_else(|| read_json(&cfg.data_dir.join("credentials.json")));
        let session = read_json(&cfg.data_dir.join("session.json"));

        {
            let mut s = hub.lock();
            s.has_credentials = credentials.is_some();
            s.credentials_from_env = from_env;
            s.account_email = credentials.as_ref().map(|c| c.email.clone());
            if credentials.is_none() {
                s.error = Some("No IQAir credentials yet. Sign in below.".into());
            }
        }

        Self {
            hub,
            cfg,
            client: Client::new(),
            credentials,
            session,
            hold_until: None,
        }
    }

    fn set_credentials(&mut self, creds: Credentials) -> Result<(), ApiError> {
        if self.cfg.env_credentials.is_some() {
            return Err(ApiError::Other(
                "credentials are set by environment variables".into(),
            ));
        }

        let session = self.client.login(&creds)?;
        write_json(&self.cfg.data_dir.join("credentials.json"), &creds);
        write_json(&self.cfg.data_dir.join("session.json"), &session);

        {
            let mut s = self.hub.lock();
            s.has_credentials = true;
            s.account_email = Some(creds.email.clone());
            s.error = None;
        }

        self.credentials = Some(creds);
        self.session = Some(session);
        info!("Signed in to IQAir");
        Ok(())
    }

    /// Run `f` with a valid session, signing in (again) if needed.
    fn with_session<T>(
        &mut self,
        f: impl Fn(&Client, &Session) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        if self.session.is_none() {
            self.sign_in()?;
        }

        match f(&self.client, self.session.as_ref().unwrap()) {
            Err(ApiError::Unauthorized) => {
                warn!("IQAir session expired; signing in again");
                self.sign_in()?;
                f(&self.client, self.session.as_ref().unwrap())
            }
            other => other,
        }
    }

    fn sign_in(&mut self) -> Result<(), ApiError> {
        let creds = self
            .credentials
            .as_ref()
            .ok_or_else(|| ApiError::Other("no IQAir credentials configured".into()))?;
        let session = self.client.login(creds)?;
        write_json(&self.cfg.data_dir.join("session.json"), &session);
        self.session = Some(session);
        Ok(())
    }

    fn poll(&mut self) -> Result<(), ApiError> {
        if self.credentials.is_none() && self.session.is_none() {
            return Err(ApiError::Other("no credentials".into()));
        }

        let serial = self.cfg.serial.clone();
        let result = self.with_session(|c, s| c.device_state(s, serial.as_deref()));

        let holding = self.hold_until.is_some_and(|t| Instant::now() < t);
        let changed = {
            let mut snap = self.hub.lock();
            snap.last_poll = Some(unix_now());
            match &result {
                Ok(fresh) => {
                    snap.error = None;
                    let merged = match (&snap.device, holding) {
                        // Keep the controls we just set; take everything else from the cloud.
                        (Some(prev), true) => DeviceState {
                            power: prev.power,
                            speed_level: prev.speed_level,
                            auto_mode: prev.auto_mode,
                            auto_profile: prev.auto_profile,
                            light: prev.light,
                            lock: prev.lock,
                            ..fresh.clone()
                        },
                        _ => fresh.clone(),
                    };
                    let changed = snap.device.as_ref() != Some(&merged);
                    snap.device = Some(merged);
                    changed
                }
                Err(e) => {
                    warn!("IQAir poll failed: {e}");
                    snap.error = Some(e.to_string());
                    false
                }
            }
        };

        if changed {
            self.hub.notify_matter();
        }

        result.map(|_| ())
    }

    fn control(&mut self, control: Control) -> Result<(), ApiError> {
        let serial = match self.hub.device() {
            Some(d) => d.serial,
            None => {
                self.poll()?;
                self.hub
                    .device()
                    .map(|d| d.serial)
                    .ok_or_else(|| ApiError::Other("purifier not found yet".into()))?
            }
        };

        info!("Sending {control:?}");
        self.with_session(|c, s| c.send(s, &serial, control))
            .inspect_err(|e| warn!("{control:?} failed: {e}"))?;

        if let Some(device) = self.hub.lock().device.as_mut() {
            device.apply(control);
        }
        self.hold_until = Some(Instant::now() + HOLD_AFTER_COMMAND);
        self.hub.notify_matter();

        Ok(())
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text)
        .inspect_err(|e| warn!("ignoring unreadable {}: {e}", path.display()))
        .ok()
}

/// Write a private (0600) JSON file.
pub fn write_json<T: Serialize>(path: &Path, value: &T) {
    let text = match serde_json::to_string_pretty(value) {
        Ok(t) => t,
        Err(e) => return warn!("couldn't serialise {}: {e}", path.display()),
    };

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);

    if let Err(e) = opts
        .open(path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, text.as_bytes()))
    {
        warn!("couldn't write {}: {e}", path.display());
    }
}
