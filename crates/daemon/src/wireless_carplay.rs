#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use std::{
    fmt,
    io::{self, Read, Write},
    net::IpAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use platform_superbird::{AmbientLightState, SuperbirdIdentitySource};
use prelude_control::{ImplementationStatus, ProjectionCapabilities};
use prelude_iap2::{
    CarkitIo, WiredCarkitSession, WirelessCarPlayStartRequest,
    csm::wifi::{AccessoryWifiConfiguration, WifiSecurityType},
};
use prelude_wireless::bluez::{PhoneKind, RfcommConnection};

#[cfg(target_os = "linux")]
use crate::airplay_data_channel::Iap2Tunnel;
use crate::{
    AirPlayConfig, AudioRuntimeSnapshot, DaemonConfig, Error, HealthState, Iap2Config, MediaConfig,
    PlatformConfig, ProjectionDevice, ProjectionRuntime, Result, RuntimeProbe, WirelessLease,
    WirelessService,
    wireless_runtime::{
        AccessPointInfo, SharedAccessPoint, SharedBluetoothAddress, SharedBluetoothControl,
        remember_phone, wait_for_access_point, wireless_state_dir,
    },
};
#[cfg(all(target_os = "linux", feature = "linux-media"))]
use crate::{
    ConnectionStep, DisplayPlatform, DisplayStatus, persist_last_display_platform,
    wired_runtime::apply_carplay_display_status,
};
#[cfg(target_os = "linux")]
use crate::{
    airplay_runtime::{AirPlayProgress, AirPlayRuntime, AirPlayStage},
    shared_mfi::SharedMfi,
    wired_runtime::{ComponentProbe, airplay_probe, projection_probe},
};

const ACCESS_POINT_WAIT: Duration = Duration::from_secs(20);
const LINK_LOCAL_WAIT: Duration = Duration::from_secs(10);
const LINK_SERVICE_INTERVAL: Duration = Duration::from_millis(250);
const WORKER_STOP_WAIT: Duration = Duration::from_secs(35);

#[derive(Debug)]
struct RfcommIo(RfcommConnection);

impl Read for RfcommIo {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}

impl Write for RfcommIo {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl CarkitIo for RfcommIo {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_read_timeout(timeout)
    }

    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.0.set_write_timeout(timeout)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    WaitingForAccessPoint,
    OpeningMfi,
    BindingAirplay,
    Setup,
    Availability,
    Start,
    Active,
}

impl Stage {
    const fn detail(self) -> &'static str {
        match self {
            Self::WaitingForAccessPoint => {
                "waiting for the Wi-Fi access point and its link-local address"
            }
            Self::OpeningMfi => "opening the MFi authentication coprocessor",
            Self::BindingAirplay => "binding the AirPlay receiver to the access point interface",
            Self::Setup => "authenticating and identifying over iAP2 on Bluetooth",
            Self::Availability => "waiting for the iPhone to offer wireless CarPlay",
            Self::Start => "sending the wireless CarPlay start message",
            Self::Active => "wireless CarPlay start was sent; the iAP2 link is serviced",
        }
    }
}

#[derive(Debug, Clone)]
struct Progress {
    stage: Stage,
    #[cfg(target_os = "linux")]
    airplay: Option<(AirPlayProgress, bool)>,
    audio_output_missing: bool,
    audio: AudioRuntimeSnapshot,
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
impl Progress {
    fn display_status(&self) -> DisplayStatus {
        let step = match self.stage {
            Stage::WaitingForAccessPoint => ConnectionStep::StartingWifi,
            Stage::OpeningMfi => ConnectionStep::PreparingAuthentication,
            Stage::BindingAirplay => ConnectionStep::StartingReceiver,
            Stage::Setup => ConnectionStep::Authenticating,
            Stage::Availability => ConnectionStep::WaitingForCarPlay,
            Stage::Start => ConnectionStep::StartingCarPlay,
            Stage::Active => {
                return self
                    .airplay
                    .map_or(AirPlayStage::Listening, |(airplay, _)| airplay.stage)
                    .display_status(ConnectionStep::WaitingForIphoneOnWifi);
            }
        };
        DisplayStatus::Connecting(Some(step))
    }
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            stage: Stage::WaitingForAccessPoint,
            #[cfg(target_os = "linux")]
            airplay: None,
            audio_output_missing: false,
            audio: AudioRuntimeSnapshot::disabled(),
        }
    }
}

