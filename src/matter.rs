//! The Matter side: an Air Purifier (or Fan) endpoint with On/Off + Fan Control,
//! bridged to the IQAir worker through the [`Hub`].

use core::pin::pin;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use embassy_futures::select::{select, select3, Either};
use log::{info, warn};
use rand::RngExt;
use serde::{Deserialize, Serialize};

use rs_matter::crypto::{default_crypto, Crypto};
use rs_matter::dm::clusters::app::fan_control::{
    self, CurrentSpeed, FanControlHooks, FanModeEnum, FanModeSequenceEnum, FanSetting, Feature,
    OutOfBandMessage,
};
use rs_matter::dm::clusters::app::on_off::{self, OnOffHooks};
use rs_matter::dm::clusters::basic_info::BasicInfoConfig;
use rs_matter::dm::clusters::decl::fan_control as fan_control_cluster;
use rs_matter::dm::clusters::decl::on_off as on_off_cluster;
use rs_matter::dm::clusters::desc::{self, ClusterHandler as _};
use rs_matter::dm::clusters::groups::{self, ClusterHandler as _};
use rs_matter::dm::clusters::identify::{self, IdentifyHandler};
use rs_matter::dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT, TEST_PID, TEST_VID};
use rs_matter::dm::devices::DEV_TYPE_FAN;
use rs_matter::dm::endpoints;
use rs_matter::dm::networks::eth::EthNetwork;
use rs_matter::dm::networks::SysNetifs;
use rs_matter::dm::{Async, Cluster, DataModel, Dataver, DeviceType, Endpoint, Node};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::im::{EthInteractionModelState, InteractionModel};
use rs_matter::pairing::qr::{
    no_optional_data, CommFlowType, NoOptionalData, QrPayload, QrTextType,
};
use rs_matter::pairing::DiscoveryCapabilities;
use rs_matter::persist::DirKvBlobStore;
use rs_matter::respond::DefaultResponder;
use rs_matter::sc::pase::MAX_COMM_WINDOW_TIMEOUT_SECS;
use rs_matter::sc::pase::{Spake2pVerifierPassword, Spake2pVerifierPasswordRef};
use rs_matter::transport::exchange::MatterBuffers;
use rs_matter::utils::init::InitMaybeUninit;
use rs_matter::utils::select::Coalesce;
use rs_matter::{clusters, devices, root_endpoint, with, BasicCommData, Matter, MATTER_PORT};

use crate::hub::{read_json, write_json, Hub, Pairing};
use crate::iqair::Control;

const EP: u16 = 1;

const KV_ON_OFF: u16 = rs_matter::persist::VENDOR_KEYS_START + 0x10;
const KV_FAN_CONTROL: u16 = rs_matter::persist::VENDOR_KEYS_START + 0x20;

/// The Atem X has 8 manual speeds (`remote.maxSpeedLevel`).
const SPEED_MAX: u8 = 8;

/// Air Purifier (`0x002D`), Matter 1.2+ Device Library.
const DEV_TYPE_AIR_PURIFIER: DeviceType = DeviceType {
    dtype: 0x002D,
    drev: 1,
};

static MATTER: static_cell::StaticCell<Matter> = static_cell::StaticCell::new();
static BUFFERS: static_cell::StaticCell<MatterBuffers> = static_cell::StaticCell::new();
static STATE: static_cell::StaticCell<EthInteractionModelState> = static_cell::StaticCell::new();

pub struct MatterConfig {
    pub data_dir: PathBuf,
    /// Expose as an Air Purifier (true) or a plain Fan (false).
    pub purifier: bool,
    pub port: Option<u16>,
    pub interface: Option<String>,
}

pub struct MatterChannels {
    pub fan: async_channel::Receiver<()>,
    pub power: async_channel::Receiver<()>,
    pub open_pairing: async_channel::Receiver<()>,
}

/// Per-install identity: pairing passcode, discriminator, mDNS hostname, serial.
#[derive(Serialize, Deserialize)]
struct Identity {
    passcode: u32,
    discriminator: u16,
    hostname: String,
    serial: String,
}

impl Identity {
    fn load_or_create(path: &std::path::Path) -> Self {
        if let Some(id) = read_json::<Identity>(path) {
            return id;
        }

        let mut rng = rand::rng();
        let passcode = loop {
            let p = rng.random_range(1..=99_999_998u32);
            if is_valid_passcode(p) {
                break p;
            }
        };
        let hex: String = (0..6)
            .map(|_| format!("{:02X}", rng.random::<u8>()))
            .collect();

        let id = Identity {
            passcode,
            discriminator: rng.random_range(0..=0xFFFu16),
            serial: format!("IQM-{}", &hex[..8]),
            hostname: hex,
        };
        write_json(path, &id);
        id
    }
}