type SharedProgress = Arc<Mutex<Progress>>;

#[derive(Debug)]
enum Connection {
    Idle,
    Worker {
        handle: JoinHandle<std::result::Result<(), String>>,
        progress: SharedProgress,
        cancel: Arc<AtomicBool>,
    },
}

#[derive(Debug, Clone)]
struct WirelessLinks {
    access_point: SharedAccessPoint,
    bluetooth_address: SharedBluetoothAddress,
    bluetooth_control: SharedBluetoothControl,
}

pub struct WirelessCarPlayRuntime {
    iap2: Iap2Config,
    airplay: AirPlayConfig,
    media: MediaConfig,
    platform: PlatformConfig,
    ambient_light: AmbientLightState,
    state_dir: std::path::PathBuf,
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    display: Option<prelude_media::LinuxDisplayHandle>,
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    display_status: Option<DisplayStatus>,
    links: WirelessLinks,
    identity_source_failure: Option<String>,
    connection: Connection,
    phone_present: bool,
    last_failure: Option<String>,
}

impl fmt::Debug for WirelessCarPlayRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WirelessCarPlayRuntime")
            .field("connection", &self.connection)
            .field("has_failure", &self.last_failure.is_some())
            .finish_non_exhaustive()
    }
}

impl WirelessCarPlayRuntime {
    pub fn new(
        config: &DaemonConfig,
        wireless: &WirelessService,
        #[cfg(all(target_os = "linux", feature = "linux-media"))] display: Option<
            prelude_media::LinuxDisplayHandle,
        >,
        ambient_light: AmbientLightState,
    ) -> Self {
        let mut iap2 = config.iap2.clone();
        let identity_source_failure = if iap2.needs_platform_identity() {
            let source = SuperbirdIdentitySource::new(env!("CARGO_PKG_VERSION"));
            iap2.resolve_missing_identity(&source)
                .err()
                .map(|error| format!("platform accessory identity is unavailable: {error}"))
        } else {
            None
        };
        Self {
            iap2,
            airplay: config.airplay.clone(),
            media: config.media.clone(),
            platform: config.platform.clone(),
            ambient_light,
            state_dir: config.state_dir.clone(),
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            display,
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            display_status: None,
            links: WirelessLinks {
                access_point: wireless.shared_access_point(),
                bluetooth_address: wireless.shared_bluetooth_address(),
                bluetooth_control: wireless.shared_bluetooth_control(),
            },
            identity_source_failure,
            connection: Connection::Idle,
            phone_present: false,
            last_failure: None,
        }
    }

    fn refresh(&mut self) {
        let finished = matches!(
            &self.connection,
            Connection::Worker { handle, .. } if handle.is_finished()
        );
        if !finished {
            return;
        }
        let Connection::Worker { handle, .. } =
            std::mem::replace(&mut self.connection, Connection::Idle)
        else {
            return;
        };
        self.last_failure = match handle.join() {
            Ok(Ok(())) => None,
            Ok(Err(error)) => {
                tracing::warn!(%error, "wireless CarPlay session ended");
                Some(error)
            }
            Err(_) => Some("wireless CarPlay worker panicked".to_owned()),
        };
    }

    fn progress(&self) -> Option<Progress> {
        match &self.connection {
            Connection::Idle => None,
            Connection::Worker { progress, .. } => {
                progress.lock().ok().map(|progress| progress.clone())
            }
        }
    }

    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    fn update_display(&mut self) {
        let status = self.progress().filter(|_| self.phone_present).map_or(
            DisplayStatus::WaitingForPhone,
            |progress| {
                if progress.audio_output_missing {
                    DisplayStatus::MissingAudioOutput
                } else {
                    progress.display_status()
                }
            },
        );
        let _ =
            apply_carplay_display_status(self.display.as_ref(), &mut self.display_status, status);
    }

    fn idle_probe(&self) -> RuntimeProbe {
        let (state, detail) = match (&self.identity_source_failure, &self.last_failure) {
            (Some(failure), _) => (HealthState::Failed, failure.clone()),
            (None, Some(failure)) => (HealthState::NotReady, failure.clone()),
            (None, None) => (
                HealthState::NotReady,
                "waiting for an iPhone to connect over Bluetooth".to_owned(),
            ),
        };
        RuntimeProbe {
            state,
            detail: detail.clone(),
            transport_state: HealthState::NotReady,
            transport_detail: "no iAP2 Bluetooth channel is open".to_owned(),
            link_state: if self.access_point().is_some() {
                HealthState::Ready
            } else {
                HealthState::NotReady
            },
            link_detail: self.access_point().map_or_else(
                || "the wireless access point is not up".to_owned(),
                |info| {
                    format!(
                        "access point {} is up on channel {}",
                        info.ssid, info.channel
                    )
                },
            ),
            control_state: HealthState::NotReady,
            control_detail: "the AirPlay receiver starts with a wireless session".to_owned(),
            authentication_state: HealthState::NotReady,
            authentication_detail: detail,
            session_state: HealthState::NotReady,
            session_detail: "no wireless CarPlay session".to_owned(),
            implementation: ImplementationStatus::ExperimentalPrivateProtocol,
            capabilities: ProjectionCapabilities::default(),
        }
    }

    fn access_point(&self) -> Option<AccessPointInfo> {
        self.links.access_point.read().ok()?.clone()
    }

    #[cfg(target_os = "linux")]
    fn worker_probe(progress: &Progress) -> RuntimeProbe {
        let Some((airplay, alive)) = progress.airplay.filter(|_| progress.stage == Stage::Active)
        else {
            let detail = progress.stage.detail().to_owned();
            return RuntimeProbe {
                state: HealthState::Starting,
                detail: detail.clone(),
                transport_state: HealthState::Ready,
                transport_detail: "iAP2 Bluetooth channel is open".to_owned(),
                link_state: HealthState::Starting,
                link_detail: detail.clone(),
                control_state: HealthState::Starting,
                control_detail: detail.clone(),
                authentication_state: HealthState::Starting,
                authentication_detail: detail.clone(),
                session_state: HealthState::Starting,
                session_detail: detail,
                implementation: ImplementationStatus::ExperimentalPrivateProtocol,
                capabilities: ProjectionCapabilities::default(),
            };
        };
        let (airplay_state, airplay_detail) = airplay_probe(airplay, alive);
        let mut probe = projection_probe(
            ComponentProbe::new(
                HealthState::Ready,
                "authenticated iAP2 link over Bluetooth RFCOMM is serviced".to_owned(),
            ),
            ComponentProbe::new(
                HealthState::Ready,
                "the iPhone was handed the access point and receiver address".to_owned(),
            ),
            ComponentProbe::new(airplay_state, airplay_detail),
            ComponentProbe::new(
                HealthState::Ready,
                "MFi-authenticated wireless iAP2 session is active".to_owned(),
            ),
            ComponentProbe::new(
                HealthState::Ready,
                "validated wireless CarPlay start message was sent".to_owned(),
            ),
            ProjectionCapabilities {
                control: matches!(
                    airplay.stage,
                    AirPlayStage::MfiAuthenticated
                        | AirPlayStage::SessionConfigured
                        | AirPlayStage::StreamsConfigured
                        | AirPlayStage::Recording
                ),
                video: airplay.media_ready && airplay.media_recording,
                audio: airplay.audio_ready && airplay.media_recording,
                microphone: airplay.microphone_ready && airplay.media_recording,
                input: airplay.input_ready && airplay.event_channel_ready,
            },
        );
        probe.detail = if probe.state == HealthState::Ready {
            "wireless CarPlay screen, input, and configured media are live".to_owned()
        } else {
            "wireless CarPlay is starting over the access point".to_owned()
        };
        probe
    }
}

impl ProjectionRuntime for WirelessCarPlayRuntime {
    fn probe(&mut self) -> RuntimeProbe {
        self.refresh();
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        self.update_display();
        match self.progress() {
            None => self.idle_probe(),
            #[cfg(target_os = "linux")]
            Some(progress) => Self::worker_probe(&progress),
            #[cfg(not(target_os = "linux"))]
            Some(_) => self.idle_probe(),
        }
    }

    fn set_phone_presence(&mut self, present: bool) {
        self.phone_present = present;
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        self.update_display();
    }

    fn audio_status(&mut self) -> AudioRuntimeSnapshot {
        self.progress()
            .map_or_else(AudioRuntimeSnapshot::disabled, |progress| progress.audio)
    }