/// Matter disallows trivial passcodes.
fn is_valid_passcode(p: u32) -> bool {
    const INVALID: [u32; 12] = [
        0, 11111111, 22222222, 33333333, 44444444, 55555555, 66666666, 77777777, 88888888,
        99999999, 12345678, 87654321,
    ];
    (1..=99_999_998).contains(&p) && !INVALID.contains(&p)
}

pub fn run(hub: Arc<Hub>, ch: MatterChannels, cfg: MatterConfig) -> Result<(), Error> {
    let matter_dir = cfg.data_dir.join("matter");
    std::fs::create_dir_all(&matter_dir).map_err(|_| ErrorCode::StdIoError)?;

    let id = Identity::load_or_create(&cfg.data_dir.join("identity.json"));

    let dev_det: &'static BasicInfoConfig = Box::leak(Box::new(BasicInfoConfig {
        vid: TEST_VID,
        pid: TEST_PID,
        hw_ver: 1,
        hw_ver_str: "1",
        sw_ver: 1,
        sw_ver_str: env!("CARGO_PKG_VERSION"),
        serial_no: Box::leak(id.serial.clone().into_boxed_str()),
        unique_id: Box::leak(id.serial.clone().into_boxed_str()),
        manufacturing_date: "20260926",
        vendor_name: "iqair-matter",
        product_name: "Atem X Bridge",
        device_name: "Atem X",
        device_type: Some(if cfg.purifier {
            DEV_TYPE_AIR_PURIFIER.dtype
        } else {
            DEV_TYPE_FAN.dtype
        }),
        ..BasicInfoConfig::new()
    }));

    let passcode_bytes: &'static [u8; 4] = Box::leak(Box::new(id.passcode.to_le_bytes()));
    let comm = BasicCommData {
        password: Spake2pVerifierPassword::new_from_ref(Spake2pVerifierPasswordRef::new(
            passcode_bytes,
        )),
        discriminator: id.discriminator,
    };

    let matter = MATTER.uninit().init_with(Matter::init(
        dev_det,
        comm.clone(),
        &TEST_DEV_ATT,
        cfg.port.unwrap_or(MATTER_PORT),
    ));

    let kv = matter.kv(DirKvBlobStore::new(matter_dir));
    matter.startup(&kv)?;

    let buffers = BUFFERS.uninit().init_with(MatterBuffers::init());
    let state = STATE.init(EthInteractionModelState::new(EthNetwork::new_default()));

    let crypto = default_crypto(rand::rng(), DAC_PRIVKEY);
    let mut rand = crypto.rand()?;

    let on_off_handler = on_off::OnOffHandler::new_standalone(
        Dataver::new_rand(&mut rand),
        EP,
        KV_ON_OFF,
        PowerLogic::new(hub.clone(), ch.power),
    );
    let fan_handler = fan_control::FanControlHandler::new(
        Dataver::new_rand(&mut rand),
        EP,
        KV_FAN_CONTROL,
        FanLogic::new(hub.clone(), ch.fan),
    );

    let node = if cfg.purifier {
        PURIFIER_NODE
    } else {
        FAN_NODE
    };
    let im = InteractionModel::new(
        matter,
        &crypto,
        buffers,
        data_model(node, rand, &on_off_handler, &fan_handler),
        &kv,
        state,
    );
    futures_lite::future::block_on(im.startup())?;

    let responder = DefaultResponder::new(&im);
    let mut respond = pin!(responder.run::<4, 4>());
    let mut im_job = pin!(im.run());

    let bind = std::net::SocketAddr::from((
        std::net::Ipv6Addr::UNSPECIFIED,
        cfg.port.unwrap_or(MATTER_PORT),
    ));
    let socket = async_io::Async::<UdpSocket>::bind(bind)?;

    let hostname: &'static str = Box::leak(id.hostname.clone().into_boxed_str());
    let mut mdns = pin!(crate::mdns::run(
        matter,
        &crypto,
        hostname,
        cfg.interface.as_deref()
    ));
    let mut transport = pin!(matter.run(&crypto, &socket, &socket, &socket));

    // Pairing info, for the logs and the web UI.
    let mut qr_buf = [0u8; 128];
    let qr_text = QrPayload::new_from_basic_info(
        DiscoveryCapabilities::IP,
        CommFlowType::Standard,
        comm.clone(),
        dev_det,
        no_optional_data as NoOptionalData,
    )
    .as_str(&mut qr_buf)?
    .0
    .to_string();
    let manual_code = comm.compute_pretty_pairing_code().to_string();

    if !matter.has_fabrics() {
        info!("Not paired yet. Pairing code: {manual_code}  QR: {qr_text}");
        matter.print_standard_qr_code(QrTextType::Unicode, DiscoveryCapabilities::IP)?;
        matter.open_basic_comm_window(MAX_COMM_WINDOW_TIMEOUT_SECS, &crypto, &())?;
    } else {
        info!("Already paired; open a pairing window from the web UI to add another controller");
    }

    // Keep the web UI's pairing status fresh, and open windows on request.
    let mut pairing = pin!(async {
        loop {
            let window_open = matter.comm_window_state().is_open();
            hub.update_pairing(Pairing {
                qr_text: qr_text.clone(),
                manual_code: manual_code.clone(),
                fabrics: matter.with_state(|s| s.fabrics.iter().count()),
                window_open,
            });

            let tick = async_io::Timer::after(Duration::from_secs(2));
            if let Either::First(Ok(())) = select(ch.open_pairing.recv(), tick).await {
                if !matter.comm_window_state().is_open() {
                    info!("Opening a {MAX_COMM_WINDOW_TIMEOUT_SECS}s pairing window");
                    if let Err(e) =
                        matter.open_basic_comm_window(MAX_COMM_WINDOW_TIMEOUT_SECS, &crypto, &())
                    {
                        warn!("couldn't open pairing window: {e:?}");
                    }
                }
            }
        }
    });

    let all = select3(
        &mut transport,
        &mut mdns,
        select3(&mut respond, &mut im_job, &mut pairing).coalesce(),
    );

    futures_lite::future::block_on(all.coalesce())
}