    fn start(&mut self, device: &ProjectionDevice) -> Result<()> {
        self.refresh();
        let ProjectionDevice::WirelessCarPlay(phone) = device else {
            return Err(Error::Unsupported(
                "wireless CarPlay runtime only starts iPhones connected over Bluetooth",
            ));
        };
        if !matches!(self.connection, Connection::Idle) {
            return Ok(());
        }
        if let Some(failure) = &self.identity_source_failure {
            return Err(Error::Runtime(failure.clone()));
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = phone;
            Err(Error::NotReady {
                component: "wireless CarPlay runtime",
                state: "linux_required",
            })
        }
        #[cfg(target_os = "linux")]
        {
            let (connection, lease) = phone.claim().ok_or(Error::NotReady {
                component: "wireless CarPlay phone",
                state: "connection_already_claimed",
            })?;
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            persist_last_display_platform(&self.state_dir, DisplayPlatform::CarPlay).map_err(
                |error| {
                    Error::Runtime(format!(
                        "persisting the CarPlay display platform failed: {error}"
                    ))
                },
            )?;
            let progress = SharedProgress::default();
            let cancel = Arc::new(AtomicBool::new(false));
            let worker = Worker {
                iap2: self.iap2.clone(),
                airplay: self.airplay.clone(),
                media: self.media.clone(),
                platform: self.platform.clone(),
                ambient_light: self.ambient_light.clone(),
                state_dir: self.state_dir.clone(),
                #[cfg(all(target_os = "linux", feature = "linux-media"))]
                display: self.display.clone(),
                links: self.links.clone(),
                progress: Arc::clone(&progress),
                cancel: Arc::clone(&cancel),
            };
            let handle = std::thread::Builder::new()
                .name("ct-wireless-carplay".to_owned())
                .spawn(move || worker.run(connection, lease))
                .map_err(|error| {
                    Error::Runtime(format!(
                        "failed to start the wireless CarPlay worker: {error}"
                    ))
                })?;
            self.last_failure = None;
            self.connection = Connection::Worker {
                handle,
                progress,
                cancel,
            };
            Ok(())
        }
    }

    fn stop(&mut self) -> Result<()> {
        if let Connection::Worker { handle, cancel, .. } =
            std::mem::replace(&mut self.connection, Connection::Idle)
        {
            cancel.store(true, Ordering::Relaxed);
            let deadline = Instant::now() + WORKER_STOP_WAIT;
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !handle.is_finished() {
                return Err(Error::Runtime(
                    "wireless CarPlay worker did not stop in time".to_owned(),
                ));
            }
            handle
                .join()
                .map_err(|_| Error::Runtime("wireless CarPlay worker panicked".to_owned()))?
                .map_err(Error::Runtime)?;
        }
        self.last_failure = None;
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        {
            self.phone_present = false;
            self.update_display();
        }
        Ok(())
    }
}

struct Worker {
    iap2: Iap2Config,
    airplay: AirPlayConfig,
    media: MediaConfig,
    platform: PlatformConfig,
    ambient_light: AmbientLightState,
    state_dir: std::path::PathBuf,
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    display: Option<prelude_media::LinuxDisplayHandle>,
    links: WirelessLinks,
    progress: SharedProgress,
    cancel: Arc<AtomicBool>,
}

#[cfg(target_os = "linux")]
impl Worker {
    fn set_stage(&self, stage: Stage) {
        if let Ok(mut progress) = self.progress.lock() {
            progress.stage = stage;
        }
    }

    fn run(
        self,
        connection: RfcommConnection,
        lease: WirelessLease,
    ) -> std::result::Result<(), String> {
        let result = self.run_session(connection);
        drop(lease);
        result
    }

    fn run_session(&self, connection: RfcommConnection) -> std::result::Result<(), String> {
        self.set_stage(Stage::WaitingForAccessPoint);
        let access_point = self.access_point_with_link_local()?;
        let link_local = access_point
            .ipv6_link_local
            .ok_or("the access point has no IPv6 link-local address")?;
        let bluetooth_address = self
            .links
            .bluetooth_address
            .read()
            .ok()
            .and_then(|address| *address)
            .ok_or("the Bluetooth adapter address is unknown")?;
        let identification = self
            .iap2
            .wireless_identification(bluetooth_address, &access_point.ssid)
            .map_err(|gate| gate.detail.to_owned())?;

        self.set_stage(Stage::OpeningMfi);
        let mfi = SharedMfi::open_default().map_err(|error| {
            format!("MFi Authentication 3.0 coprocessor is unavailable or invalid: {error}")
        })?;
        if self.cancel.load(Ordering::Relaxed) {
            return Ok(());
        }

        self.set_stage(Stage::BindingAirplay);
        let airplay = AirPlayRuntime::bind(
            &self.airplay,
            &self.media,
            &self.platform,
            &self.iap2,
            &self.state_dir,
            link_local,
            &access_point.interface,
            parse_mac(&access_point.bssid).ok_or("the access point MAC is malformed")?,
            mfi.clone(),
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            self.display.clone(),
            self.ambient_light.clone(),
        )
        .map_err(|error| {
            format!("the AirPlay receiver on the access point is not ready: {error}")
        })?;
        if let Ok(mut progress) = self.progress.lock() {
            progress.audio_output_missing = airplay.audio_output_missing();
        }

        let result = self.drive(
            connection,
            mfi,
            identification,
            &access_point,
            link_local,
            &airplay,
        );
        let stopped = airplay.stop();
        result.and(stopped)
    }