const FAN_CLUSTERS: &[Cluster<'static>] = clusters!(
    desc::DescHandler::CLUSTER,
    identify::CLUSTER,
    groups::GroupsHandler::CLUSTER,
    PowerLogic::CLUSTER,
    FanLogic::CLUSTER,
);

const PURIFIER_NODE: Node<'static> = Node {
    endpoints: &[
        root_endpoint!(eth),
        Endpoint::new(EP, devices!(DEV_TYPE_AIR_PURIFIER), FAN_CLUSTERS),
    ],
};

const FAN_NODE: Node<'static> = Node {
    endpoints: &[
        root_endpoint!(eth),
        Endpoint::new(EP, devices!(DEV_TYPE_FAN), FAN_CLUSTERS),
    ],
};

fn data_model<'a, OH: OnOffHooks, FH: FanControlHooks>(
    node: Node<'static>,
    mut rand: impl rand::Rng + Copy,
    on_off: &'a on_off::OnOffHandler<'a, OH, on_off::NoLevelControl>,
    fan: &'a fan_control::FanControlHandler<FH>,
) -> impl DataModel + 'a {
    (
        node,
        endpoints::EthSysHandlerBuilder::new()
            .netif_diag(&SysNetifs)
            .build(rand)
            .chain(
                |e, c| e == EP && c == desc::DescHandler::CLUSTER.id,
                Async(desc::DescHandler::new(Dataver::new_rand(&mut rand)).adapt()),
            )
            .chain(
                |e, c| e == EP && c == identify::CLUSTER.id,
                Async(IdentifyHandler::new(Dataver::new_rand(&mut rand)).adapt()),
            )
            .chain(
                |e, c| e == EP && c == groups::GroupsHandler::CLUSTER.id,
                Async(groups::GroupsHandler::new(Dataver::new_rand(&mut rand)).adapt()),
            )
            .chain(
                |e, c| e == EP && c == PowerLogic::CLUSTER.id,
                Async(on_off::HandlerAdaptor(on_off)),
            )
            .chain(
                |e, c| e == EP && c == FanLogic::CLUSTER.id,
                Async(fan_control::HandlerAdaptor(fan)),
            ),
    )
}

/// Fan Control ↔ purifier speed / auto / standby.
///
/// Power off is reported as `FanMode` Off (not just a zero current speed), so ecosystems
/// that only look at Fan Control still see the purifier as off.
struct FanLogic {
    hub: Arc<Hub>,
    changes: async_channel::Receiver<()>,
    /// False until the first real device state arrives. rs-matter replays the persisted
    /// setting through `set_fan` at startup; ignoring it means a restart never drives
    /// the purifier.
    synced: AtomicBool,
    last_power: AtomicBool,
    last_auto: AtomicBool,
    last_speed: AtomicU8,
}

impl FanLogic {
    fn new(hub: Arc<Hub>, changes: async_channel::Receiver<()>) -> Self {
        Self {
            hub,
            changes,
            synced: AtomicBool::new(false),
            last_power: AtomicBool::new(false),
            last_auto: AtomicBool::new(false),
            last_speed: AtomicU8::new(0),
        }
    }
}