    fn access_point_with_link_local(&self) -> std::result::Result<AccessPointInfo, String> {
        wait_for_access_point(&self.links.access_point, &self.cancel, ACCESS_POINT_WAIT)
            .ok_or("the access point is not up; is the USB Wi-Fi adapter connected?")?;
        let deadline = Instant::now() + LINK_LOCAL_WAIT;
        loop {
            let info = self
                .links
                .access_point
                .read()
                .ok()
                .and_then(|info| info.clone())
                .ok_or("the access point went down")?;
            if info.ipv6_link_local.is_some() {
                return Ok(info);
            }
            if Instant::now() >= deadline || self.cancel.load(Ordering::Relaxed) {
                return Err("the access point interface has no IPv6 link-local address".to_owned());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn drive(
        &self,
        connection: RfcommConnection,
        mfi: SharedMfi,
        identification: prelude_iap2::csm::identification::IdentificationInformation,
        access_point: &AccessPointInfo,
        link_local: std::net::Ipv6Addr,
        airplay: &AirPlayRuntime,
    ) -> std::result::Result<(), String> {
        let wifi = AccessoryWifiConfiguration {
            ssid: access_point.ssid.clone(),
            passphrase: access_point.passphrase.clone(),
            security_type: WifiSecurityType::WpaWpa2,
            channel: access_point.channel,
        };
        let peer = connection.peer.clone();
        let tunnel_parts = (mfi.clone(), identification.clone(), wifi.clone());
        let mut session =
            WiredCarkitSession::new_wireless(RfcommIo(connection), mfi, identification, wifi)
                .map_err(|error| format!("wireless iAP2 session construction failed: {error}"))?;

        self.set_stage(Stage::Setup);
        session
            .run_setup()
            .map_err(|error| format!("wireless iAP2 setup failed: {error}"))?;
        self.set_stage(Stage::Availability);
        session
            .wait_for_wireless_carplay()
            .map_err(|error| format!("the iPhone did not offer wireless CarPlay: {error}"))?;

        self.set_stage(Stage::Start);
        let metadata = airplay.start_metadata();
        session
            .start_wireless_carplay(&WirelessCarPlayStartRequest {
                ssid: access_point.ssid.clone(),
                passphrase: access_point.passphrase.clone(),
                channel: access_point.channel,
                security_type: WifiSecurityType::WpaWpa2,
                ip_addresses: vec![IpAddr::V6(link_local)],
                port: metadata.service_port,
                device_identifier: metadata.device_identifier.clone(),
                public_key: Some(metadata.public_key.clone()),
                source_version: metadata.source_version.clone(),
            })
            .map_err(|error| format!("sending the wireless CarPlay start failed: {error}"))?;
        tracing::info!(
            ssid = access_point.ssid,
            channel = access_point.channel,
            port = metadata.service_port,
            "wireless CarPlay start sent"
        );
        self.set_stage(Stage::Active);
        if let Err(error) = remember_phone(
            &wireless_state_dir(&self.state_dir),
            &self.links.bluetooth_control,
            PhoneKind::CarPlay,
            &peer,
        ) {
            tracing::warn!(peer, %error, "remembering the CarPlay iPhone failed");
        }

        let mut bluetooth = Some(session);
        let mut tunnel: Option<TunnelSession> = None;
        while !self.cancel.load(Ordering::Relaxed) {
            if tunnel.is_none() {
                if let Some(opened) = airplay.take_iap2_tunnel() {
                    let (mfi, identification, wifi) = tunnel_parts.clone();
                    tunnel = Some(TunnelSession::start(opened, mfi, identification, wifi)?);
                }
            }
            let snapshot = airplay.snapshot();
            let alive = airplay.is_alive();
            if let Ok(mut progress) = self.progress.lock() {
                progress.airplay = Some((snapshot, alive));
                progress.audio = airplay.audio_status();
                progress.audio_output_missing = airplay.audio_output_missing();
            }
            if !alive {
                return Err(airplay
                    .failure_detail()
                    .unwrap_or_else(|| "the AirPlay receiver stopped".to_owned()));
            }
            if snapshot.bluetooth_handoff_requested
                && bluetooth.is_some()
                && tunnel.as_ref().is_some_and(TunnelSession::is_identified)
            {
                drop(bluetooth.take());
                if let Some(control) = self
                    .links
                    .bluetooth_control
                    .read()
                    .ok()
                    .and_then(|control| control.clone())
                {
                    control.disconnect(&peer);
                }
                tracing::info!(peer, "wireless CarPlay moved to Wi-Fi; Bluetooth released");
            }
            match bluetooth.as_mut() {
                Some(session) => {
                    session
                        .service_link(LINK_SERVICE_INTERVAL)
                        .map_err(|error| format!("the Bluetooth iAP2 link was lost: {error}"))?;
                }
                None if tunnel.as_ref().is_some_and(TunnelSession::is_finished) => {
                    return Err("the Wi-Fi iAP2 tunnel closed".to_owned());
                }
                None => std::thread::sleep(LINK_SERVICE_INTERVAL),
            }
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
struct TunnelSession {
    stop: Arc<AtomicBool>,
    identified: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
impl TunnelSession {
    fn start(
        tunnel: Iap2Tunnel,
        mfi: SharedMfi,
        identification: prelude_iap2::csm::identification::IdentificationInformation,
        wifi: AccessoryWifiConfiguration,
    ) -> std::result::Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let identified = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker_identified = Arc::clone(&identified);
        let worker = std::thread::Builder::new()
            .name("prelude-iap2-tunnel".to_owned())
            .spawn(move || {
                if let Err(error) = run_tunnel(
                    tunnel,
                    mfi,
                    identification,
                    wifi,
                    &worker_stop,
                    &worker_identified,
                ) {
                    tracing::warn!(%error, "wireless iAP2 tunnel ended");
                }
            })
            .map_err(|error| format!("starting the wireless iAP2 tunnel failed: {error}"))?;
        Ok(Self {
            stop,
            identified,
            worker: Some(worker),
        })
    }

    fn is_identified(&self) -> bool {
        self.identified.load(Ordering::Acquire)
    }

    fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

#[cfg(target_os = "linux")]
impl Drop for TunnelSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(target_os = "linux")]
fn run_tunnel(
    tunnel: Iap2Tunnel,
    mfi: SharedMfi,
    identification: prelude_iap2::csm::identification::IdentificationInformation,
    wifi: AccessoryWifiConfiguration,
    stop: &AtomicBool,
    identified: &AtomicBool,
) -> std::result::Result<(), String> {
    let mut session = WiredCarkitSession::new_wireless_tunnel(tunnel, mfi, identification, wifi)
        .map_err(|error| format!("constructing the session failed: {error}"))?;
    session
        .run_setup()
        .map_err(|error| format!("setup failed: {error}"))?;
    tracing::info!("wireless iAP2 tunnel identified");
    identified.store(true, Ordering::Release);
    while !stop.load(Ordering::Relaxed) {
        session
            .service_link(LINK_SERVICE_INTERVAL)
            .map_err(|error| format!("the link was lost: {error}"))?;
    }
    Ok(())
}

fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0_u8; 6];
    let mut octets = text.split(':');
    for byte in &mut mac {
        *byte = u8::from_str_radix(octets.next()?, 16).ok()?;
    }
    octets.next().is_none().then_some(mac)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_point_macs_parse() {
        assert_eq!(
            parse_mac("00:c0:ca:b1:22:3f"),
            Some([0x00, 0xc0, 0xca, 0xb1, 0x22, 0x3f])
        );
        assert_eq!(parse_mac("00:c0:ca"), None);
    }
}