impl FanControlHooks for FanLogic {
    const CLUSTER: Cluster<'static> = fan_control_cluster::FULL_CLUSTER
        .with_revision(6)
        .with_features(Feature::MULTI_SPEED.bits() | Feature::AUTO.bits() | Feature::STEP.bits())
        .with_attrs(with!(
            required;
            fan_control_cluster::AttributeId::SpeedMax
                | fan_control_cluster::AttributeId::SpeedSetting
                | fan_control_cluster::AttributeId::SpeedCurrent
        ))
        .with_cmds(with!(fan_control_cluster::CommandId::Step));

    const FAN_MODE_SEQUENCE: FanModeSequenceEnum = FanModeSequenceEnum::OffLowMedHighAuto;
    const SPEED_MAX: u8 = SPEED_MAX;

    fn set_fan(&self, setting: FanSetting) -> Result<(), ()> {
        if !self.synced.load(Ordering::Relaxed) {
            return Ok(());
        }

        let powered = self.hub.device().is_some_and(|d| d.power);
        info!("Matter: fan {setting:?}");

        match setting {
            FanSetting::Off => self.hub.send(Control::Power(false)),
            FanSetting::Auto => {
                if !powered {
                    self.hub.send(Control::Power(true));
                }
                self.hub.send(Control::Auto(true));
            }
            FanSetting::Manual { speed, .. } => {
                if !powered {
                    self.hub.send(Control::Power(true));
                }
                self.hub.send(Control::Speed(speed.clamp(1, SPEED_MAX)));
            }
        }

        Ok(())
    }

    fn current_speed(&self) -> CurrentSpeed {
        match self.hub.device() {
            Some(d) if d.power => CurrentSpeed::Speed(d.fan_speed.clamp(1, SPEED_MAX)),
            _ => CurrentSpeed::Speed(0),
        }
    }

    async fn run<F: Fn(OutOfBandMessage)>(&self, notify: F) {
        loop {
            if self.changes.recv().await.is_err() {
                return core::future::pending().await;
            }
            let Some(d) = self.hub.device() else { continue };

            let first = !self.synced.swap(true, Ordering::Relaxed);
            let was_on = self.last_power.swap(d.power, Ordering::Relaxed);
            let resync = first || was_on != d.power;

            if !d.power {
                if resync {
                    notify(OutOfBandMessage::FanMode(FanModeEnum::Off));
                }
            } else if d.auto_mode {
                if resync || !self.last_auto.swap(true, Ordering::Relaxed) {
                    notify(OutOfBandMessage::FanMode(FanModeEnum::Auto));
                }
            } else {
                let speed = d.manual_speed().clamp(1, SPEED_MAX);
                let was_auto = self.last_auto.swap(false, Ordering::Relaxed);
                let old = self.last_speed.swap(speed, Ordering::Relaxed);
                if resync || was_auto || old != speed {
                    notify(OutOfBandMessage::SpeedSetting(speed));
                }
            }

            notify(OutOfBandMessage::CurrentSpeed);
        }
    }
}

/// On/Off ↔ purifier power (off = standby, like the unit's own button).
struct PowerLogic {
    hub: Arc<Hub>,
    changes: async_channel::Receiver<()>,
    synced: AtomicBool,
}

impl PowerLogic {
    fn new(hub: Arc<Hub>, changes: async_channel::Receiver<()>) -> Self {
        Self {
            hub,
            changes,
            synced: AtomicBool::new(false),
        }
    }
}

impl OnOffHooks for PowerLogic {
    const CLUSTER: Cluster<'static> = on_off_cluster::FULL_CLUSTER
        .with_attrs(with!(required))
        .with_cmds(with!(
            on_off_cluster::CommandId::Off
                | on_off_cluster::CommandId::On
                | on_off_cluster::CommandId::Toggle
        ));

    fn set_on_off(&self, on: bool) {
        // Ignore the startup replay of the persisted value; see `FanLogic::synced`.
        if self.synced.load(Ordering::Relaxed) {
            info!("Matter: power {}", if on { "on" } else { "off" });
            self.hub.send(Control::Power(on));
        }
    }

    async fn handle_off_with_effect(&self, _effect: on_off::EffectVariantEnum) {
        self.set_on_off(false);
    }

    async fn run<F: Fn(on_off::OutOfBandMessage)>(&self, notify: F) {
        loop {
            if self.changes.recv().await.is_err() {
                return core::future::pending().await;
            }
            if let Some(d) = self.hub.device() {
                self.synced.store(true, Ordering::Relaxed);
                notify(on_off::OutOfBandMessage::Update(d.power));
            }
        }
    }
}
