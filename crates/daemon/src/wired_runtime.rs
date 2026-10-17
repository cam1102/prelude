use std::{
    fmt,
    time::{Duration, Instant},
};

use platform_superbird::{AmbientLightState, SuperbirdIdentitySource};
use prelude_control::{ImplementationStatus, ProjectionCapabilities};
use prelude_usb_carkit::{
    Config6Policy, Config6Timeouts, Error as CarrierError, FirstTimePairingPolicy, WiredCarrier,
    WiredCarrierConfig,
};

#[cfg(target_os = "linux")]
use prelude_iap2::{SessionPhase, WiredCarkitSession};
#[cfg(any(test, target_os = "linux"))]
use prelude_usb_carkit::CancellationToken;
#[cfg(target_os = "linux")]
use prelude_usb_carkit::{IphoneCandidate, NcmCarrierKind, WiredNcm, WiredNcmConfig};
#[cfg(any(test, target_os = "linux"))]
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};
#[cfg(target_os = "linux")]
use std::{
    mem,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
};

#[cfg(target_os = "linux")]
use crate::iap2_runtime::{
    CarkitStreamAdapter, DriverStage, StartDisposition, drive_session_with_observer,
    recover_session_stream,
};
use crate::{
    AirPlayConfig, AudioBackend, AudioFailureKind, AudioPipelineState, AudioRuntimeRoute,
    AudioRuntimeSnapshot, AutomaticProjectionRuntime, DaemonConfig, Error, HealthState,
    ProjectionDevice, ProjectionMode, ProjectionRuntime, Result, RuntimeProbe,
    UnsupportedProjectionRuntime, iap2_runtime::SessionStartPlan,
};
#[cfg(all(target_os = "linux", feature = "linux-media"))]
use crate::{ConnectionStep, DisplayPlatform, DisplayStatus, persist_last_display_platform};
#[cfg(target_os = "linux")]
use crate::{
    airplay_runtime::{AirPlayProgress, AirPlayRuntime, AirPlayStage},
    shared_mfi::SharedMfi,
};

pub const MAX_IAP2_BLOCKING_STAGE: Duration = Duration::from_secs(30);

pub const DEFAULT_MAX_WORKER_STOP_WAIT: Duration = Duration::from_secs(35);

pub fn configured_worker_stop_bound(config: &DaemonConfig) -> Duration {
    let usb = &config.usb;
    let reconnect = Duration::from_millis(usb.configuration_reconnect_timeout_ms);
    let activation = Duration::from_millis(usb.control_transfer_timeout_ms)
        .saturating_add(reconnect.saturating_mul(2));
    let carrier_io =
        Duration::from_millis(usb.mux_handshake_timeout_ms.max(usb.mux_connect_timeout_ms));
    let mfi_exclusive_open = Duration::from_secs(2);
    MAX_IAP2_BLOCKING_STAGE
        .max(activation)
        .max(carrier_io)
        .max(mfi_exclusive_open)
        .saturating_add(reconnect)
}

#[derive(Debug)]
pub enum SelectedRuntime {
    Unsupported(UnsupportedProjectionRuntime),
    Automatic(Box<AutomaticProjectionRuntime>),
    Wired(Box<WiredCarkitRuntime>),
}

impl SelectedRuntime {
    pub fn attach_wireless(
        &mut self,
        config: &DaemonConfig,
        wireless: &crate::WirelessService,
        #[cfg(all(target_os = "linux", feature = "linux-media"))] display: Option<
            prelude_media::LinuxDisplayHandle,
        >,
        ambient_light: AmbientLightState,
    ) {
        if let Self::Automatic(runtime) = self {
            runtime.attach_hands_free(wireless.hands_free());
            runtime.attach_wireless(crate::WirelessCarPlayRuntime::new(
                config,
                wireless,
                #[cfg(all(target_os = "linux", feature = "linux-media"))]
                display,
                ambient_light,
            ));
        }
    }

    pub fn from_config(config: &DaemonConfig) -> Result<Self> {
        Self::from_config_with_display(
            config,
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            None,
        )
    }

    pub fn from_config_with_display(
        config: &DaemonConfig,
        #[cfg(all(target_os = "linux", feature = "linux-media"))] display: Option<
            prelude_media::LinuxDisplayHandle,
        >,
    ) -> Result<Self> {
        Self::from_config_with_display_and_ambient(
            config,
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            display,
            AmbientLightState::default(),
        )
    }

    pub fn from_config_with_display_and_ambient(
        config: &DaemonConfig,
        #[cfg(all(target_os = "linux", feature = "linux-media"))] display: Option<
            prelude_media::LinuxDisplayHandle,
        >,
        ambient_light: AmbientLightState,
    ) -> Result<Self> {
        match config.projection.mode {
            ProjectionMode::Automatic => Ok(Self::Automatic(Box::new(
                AutomaticProjectionRuntime::from_config_with_display_and_ambient(
                    config,
                    #[cfg(all(target_os = "linux", feature = "linux-media"))]
                    display,
                    ambient_light,
                )?,
            ))),
            ProjectionMode::ExperimentalPrivateProtocol => Ok(Self::Wired(Box::new(
                WiredCarkitRuntime::from_config_with_display_and_ambient(
                    config,
                    #[cfg(all(target_os = "linux", feature = "linux-media"))]
                    display,
                    ambient_light,
                )?,
            ))),
            ProjectionMode::Disabled | ProjectionMode::AppleLicensedPlugin => {
                Ok(Self::Unsupported(UnsupportedProjectionRuntime))
            }
        }
    }
}

impl ProjectionRuntime for SelectedRuntime {
    fn probe(&mut self) -> RuntimeProbe {
        match self {
            Self::Unsupported(runtime) => runtime.probe(),
            Self::Automatic(runtime) => runtime.probe(),
            Self::Wired(runtime) => runtime.probe(),
        }
    }

    fn set_phone_presence(&mut self, present: bool) {
        match self {
            Self::Unsupported(runtime) => runtime.set_phone_presence(present),
            Self::Automatic(runtime) => runtime.set_phone_presence(present),
            Self::Wired(runtime) => runtime.set_phone_presence(present),
        }
    }

    fn start(&mut self, device: &ProjectionDevice) -> Result<()> {
        match self {
            Self::Unsupported(runtime) => runtime.start(device),
            Self::Automatic(runtime) => runtime.start(device),
            Self::Wired(runtime) => runtime.start(device),
        }
    }

    fn audio_status(&mut self) -> AudioRuntimeSnapshot {
        match self {
            Self::Unsupported(runtime) => runtime.audio_status(),
            Self::Automatic(runtime) => runtime.audio_status(),
            Self::Wired(runtime) => runtime.audio_status(),
        }
    }

    fn play_audio_test_tone(&mut self) -> Result<()> {
        match self {
            Self::Unsupported(runtime) => runtime.play_audio_test_tone(),
            Self::Automatic(runtime) => runtime.play_audio_test_tone(),
            Self::Wired(runtime) => runtime.play_audio_test_tone(),
        }
    }

    fn stop(&mut self) -> Result<()> {
        match self {
            Self::Unsupported(runtime) => runtime.stop(),
            Self::Automatic(runtime) => runtime.stop(),
            Self::Wired(runtime) => runtime.stop(),
        }
    }
}

pub struct WiredCarkitRuntime {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    carrier: WiredCarrier,
    iap2: crate::Iap2Config,
    airplay: AirPlayConfig,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    media: crate::MediaConfig,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    platform: crate::PlatformConfig,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    ambient_light: AmbientLightState,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    state_dir: std::path::PathBuf,
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    display: Option<prelude_media::LinuxDisplayHandle>,
    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    display_status: Option<DisplayStatus>,
    identity_source_failure: Option<String>,
    connection: RuntimeConnection,
    phone_present: bool,
    reconnect: ReconnectSchedule,
    last_failure: Option<RuntimeFailure>,
}

impl fmt::Debug for WiredCarkitRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WiredCarkitRuntime")
            .field("iap2_enabled", &self.iap2.enable_live_session)
            .field("airplay_enabled", &self.airplay.enable_live_receiver)
            .field(
                "identity_source_failed",
                &self.identity_source_failure.is_some(),
            )
            .field("connection", &self.connection)
            .field("reconnect", &self.reconnect)
            .field("has_failure", &self.last_failure.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
enum RuntimeConnection {
    Idle,
    #[cfg(target_os = "linux")]
    Worker {
        handle: JoinHandle<WorkerOutcome>,
        progress: SharedProgress,
        cancellation: CancellationToken,
    },
    #[cfg(target_os = "linux")]
    Active(Box<ActiveSession>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureComponent {
    Carrier,
    Ncm,
    Airplay,
    Iap2,
    Start,
}

#[derive(Debug)]
struct RuntimeFailure {
    component: FailureComponent,
    detail: String,
}

impl WiredCarkitRuntime {
    pub fn from_config(config: &DaemonConfig) -> Result<Self> {
        Self::from_config_with_display(
            config,
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            None,
        )
    }

    pub fn from_config_with_display(
        config: &DaemonConfig,
        #[cfg(all(target_os = "linux", feature = "linux-media"))] display: Option<
            prelude_media::LinuxDisplayHandle,
        >,
    ) -> Result<Self> {
        Self::from_config_with_display_and_ambient(
            config,
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            display,
            AmbientLightState::default(),
        )
    }

    pub fn from_config_with_display_and_ambient(
        config: &DaemonConfig,
        #[cfg(all(target_os = "linux", feature = "linux-media"))] display: Option<
            prelude_media::LinuxDisplayHandle,
        >,
        ambient_light: AmbientLightState,
    ) -> Result<Self> {
        if !matches!(
            config.projection.mode,
            ProjectionMode::Automatic | ProjectionMode::ExperimentalPrivateProtocol
        ) {
            return Err(Error::Config(
                "wired carkit runtime requires automatic or experimental_private_protocol mode"
                    .to_owned(),
            ));
        }
        let usb = &config.usb;
        let carrier = WiredCarrier::new(WiredCarrierConfig {
            config6_policy: Config6Policy {
                allow_private_configuration: usb.allow_private_apple_configuration,
                allow_configuration_change: usb.allow_configuration_changes,
            },
            config6_timeouts: Config6Timeouts {
                control_transfer: Duration::from_millis(usb.control_transfer_timeout_ms),
                reconnect: Duration::from_millis(usb.configuration_reconnect_timeout_ms),
                poll_interval: Duration::from_millis(usb.configuration_poll_interval_ms),
            },
            mux_handshake_timeout: Duration::from_millis(usb.mux_handshake_timeout_ms),
            mux_io_timeout: Duration::from_millis(usb.mux_io_timeout_ms),
            tcp_connect_timeout: Duration::from_millis(usb.mux_connect_timeout_ms),
            pairing_records_directory: usb.pairing_records_directory.clone(),
            first_time_pairing: FirstTimePairingPolicy {
                allow: usb.allow_first_time_pairing,
                dialog_timeout: Duration::from_millis(usb.pairing_dialog_timeout_ms),
                retry_interval: Duration::from_millis(usb.pairing_retry_interval_ms),
            },
            allow_private_lockdown_service: usb.allow_private_lockdown_service,
            enable_syslog_tap: usb.enable_iphone_syslog_tap,
        })
        .map_err(|error| Error::Runtime(sanitized_carrier_error(&error)))?;
        let mut iap2 = config.iap2.clone();
        let identity_source_failure = if iap2.needs_platform_identity() {
            let source = SuperbirdIdentitySource::new(env!("CARGO_PKG_VERSION"));
            iap2.resolve_missing_identity(&source)
                .err()
                .map(|error| format!("platform accessory identity is unavailable: {error}"))
        } else {
            None
        };
        Ok(Self {
            carrier,
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
            identity_source_failure,
            connection: RuntimeConnection::Idle,
            phone_present: false,
            reconnect: ReconnectSchedule::new(
                Duration::from_millis(usb.reconnect_initial_delay_ms),
                Duration::from_millis(usb.reconnect_maximum_delay_ms),
            ),
            last_failure: None,
        })
    }

    #[cfg(target_os = "linux")]
    fn refresh(&mut self) {
        let finished = matches!(
            &self.connection,
            RuntimeConnection::Worker { handle, .. } if handle.is_finished()
        );
        if finished {
            let RuntimeConnection::Worker { handle, .. } =
                mem::replace(&mut self.connection, RuntimeConnection::Idle)
            else {
                unreachable!("worker state checked above")
            };
            match handle.join() {
                Ok(WorkerOutcome::Active(session)) => {
                    self.last_failure = None;
                    self.reconnect.record_success();
                    clear_last_wired_failure(&self.state_dir);
                    self.connection = RuntimeConnection::Active(session);
                }
                Ok(WorkerOutcome::Failed(failure)) => {
                    self.record_failure(failure);
                }
                Err(_) => {
                    self.record_failure(RuntimeFailure {
                        component: FailureComponent::Iap2,
                        detail: "wired session worker terminated unexpectedly".to_owned(),
                    });
                }
            }
        }

        let disconnected_failure = match &mut self.connection {
            RuntimeConnection::Active(session) => {
                if !session.carrier_alive() {
                    Some(RuntimeFailure {
                        component: FailureComponent::Carrier,
                        detail: "the carkit byte stream disconnected".to_owned(),
                    })
                } else if !session.ncm_alive() {
                    Some(RuntimeFailure {
                        component: FailureComponent::Ncm,
                        detail: "the topology-associated NCM carrier stopped".to_owned(),
                    })
                } else if !session.airplay_alive() {
                    Some(RuntimeFailure {
                        component: FailureComponent::Airplay,
                        detail: session.airplay.failure_detail().unwrap_or_else(|| {
                            "the topology-bound AirPlay control worker stopped".to_owned()
                        }),
                    })
                } else {
                    None
                }
            }
            RuntimeConnection::Idle | RuntimeConnection::Worker { .. } => None,
        };
        if let Some(mut failure) = disconnected_failure {
            let RuntimeConnection::Active(session) =
                mem::replace(&mut self.connection, RuntimeConnection::Idle)
            else {
                unreachable!("active state checked above")
            };
            let cleanup = close_active_session(&self.carrier, session);
            if let Err(error) = cleanup {
                failure.detail.push_str("; wired session cleanup failed: ");
                failure.detail.push_str(&error.to_string());
            }
            self.record_failure(failure);
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn refresh(&mut self) {}

    #[cfg(all(target_os = "linux", feature = "linux-media"))]
    fn set_display_status(&mut self, status: DisplayStatus) -> Result<()> {
        apply_carplay_display_status(self.display.as_ref(), &mut self.display_status, status)
    }

    #[cfg(target_os = "linux")]
    fn record_failure(&mut self, failure: RuntimeFailure) {
        persist_last_wired_failure(&self.state_dir, &failure);
        self.last_failure = Some(failure);
        self.reconnect.record_failure(Instant::now());
    }

    fn idle_probe(&self) -> RuntimeProbe {
        let backoff = self.reconnect.remaining(Instant::now());
        let carrier_failure = self
            .last_failure
            .as_ref()
            .filter(|failure| failure.component == FailureComponent::Carrier);
        let (carrier_state, carrier_detail) = if let Some(failure) = carrier_failure {
            (HealthState::Failed, failure.detail.clone())
        } else if let Some(remaining) = backoff {
            (
                HealthState::NotReady,
                format!(
                    "carkit reconnect backoff active for {}ms",
                    remaining.as_millis()
                ),
            )
        } else {
            (
                HealthState::NotReady,
                "carkit byte stream is not connected".to_owned(),
            )
        };

        let identity_gate = self.iap2.first_identification_gate();
        let iap2_failure = self
            .last_failure
            .as_ref()
            .filter(|failure| failure.component == FailureComponent::Iap2);
        let (iap2_state, iap2_detail) = if !self.iap2.enable_live_session {
            (
                HealthState::NotReady,
                "live iAP2 is disabled; set iap2.enable_live_session = true only on an authorized development image".to_owned(),
            )
        } else if let Some(gate) = identity_gate {
            (
                HealthState::NotReady,
                self.identity_source_failure
                    .clone()
                    .unwrap_or_else(|| gate.detail.to_owned()),
            )
        } else if let Some(failure) = iap2_failure {
            (HealthState::Failed, failure.detail.clone())
        } else if cfg!(not(target_os = "linux")) {
            (
                HealthState::NotReady,
                "live MFi Authentication 3.0 requires Linux access to the exclusive /dev/i2c-3 driver".to_owned(),
            )
        } else {
            (
                HealthState::NotReady,
                "iAP2 is waiting for the carkit carrier and MFi coprocessor".to_owned(),
            )
        };

        let start_failure = self
            .last_failure
            .as_ref()
            .filter(|failure| failure.component == FailureComponent::Start);
        let (start_state, start_detail) = if let Some(failure) = start_failure {
            (HealthState::Failed, failure.detail.clone())
        } else {
            start_plan_probe(&self.iap2.start_plan(None, None))
        };
        let airplay_failure = self
            .last_failure
            .as_ref()
            .filter(|failure| failure.component == FailureComponent::Airplay);
        let (airplay_state, airplay_detail) = if !self.airplay.enable_live_receiver {
            (
                HealthState::NotReady,
                "live AirPlay control is disabled; no receiver socket will be advertised"
                    .to_owned(),
            )
        } else if let Some(failure) = airplay_failure {
            (HealthState::Failed, failure.detail.clone())
        } else if cfg!(not(target_os = "linux")) {
            (
                HealthState::NotReady,
                "scoped AirPlay control sockets require the Linux NCM interface runtime".to_owned(),
            )
        } else {
            (
                HealthState::NotReady,
                "AirPlay is waiting for the selected NCM interface and shared MFi coprocessor"
                    .to_owned(),
            )
        };
        projection_probe(
            ComponentProbe::new(carrier_state, carrier_detail),
            ComponentProbe::new(
                if self
                    .last_failure
                    .as_ref()
                    .is_some_and(|failure| failure.component == FailureComponent::Ncm)
                {
                    HealthState::Failed
                } else {
                    HealthState::NotReady
                },
                self.last_failure
                    .as_ref()
                    .filter(|failure| failure.component == FailureComponent::Ncm)
                    .map_or_else(
                        || {
                            "NCM is waiting for an exact selected-iPhone topology association"
                                .to_owned()
                        },
                        |failure| failure.detail.clone(),
                    ),
            ),
            ComponentProbe::new(airplay_state, airplay_detail),
            ComponentProbe::new(iap2_state, iap2_detail),
            ComponentProbe::new(start_state, start_detail),
            ProjectionCapabilities::default(),
        )
    }
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
pub(crate) fn apply_carplay_display_status(
    display: Option<&prelude_media::LinuxDisplayHandle>,
    current: &mut Option<DisplayStatus>,
    status: DisplayStatus,
) -> Result<()> {
    let Some(display) = display else {
        return Ok(());
    };
    let projection_active = status != DisplayStatus::WaitingForPhone;
    if *current != Some(status) {
        let overlay =
            crate::status_screen::render(DisplayPlatform::CarPlay, status).map_err(|error| {
                Error::Runtime(format!(
                    "rendering the CarPlay status screen failed: {error}"
                ))
            })?;
        display.set_waiting_overlay(overlay).map_err(|error| {
            Error::Runtime(format!(
                "setting the CarPlay status overlay failed: {error}"
            ))
        })?;
    }
    display
        .set_projection_active(projection_active)
        .map_err(|error| {
            Error::Runtime(format!(
                "updating the CarPlay projection activity state failed: {error}"
            ))
        })?;
    *current = Some(status);
    Ok(())
}

#[cfg(any(test, target_os = "linux"))]
fn persist_last_wired_failure(state_dir: &Path, failure: &RuntimeFailure) {
    const MAX_DETAIL_BYTES: usize = 1024;

    let directory = state_dir.join("diagnostics");
    let path = directory.join("last-wired-failure");
    let temporary = directory.join(format!(".last-wired-failure.{}.tmp", std::process::id()));
    let component = match failure.component {
        FailureComponent::Carrier => "carrier",
        FailureComponent::Ncm => "ncm",
        FailureComponent::Airplay => "airplay",
        FailureComponent::Iap2 => "iap2",
        FailureComponent::Start => "start",
    };
    let mut detail = failure.detail.as_str();
    if detail.len() > MAX_DETAIL_BYTES {
        let mut end = MAX_DETAIL_BYTES;
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        detail = &detail[..end];
    }

    let result = (|| -> std::io::Result<()> {
        fs::create_dir_all(&directory)?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        writeln!(file, "component={component}")?;
        writeln!(file, "detail={detail}")?;
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        fs::File::open(&directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
}

#[cfg(any(test, target_os = "linux"))]
fn clear_last_wired_failure(state_dir: &Path) {
    let _ = fs::remove_file(state_dir.join("diagnostics/last-wired-failure"));
}

#[cfg(any(test, target_os = "linux"))]
fn persist_private_pairing_reason(state_dir: &Path, reason: &str) {
    const MAX_REASON_BYTES: usize = 1024;

    let directory = state_dir.join("diagnostics");
    let path = directory.join("last-pairing-record-error");
    let temporary = directory.join(format!(
        ".last-pairing-record-error.{}.tmp",
        std::process::id()
    ));
    let mut reason = reason;
    if reason.len() > MAX_REASON_BYTES {
        let mut end = MAX_REASON_BYTES;
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        reason = &reason[..end];
    }

    let result = (|| -> std::io::Result<()> {
        fs::create_dir_all(&directory)?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        writeln!(file, "{reason}")?;
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        fs::File::open(&directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
}

impl ProjectionRuntime for WiredCarkitRuntime {
    fn probe(&mut self) -> RuntimeProbe {
        self.refresh();
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        {
            let status = if !self.phone_present {
                Some(DisplayStatus::WaitingForPhone)
            } else {
                match &self.connection {
                    RuntimeConnection::Idle => None,
                    RuntimeConnection::Worker { progress, .. } => {
                        progress.lock().ok().map(|progress| {
                            if progress.audio_output_missing {
                                DisplayStatus::MissingAudioOutput
                            } else {
                                carplay_display_status(progress.stage)
                            }
                        })
                    }
                    RuntimeConnection::Active(session) => {
                        if session.airplay.audio_output_missing() {
                            Some(DisplayStatus::MissingAudioOutput)
                        } else {
                            Some(
                                session
                                    .airplay
                                    .snapshot()
                                    .stage
                                    .display_status(ConnectionStep::WaitingForIphone),
                            )
                        }
                    }
                }
            };
            if let Some(status) = status {
                let _ = self.set_display_status(status);
            } else if let Some(display) = self.display.as_ref() {
                let _ = display.set_projection_active(false);
            }
        }
        match &mut self.connection {
            RuntimeConnection::Idle => self.idle_probe(),
            #[cfg(target_os = "linux")]
            RuntimeConnection::Worker { progress, .. } => worker_probe(progress, &self.iap2),
            #[cfg(target_os = "linux")]
            RuntimeConnection::Active(session) => active_probe(session),
        }
    }

    fn set_phone_presence(&mut self, present: bool) {
        self.phone_present = present;
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        if !present {
            let _ = self.set_display_status(DisplayStatus::WaitingForPhone);
        }
    }

    fn start(&mut self, device: &ProjectionDevice) -> Result<()> {
        self.refresh();
        let ProjectionDevice::CarPlay(device) = device else {
            return Err(Error::Unsupported(
                "wired carkit runtime cannot start an Android Auto device",
            ));
        };
        if !matches!(self.connection, RuntimeConnection::Idle) {
            return Ok(());
        }
        if !self.iap2.enable_live_session {
            return Err(Error::NotReady {
                component: "iAP2 session",
                state: "live_session_disabled",
            });
        }
        if !self.airplay.enable_live_receiver {
            return Err(Error::NotReady {
                component: "AirPlay control receiver",
                state: "live_receiver_disabled",
            });
        }
        if let Some(gate) = self.iap2.first_identification_gate() {
            return Err(Error::NotReady {
                component: "iAP2 identification",
                state: gate.state,
            });
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = device;
            Err(Error::NotReady {
                component: "MFi Authentication 3.0",
                state: "linux_i2c_required",
            })
        }

        #[cfg(target_os = "linux")]
        {
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            {
                self.phone_present = true;
                if !self.airplay.disable_audio_output && self.media.audio == AudioBackend::AlsaUsb {
                    let available =
                        crate::media_runtime::configured_audio_output_available(&self.media, true)
                            .map_err(Error::Runtime)?;
                    if !available {
                        self.set_display_status(DisplayStatus::MissingAudioOutput)?;
                        return Err(Error::NotReady {
                            component: "audio output device",
                            state: "not_connected",
                        });
                    }
                }
            }
            if self.reconnect.remaining(Instant::now()).is_some() {
                return Err(Error::NotReady {
                    component: "wired carkit carrier",
                    state: "reconnect_backoff",
                });
            }
            let progress = Arc::new(Mutex::new(WorkerProgress {
                stage: WorkerStage::ConnectingCarrier,
                ncm_link_local: None,
                ncm_kind: None,
                audio_output_missing: false,
            }));
            let thread_progress = Arc::clone(&progress);
            let cancellation = CancellationToken::default();
            let thread_cancellation = cancellation.clone();
            let carrier = self.carrier.clone();
            let device = device.clone();
            let config = self.iap2.clone();
            let airplay = self.airplay.clone();
            let media = self.media.clone();
            let platform = self.platform.clone();
            let ambient_light = self.ambient_light.clone();
            let state_dir = self.state_dir.clone();
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            let display = self.display.clone();
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            persist_last_display_platform(&self.state_dir, DisplayPlatform::CarPlay).map_err(
                |error| {
                    Error::Runtime(format!(
                        "persisting the CarPlay display platform failed: {error}"
                    ))
                },
            )?;
            #[cfg(all(target_os = "linux", feature = "linux-media"))]
            self.set_display_status(DisplayStatus::UnlockPhone)?;
            self.phone_present = true;
            let handle = match thread::Builder::new()
                .name("prelude-iap2".to_owned())
                .spawn(move || {
                    run_live_worker(
                        carrier,
                        device,
                        config,
                        airplay,
                        media,
                        platform,
                        ambient_light,
                        state_dir,
                        #[cfg(all(target_os = "linux", feature = "linux-media"))]
                        display,
                        &thread_progress,
                        &thread_cancellation,
                    )
                }) {
                Ok(handle) => handle,
                Err(error) => {
                    #[cfg(all(target_os = "linux", feature = "linux-media"))]
                    {
                        self.phone_present = false;
                    }
                    #[cfg(all(target_os = "linux", feature = "linux-media"))]
                    let _ = self.set_display_status(DisplayStatus::WaitingForPhone);
                    return Err(Error::Runtime(format!(
                        "failed to start wired session worker: {error}"
                    )));
                }
            };
            self.last_failure = None;
            self.connection = RuntimeConnection::Worker {
                handle,
                progress,
                cancellation,
            };
            Ok(())
        }
    }

    fn audio_status(&mut self) -> AudioRuntimeSnapshot {
        self.refresh();
        #[cfg(target_os = "linux")]
        if let RuntimeConnection::Active(session) = &self.connection {
            return session.airplay.audio_status();
        }
        let requested_route = match (self.airplay.disable_audio_output, self.media.audio) {
            (false, AudioBackend::AlsaUsb) => AudioRuntimeRoute::UsbDac,
            _ => AudioRuntimeRoute::Disabled,
        };
        if requested_route == AudioRuntimeRoute::Disabled {
            return AudioRuntimeSnapshot::disabled();
        }
        let mut status = AudioRuntimeSnapshot::waiting(requested_route);
        if let Some(failure) = self
            .last_failure
            .as_ref()
            .filter(|failure| failure.component == FailureComponent::Airplay)
        {
            status.pipeline_state = AudioPipelineState::Failed;
            status.last_failure =
                Some(if failure.detail.contains("audio route preflight failed") {
                    AudioFailureKind::Endpoint
                } else {
                    AudioFailureKind::MediaWorker
                });
        }
        status
    }

    fn play_audio_test_tone(&mut self) -> Result<()> {
        self.refresh();
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        if let RuntimeConnection::Active(session) = &self.connection {
            return session
                .airplay
                .play_audio_test_tone()
                .map_err(Error::Runtime);
        }
        Err(Error::NotReady {
            component: "audio mixer",
            state: "not running",
        })
    }

    fn stop(&mut self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            let connection = mem::replace(&mut self.connection, RuntimeConnection::Idle);
            match connection {
                RuntimeConnection::Idle => {}
                RuntimeConnection::Active(session) => close_active_session(&self.carrier, session)?,
                RuntimeConnection::Worker {
                    handle,
                    cancellation,
                    ..
                } => match cancel_and_join(cancellation, handle) {
                    Ok(WorkerOutcome::Active(session)) => {
                        close_active_session(&self.carrier, session)?;
                    }
                    Ok(WorkerOutcome::Failed(_)) => {}
                    Err(_) => {
                        return Err(Error::Runtime(
                            "wired session worker terminated during shutdown".to_owned(),
                        ));
                    }
                },
            }
        }
        self.last_failure = None;
        self.reconnect.record_success();
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        self.set_display_status(DisplayStatus::WaitingForPhone)?;
        Ok(())
    }
}

#[cfg(any(test, target_os = "linux"))]
fn cancel_and_join<T>(
    cancellation: CancellationToken,
    handle: std::thread::JoinHandle<T>,
) -> std::thread::Result<T> {
    cancellation.cancel();
    handle.join()
}

pub(crate) struct ComponentProbe {
    state: HealthState,
    detail: String,
}

impl ComponentProbe {
    pub(crate) fn new(state: HealthState, detail: String) -> Self {
        Self { state, detail }
    }
}

pub(crate) fn projection_probe(
    carrier: ComponentProbe,
    ncm: ComponentProbe,
    airplay: ComponentProbe,
    iap2: ComponentProbe,
    start: ComponentProbe,
    capabilities: ProjectionCapabilities,
) -> RuntimeProbe {
    let states = [
        carrier.state,
        ncm.state,
        airplay.state,
        iap2.state,
        start.state,
    ];
    let state = if states.contains(&HealthState::Failed) {
        HealthState::Failed
    } else if capabilities.control
        && capabilities.video
        && capabilities.input
        && states.iter().all(|state| *state == HealthState::Ready)
    {
        HealthState::Ready
    } else if states.contains(&HealthState::Starting) {
        HealthState::Starting
    } else {
        HealthState::NotReady
    };
    RuntimeProbe {
        state,
        detail: if state == HealthState::Ready {
            if capabilities.audio && capabilities.microphone {
                "experimental wired CarPlay control, screen, selected audio output, microphone, and input are live"
                    .to_owned()
            } else {
                "experimental wired CarPlay control, screen, and input are live; optional audio output is disabled"
                    .to_owned()
            }
        } else {
            "experimental wired CarPlay is fail-closed until every configured live capability is ready"
                .to_owned()
        },
        transport_state: carrier.state,
        transport_detail: carrier.detail,
        link_state: ncm.state,
        link_detail: ncm.detail,
        control_state: airplay.state,
        control_detail: airplay.detail,
        authentication_state: iap2.state,
        authentication_detail: iap2.detail,
        session_state: start.state,
        session_detail: start.detail,
        implementation: ImplementationStatus::ExperimentalPrivateProtocol,
        capabilities,
    }
}

fn start_plan_probe(plan: &SessionStartPlan) -> (HealthState, String) {
    match plan {
        SessionStartPlan::Ready(_) => (
            HealthState::NotReady,
            "validated start inputs are configured; waiting for wired CarPlay availability"
                .to_owned(),
        ),
        SessionStartPlan::NotReady(gate) => (HealthState::NotReady, gate.detail.to_owned()),
    }
}

#[cfg(target_os = "linux")]
type SharedProgress = Arc<Mutex<WorkerProgress>>;

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
struct WorkerProgress {
    stage: WorkerStage,
    ncm_link_local: Option<std::net::Ipv6Addr>,
    ncm_kind: Option<NcmCarrierKind>,
    audio_output_missing: bool,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerStage {
    ConnectingCarrier,
    BringingUpNcm,
    OpeningMfi,
    BindingAirplay,
    Setup,
    Availability,
    Start,
}

#[cfg(target_os = "linux")]
type ProductionSession =
    WiredCarkitSession<CarkitStreamAdapter<prelude_usb_carkit::CarkitByteStream>, SharedMfi>;

#[cfg(target_os = "linux")]
struct ActiveSession {
    session: ProductionSession,
    airplay: AirPlayRuntime,
    ncm: WiredNcm,
    start: StartDisposition,
}

#[cfg(target_os = "linux")]
impl fmt::Debug for ActiveSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActiveSession")
            .field("phase", &self.session.phase())
            .field("airplay", &self.airplay)
            .field("ncm", &self.ncm)
            .field("start", &self.start)
            .finish_non_exhaustive()
    }
}

#[cfg(target_os = "linux")]
impl ActiveSession {
    fn carrier_alive(&mut self) -> bool {
        self.session.stream_mut().is_alive()
    }

    fn ncm_alive(&self) -> bool {
        self.ncm.is_alive()
    }

    fn airplay_alive(&self) -> bool {
        self.airplay.is_alive()
    }
}

#[cfg(target_os = "linux")]
enum WorkerOutcome {
    Active(Box<ActiveSession>),
    Failed(RuntimeFailure),
}

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn run_live_worker(
    carrier: WiredCarrier,
    device: IphoneCandidate,
    config: crate::Iap2Config,
    airplay_config: AirPlayConfig,
    media_config: crate::MediaConfig,
    platform_config: crate::PlatformConfig,
    ambient_light: AmbientLightState,
    state_dir: std::path::PathBuf,
    #[cfg(all(target_os = "linux", feature = "linux-media"))] display: Option<
        prelude_media::LinuxDisplayHandle,
    >,
    progress: &SharedProgress,
    cancellation: &CancellationToken,
) -> WorkerOutcome {
    set_progress(progress, WorkerStage::ConnectingCarrier);
    let stream = match carrier.connect_cancellable(&device, cancellation) {
        Ok(stream) => stream,
        Err(error) => {
            if let CarrierError::InvalidPairingRecord(reason) = &error {
                persist_private_pairing_reason(&state_dir, reason);
            }
            return WorkerOutcome::Failed(RuntimeFailure {
                component: FailureComponent::Carrier,
                detail: sanitized_carrier_error(&error),
            });
        }
    };
    if cancellation.is_cancelled() {
        return fail_and_disconnect(
            &carrier,
            stream,
            FailureComponent::Ncm,
            "wired session was cancelled before NCM setup".to_owned(),
        );
    }
    set_progress(progress, WorkerStage::BringingUpNcm);
    let ncm = match WiredNcm::start(
        stream.active_candidate(),
        WiredNcmConfig::default(),
        cancellation.clone(),
    ) {
        Ok(ncm) => ncm,
        Err(error) => {
            return fail_and_disconnect(
                &carrier,
                stream,
                FailureComponent::Ncm,
                sanitized_carrier_error(&error),
            );
        }
    };
    let ncm_link_local = ncm.link_local_address();
    set_ncm_progress(progress, ncm_link_local, ncm.kind());
    let identification = match config.identification(ncm.carplay_interface_number()) {
        Ok(identification) => identification,
        Err(gate) => {
            return fail_ncm_and_disconnect(
                &carrier,
                stream,
                ncm,
                FailureComponent::Iap2,
                gate.detail.to_owned(),
            );
        }
    };

    if cancellation.is_cancelled() {
        return fail_ncm_and_disconnect(
            &carrier,
            stream,
            ncm,
            FailureComponent::Iap2,
            "wired session was cancelled before opening MFi authentication".to_owned(),
        );
    }

    set_progress(progress, WorkerStage::OpeningMfi);
    let mfi = match SharedMfi::open_default() {
        Ok(mfi) => mfi,
        Err(error) => {
            return fail_ncm_and_disconnect(
                &carrier,
                stream,
                ncm,
                FailureComponent::Iap2,
                format!(
                    "MFi Authentication 3.0 coprocessor /dev/i2c-3 is unavailable or invalid: {error}"
                ),
            );
        }
    };

    if cancellation.is_cancelled() {
        drop(mfi);
        return fail_ncm_and_disconnect(
            &carrier,
            stream,
            ncm,
            FailureComponent::Iap2,
            "wired session was cancelled before AirPlay setup".to_owned(),
        );
    }

    set_progress(progress, WorkerStage::BindingAirplay);
    let airplay = match AirPlayRuntime::bind(
        &airplay_config,
        &media_config,
        &platform_config,
        &config,
        &state_dir,
        ncm_link_local,
        ncm.interface_name(),
        ncm.mac_address(),
        mfi.clone(),
        #[cfg(all(target_os = "linux", feature = "linux-media"))]
        display,
        ambient_light,
    ) {
        Ok(airplay) => airplay,
        Err(error) => {
            drop(mfi);
            return fail_ncm_and_disconnect(
                &carrier,
                stream,
                ncm,
                FailureComponent::Airplay,
                format!("topology-bound AirPlay receiver is not ready: {error}"),
            );
        }
    };
    set_audio_output_missing(progress, airplay.audio_output_missing());

    if cancellation.is_cancelled() {
        drop(mfi);
        return fail_airplay_ncm_and_disconnect(
            &carrier,
            stream,
            airplay,
            ncm,
            FailureComponent::Iap2,
            "wired session was cancelled before iAP2 setup".to_owned(),
        );
    }

    let adapter = CarkitStreamAdapter::new(stream);
    let recovery = adapter.recovery_handle();
    let mut session = match WiredCarkitSession::new(adapter, mfi, identification) {
        Ok(session) => {
            drop(recovery);
            session
        }
        Err(error) => {
            return match recovery.take() {
                Ok(stream) => fail_airplay_ncm_and_disconnect(
                    &carrier,
                    stream,
                    airplay,
                    ncm,
                    FailureComponent::Iap2,
                    format!("iAP2 session construction failed: {error}"),
                ),
                Err(_) => {
                    let mut detail = "iAP2 session construction failed and the carrier stream could not be recovered".to_owned();
                    if let Err(error) = airplay.stop() {
                        detail.push_str("; AirPlay cleanup failed: ");
                        detail.push_str(&error);
                    }
                    if let Err(error) = ncm.stop() {
                        detail.push_str("; NCM cleanup failed: ");
                        detail.push_str(&sanitized_carrier_error(&error));
                    }
                    WorkerOutcome::Failed(RuntimeFailure {
                        component: FailureComponent::Iap2,
                        detail,
                    })
                }
            };
        }
    };
    let start_plan = config.start_plan(Some(ncm_link_local), Some(airplay.start_metadata()));
    let result = drive_session_with_observer(
        &mut session,
        &start_plan,
        |stage| {
            set_progress(
                progress,
                match stage {
                    DriverStage::Setup => WorkerStage::Setup,
                    DriverStage::Availability => WorkerStage::Availability,
                    DriverStage::Start => WorkerStage::Start,
                },
            );
        },
        || cancellation.is_cancelled(),
    );
    match result {
        Ok(start) => WorkerOutcome::Active(Box::new(ActiveSession {
            session,
            airplay,
            ncm,
            start,
        })),
        Err(error) => {
            let component = if progress
                .lock()
                .ok()
                .is_some_and(|progress| progress.stage == WorkerStage::Start)
            {
                FailureComponent::Start
            } else {
                FailureComponent::Iap2
            };
            match recover_session_stream(session) {
                Ok(stream) => fail_airplay_ncm_and_disconnect(
                    &carrier,
                    stream,
                    airplay,
                    ncm,
                    component,
                    format!("wired iAP2 session failed: {error}"),
                ),
                Err(_) => {
                    let mut detail =
                        "wired iAP2 session failed and the carrier stream could not be recovered"
                            .to_owned();
                    if let Err(cleanup) = airplay.stop() {
                        detail.push_str("; AirPlay cleanup failed: ");
                        detail.push_str(&cleanup);
                    }
                    if let Err(cleanup) = ncm.stop() {
                        detail.push_str("; NCM cleanup failed: ");
                        detail.push_str(&sanitized_carrier_error(&cleanup));
                    }
                    WorkerOutcome::Failed(RuntimeFailure { component, detail })
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn set_progress(progress: &SharedProgress, stage: WorkerStage) {
    if let Ok(mut current) = progress.lock() {
        current.stage = stage;
    }
}

#[cfg(target_os = "linux")]
fn set_audio_output_missing(progress: &SharedProgress, missing: bool) {
    if let Ok(mut current) = progress.lock() {
        current.audio_output_missing = missing;
    }
}

#[cfg(target_os = "linux")]
fn set_ncm_progress(
    progress: &SharedProgress,
    link_local: std::net::Ipv6Addr,
    kind: NcmCarrierKind,
) {
    if let Ok(mut current) = progress.lock() {
        current.ncm_link_local = Some(link_local);
        current.ncm_kind = Some(kind);
    }
}

#[cfg(target_os = "linux")]
fn fail_and_disconnect(
    carrier: &WiredCarrier,
    stream: prelude_usb_carkit::CarkitByteStream,
    component: FailureComponent,
    mut detail: String,
) -> WorkerOutcome {
    if let Err(error) = carrier.disconnect(stream) {
        detail.push_str("; prior USB configuration restoration failed: ");
        detail.push_str(&sanitized_carrier_error(&error));
    }
    WorkerOutcome::Failed(RuntimeFailure { component, detail })
}

#[cfg(target_os = "linux")]
fn fail_ncm_and_disconnect(
    carrier: &WiredCarrier,
    stream: prelude_usb_carkit::CarkitByteStream,
    ncm: WiredNcm,
    component: FailureComponent,
    mut detail: String,
) -> WorkerOutcome {
    if let Err(error) = ncm.stop() {
        detail.push_str("; NCM cleanup failed: ");
        detail.push_str(&sanitized_carrier_error(&error));
    }
    fail_and_disconnect(carrier, stream, component, detail)
}

#[cfg(target_os = "linux")]
fn fail_airplay_ncm_and_disconnect(
    carrier: &WiredCarrier,
    stream: prelude_usb_carkit::CarkitByteStream,
    airplay: AirPlayRuntime,
    ncm: WiredNcm,
    component: FailureComponent,
    mut detail: String,
) -> WorkerOutcome {
    if let Err(error) = airplay.stop() {
        detail.push_str("; AirPlay cleanup failed: ");
        detail.push_str(&error);
    }
    fail_ncm_and_disconnect(carrier, stream, ncm, component, detail)
}

#[cfg(target_os = "linux")]
fn close_active_session(carrier: &WiredCarrier, session: Box<ActiveSession>) -> Result<()> {
    let ActiveSession {
        session,
        airplay,
        ncm,
        start: _,
    } = *session;
    let stream = recover_session_stream(session)
        .map_err(|error| Error::Runtime(format!("failed to recover carkit stream: {error}")));
    let airplay_result = airplay
        .stop()
        .map_err(|error| Error::Runtime(format!("AirPlay cleanup failed: {error}")));
    let ncm_result = ncm
        .stop()
        .map_err(|error| Error::Runtime(sanitized_carrier_error(&error)));
    let network_result = airplay_result.and(ncm_result);
    match stream {
        Ok(stream) => {
            let carrier_result = carrier
                .disconnect(stream)
                .map_err(|error| Error::Runtime(sanitized_carrier_error(&error)));
            network_result.and(carrier_result)
        }
        Err(stream_error) => match network_result {
            Ok(()) => Err(stream_error),
            Err(cleanup_error) => Err(Error::Runtime(format!(
                "{stream_error}; AirPlay/NCM cleanup also failed: {cleanup_error}"
            ))),
        },
    }
}

#[cfg(target_os = "linux")]
fn worker_probe(progress: &SharedProgress, config: &crate::Iap2Config) -> RuntimeProbe {
    let progress = progress
        .lock()
        .map(|progress| *progress)
        .unwrap_or(WorkerProgress {
            stage: WorkerStage::Setup,
            ncm_link_local: None,
            ncm_kind: None,
            audio_output_missing: false,
        });
    let stage = progress.stage;
    let carrier_ready = stage != WorkerStage::ConnectingCarrier;
    let (iap2_state, iap2_detail) = match stage {
        WorkerStage::ConnectingCarrier => (
            HealthState::NotReady,
            "iAP2 is waiting for the trusted TLS carkit carrier".to_owned(),
        ),
        WorkerStage::BringingUpNcm => (
            HealthState::NotReady,
            "iAP2 is waiting for the topology-associated NCM carrier".to_owned(),
        ),
        WorkerStage::OpeningMfi => (
            HealthState::Starting,
            "opening and validating the exclusive /dev/i2c-3 MFi Authentication 3.0 coprocessor"
                .to_owned(),
        ),
        WorkerStage::BindingAirplay => (
            HealthState::Starting,
            "the shared MFi coprocessor is ready; binding the scoped AirPlay receiver sockets"
                .to_owned(),
        ),
        WorkerStage::Setup => (
            HealthState::Starting,
            "running direct iAP2 link negotiation, accessory authentication, and identification"
                .to_owned(),
        ),
        WorkerStage::Availability => (
            HealthState::Starting,
            "authenticated iAP2 setup completed; waiting for wired CarPlay availability".to_owned(),
        ),
        WorkerStage::Start => (
            HealthState::Ready,
            "authenticated iAP2 session and wired CarPlay availability are ready".to_owned(),
        ),
    };
    let airplay_bound = matches!(
        stage,
        WorkerStage::Setup | WorkerStage::Availability | WorkerStage::Start
    );
    let (start_state, start_detail) = if stage == WorkerStage::Start {
        (
            HealthState::Starting,
            "sending validated wired CarPlay start metadata".to_owned(),
        )
    } else if airplay_bound {
        (
            HealthState::NotReady,
            "live scoped AirPlay and NCM metadata are validated; waiting for wired CarPlay availability"
                .to_owned(),
        )
    } else {
        start_plan_probe(&config.start_plan(progress.ncm_link_local, None))
    };
    let ncm_ready = progress.ncm_link_local.is_some() && progress.ncm_kind.is_some();
    let ncm_detail = match progress.ncm_kind {
        Some(kind) if ncm_ready => format!(
            "verified link-local IPv6 is active on the {kind:?} topology-associated NCM carrier"
        ),
        _ => "associating a kernel cdc_ncm/ipheth interface or starting the unbound userspace fallback"
            .to_owned(),
    };
    projection_probe(
        ComponentProbe::new(
            if carrier_ready {
                HealthState::Ready
            } else {
                HealthState::Starting
            },
            if carrier_ready {
                "trusted TLS com.apple.carkit.service byte stream established".to_owned()
            } else {
                "opening the configuration-6 usbmux/Lockdown carkit carrier".to_owned()
            },
        ),
        ComponentProbe::new(
            if ncm_ready {
                HealthState::Ready
            } else {
                HealthState::Starting
            },
            ncm_detail,
        ),
        ComponentProbe::new(
            if airplay_bound {
                HealthState::Ready
            } else {
                HealthState::Starting
            },
            if airplay_bound {
                "scoped AirPlay RTSP control and negotiation sockets are bound on the selected NCM interface"
                    .to_owned()
            } else {
                "waiting to bind and retain AirPlay sockets on the selected NCM interface"
                    .to_owned()
            },
        ),
        ComponentProbe::new(iap2_state, iap2_detail),
        ComponentProbe::new(start_state, start_detail),
        ProjectionCapabilities::default(),
    )
}

#[cfg(all(target_os = "linux", feature = "linux-media"))]
fn carplay_display_status(stage: WorkerStage) -> DisplayStatus {
    let step = match stage {
        WorkerStage::ConnectingCarrier => return DisplayStatus::UnlockPhone,
        WorkerStage::BringingUpNcm => ConnectionStep::StartingUsbNetwork,
        WorkerStage::OpeningMfi => ConnectionStep::PreparingAuthentication,
        WorkerStage::BindingAirplay => ConnectionStep::StartingReceiver,
        WorkerStage::Setup => ConnectionStep::Authenticating,
        WorkerStage::Availability => ConnectionStep::WaitingForCarPlay,
        WorkerStage::Start => ConnectionStep::StartingCarPlay,
    };
    DisplayStatus::Connecting(Some(step))
}

#[cfg(target_os = "linux")]
fn active_probe(session: &ActiveSession) -> RuntimeProbe {
    let (start_state, start_detail) = match session.start {
        StartDisposition::Sent => (
            HealthState::Ready,
            "validated wired CarPlay start message was sent".to_owned(),
        ),
        StartDisposition::NotReady(gate) => (HealthState::NotReady, gate.detail.to_owned()),
    };
    let phase = match session.session.phase() {
        SessionPhase::ReadyForCarPlay => "ready_for_carplay",
        SessionPhase::CarPlayStartSent => "carplay_start_sent",
        _ => "unexpected",
    };
    let airplay = session.airplay.snapshot();
    let (airplay_state, airplay_detail) = airplay_probe(airplay, session.airplay.is_alive());
    projection_probe(
        ComponentProbe::new(
            HealthState::Ready,
            "trusted TLS com.apple.carkit.service byte stream established".to_owned(),
        ),
        ComponentProbe::new(
            if session.ncm.is_alive() {
                HealthState::Ready
            } else {
                HealthState::Failed
            },
            format!(
                "verified link-local IPv6 is active on the {:?} topology-associated NCM carrier",
                session.ncm.kind()
            ),
        ),
        ComponentProbe::new(airplay_state, airplay_detail),
        ComponentProbe::new(
            HealthState::Ready,
            format!("authenticated direct iAP2 session is active (phase: {phase})"),
        ),
        ComponentProbe::new(start_state, start_detail),
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
    )
}

#[cfg(target_os = "linux")]
pub(crate) fn airplay_probe(progress: AirPlayProgress, alive: bool) -> (HealthState, String) {
    if !alive || matches!(progress.stage, AirPlayStage::Failed | AirPlayStage::Stopped) {
        return (
            HealthState::Failed,
            "the topology-bound AirPlay control worker is not running".to_owned(),
        );
    }
    let control_detail = match progress.stage {
        AirPlayStage::Listening => {
            "scoped RTSP listener and negotiation sockets are bound; awaiting the selected iPhone"
        }
        AirPlayStage::Connected => {
            "the selected iPhone is connected to the cleartext AirPlay control phase"
        }
        AirPlayStage::EncryptedControl => {
            "pair verification completed and the AirPlay control channel is encrypted"
        }
        AirPlayStage::MfiAuthenticated => {
            "encrypted AirPlay control completed hardware-backed MFiSAP authentication"
        }
        AirPlayStage::SessionConfigured => {
            "AirPlay session ports were negotiated from retained bound sockets"
        }
        AirPlayStage::StreamsConfigured => {
            "AirPlay stream sockets are owned by bounded timing/video/audio/microphone workers"
        }
        AirPlayStage::Recording => {
            if progress.media_recording {
                if progress.audio_ready {
                    "AirPlay RECORD is active with the preflighted H.264 and selected audio backends"
                } else {
                    "AirPlay RECORD is active with H.264 video; audio output is unavailable"
                }
            } else {
                "AirPlay RECORD is waiting for factual media backend readiness"
            }
        }
        AirPlayStage::Failed | AirPlayStage::Stopped => unreachable!("handled above"),
    };
    let event_detail = if progress.event_channel_ready {
        "encrypted event channel is connected"
    } else {
        "encrypted event channel is awaiting the selected iPhone"
    };
    let input_detail = if progress.input_ready {
        "Superbird input is owned and HID declarations are active"
    } else {
        "HID declarations are omitted because no live input worker is ready"
    };
    let state = if progress.stage == AirPlayStage::Recording && progress.media_recording {
        HealthState::Ready
    } else {
        HealthState::Starting
    };
    (
        state,
        format!("{control_detail}; {event_detail}; {input_detail}"),
    )
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug)]
struct ReconnectSchedule {
    initial: Duration,
    maximum: Duration,
    failures: u32,
    next_attempt: Option<Instant>,
}

impl ReconnectSchedule {
    fn new(initial: Duration, maximum: Duration) -> Self {
        Self {
            initial,
            maximum,
            failures: 0,
            next_attempt: None,
        }
    }

    #[cfg(any(test, target_os = "linux"))]
    fn record_failure(&mut self, now: Instant) {
        let shift = self.failures.min(31);
        let multiplier = 1_u32.checked_shl(shift).unwrap_or(u32::MAX);
        let delay = self.initial.saturating_mul(multiplier).min(self.maximum);
        self.failures = self.failures.saturating_add(1);
        self.next_attempt = Some(now + delay);
    }

    fn record_success(&mut self) {
        self.failures = 0;
        self.next_attempt = None;
    }

    fn remaining(&self, now: Instant) -> Option<Duration> {
        self.next_attempt
            .and_then(|next| next.checked_duration_since(now))
            .filter(|remaining| !remaining.is_zero())
    }
}

fn sanitized_carrier_error(error: &CarrierError) -> String {
    match error {
        CarrierError::PairingRecordMissing { .. } => {
            "trusted Lockdown pairing record is missing".to_owned()
        }
        CarrierError::InvalidPairingRecord(reason) => sanitized_pairing_record_error(reason),
        CarrierError::LockdownRejected { request, reason } => {
            format!("Lockdown rejected {request}: {reason}")
        }
        CarrierError::Tls(reason) => format!("Lockdown/carkit TLS setup failed: {reason}"),
        _ => error.to_string(),
    }
}

fn sanitized_pairing_record_error(reason: &str) -> String {
    let category = if reason == "iPhone DevicePublicKey is not PEM text" {
        "device_public_key_not_pem"
    } else if reason == "iPhone DevicePublicKey is not an RSA public key" {
        "device_public_key_not_rsa"
    } else if reason == "iPhone DevicePublicKey is smaller than RSA-2048" {
        "device_public_key_too_small"
    } else if reason.starts_with("root key generation failed:") {
        "root_key_generation"
    } else if reason.starts_with("host key generation failed:") {
        "host_key_generation"
    } else if reason.starts_with("pairing certificate generation failed:") {
        "certificate_generation"
    } else if reason.starts_with("certificate encoding failed:") {
        "certificate_encoding"
    } else if reason.starts_with("pairing key encoding failed:") {
        "key_encoding"
    } else if reason.starts_with("system clock cannot represent certificate") {
        "certificate_clock"
    } else {
        "record_validation"
    };
    format!("Lockdown pairing record is invalid ({category})")
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;

    fn experimental_config() -> (tempfile::TempDir, DaemonConfig) {
        let directory = tempfile::tempdir().expect("pairing store");
        let mut config = DaemonConfig::default();
        config.usb.configuration = Some(6);
        config.usb.configuration_is_private = true;
        config.usb.allow_configuration_changes = true;
        config.usb.allow_private_apple_configuration = true;
        config.usb.allow_private_lockdown_service = true;
        config.usb.pairing_records_directory = directory.path().to_path_buf();
        config.projection.mode = ProjectionMode::ExperimentalPrivateProtocol;
        config.projection.allow_experimental_private_protocol = true;
        (directory, config)
    }

    #[test]
    fn reconnect_backoff_doubles_and_caps_without_sleeping() {
        let start = Instant::now();
        let mut reconnect = ReconnectSchedule::new(Duration::from_secs(2), Duration::from_secs(5));
        reconnect.record_failure(start);
        assert_eq!(reconnect.remaining(start), Some(Duration::from_secs(2)));
        reconnect.record_failure(start + Duration::from_secs(2));
        assert_eq!(
            reconnect.remaining(start + Duration::from_secs(2)),
            Some(Duration::from_secs(4))
        );
        reconnect.record_failure(start + Duration::from_secs(6));
        assert_eq!(
            reconnect.remaining(start + Duration::from_secs(6)),
            Some(Duration::from_secs(5))
        );
        reconnect.record_success();
        assert_eq!(reconnect.remaining(start), None);
    }

    #[test]
    fn missing_pairing_record_error_does_not_leak_device_identifier() {
        let error = CarrierError::PairingRecordMissing {
            device: "sensitive-device-id".to_owned(),
            directory: "/var/lib/lockdown".into(),
        };
        let diagnostic = sanitized_carrier_error(&error);
        assert!(!diagnostic.contains("sensitive-device-id"));
    }

    #[test]
    fn pairing_record_diagnostic_exposes_only_a_fixed_failure_category() {
        let diagnostic = sanitized_carrier_error(&CarrierError::InvalidPairingRecord(
            "root key generation failed: sensitive backend detail".to_owned(),
        ));
        assert_eq!(
            diagnostic,
            "Lockdown pairing record is invalid (root_key_generation)"
        );
        assert!(!diagnostic.contains("sensitive"));

        let unknown = sanitized_carrier_error(&CarrierError::InvalidPairingRecord(
            "untrusted value containing a secret".to_owned(),
        ));
        assert_eq!(
            unknown,
            "Lockdown pairing record is invalid (record_validation)"
        );
        assert!(!unknown.contains("secret"));
    }

    #[test]
    fn last_wired_failure_is_atomic_bounded_and_clearable() {
        use std::os::unix::fs::PermissionsExt;

        let state = tempfile::tempdir().expect("state directory");
        let detail = "é".repeat(700);
        persist_last_wired_failure(
            state.path(),
            &RuntimeFailure {
                component: FailureComponent::Carrier,
                detail,
            },
        );

        let path = state.path().join("diagnostics/last-wired-failure");
        let contents = std::fs::read_to_string(&path).expect("persistent diagnostic");
        assert!(contents.starts_with("component=carrier\ndetail="));
        assert!(contents.len() <= 1024 + "component=carrier\ndetail=\n".len());
        assert_eq!(
            std::fs::metadata(&path)
                .expect("diagnostic metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_dir(state.path().join("diagnostics"))
                .expect("diagnostics directory")
                .count(),
            1
        );

        clear_last_wired_failure(state.path());
        assert!(!path.exists());
    }

    #[test]
    fn private_pairing_reason_is_bounded_atomic_and_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let state = tempfile::tempdir().expect("state directory");
        persist_private_pairing_reason(state.path(), &"é".repeat(700));
        let path = state.path().join("diagnostics/last-pairing-record-error");
        let contents = std::fs::read_to_string(&path).expect("pairing diagnostic");
        assert!(contents.len() <= 1025);
        assert!(contents.ends_with('\n'));
        assert_eq!(
            std::fs::metadata(path)
                .expect("pairing diagnostic metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn experimental_selection_reports_separate_fail_closed_gates() {
        let (_directory, config) = experimental_config();
        config.validate().expect("config");
        let mut runtime = SelectedRuntime::from_config(&config).expect("runtime");
        assert!(matches!(runtime, SelectedRuntime::Wired(_)));
        let probe = runtime.probe();
        assert_eq!(probe.state, HealthState::NotReady);
        assert_eq!(
            probe.implementation,
            ImplementationStatus::ExperimentalPrivateProtocol
        );
        assert_eq!(probe.transport_state, HealthState::NotReady);
        assert_eq!(probe.link_state, HealthState::NotReady);
        assert_eq!(probe.authentication_state, HealthState::NotReady);
        assert_eq!(probe.session_state, HealthState::NotReady);
        assert!(probe.authentication_detail.contains("enable_live_session"));
        assert!(probe.session_detail.contains("link-local IPv6"));
    }

    #[test]
    fn packaged_live_profile_selects_the_automatic_protocol_supervisor() {
        let config = DaemonConfig::from_toml(include_str!(
            "../../../image/meta-prelude/recipes-core/prelude/files/config-dev.toml"
        ))
        .expect("live packaged config");
        let mut runtime = SelectedRuntime::from_config(&config).expect("automatic runtime");
        assert!(matches!(runtime, SelectedRuntime::Automatic(_)));
        let probe = runtime.probe();
        assert_eq!(probe.state, HealthState::NotReady);
        assert!(probe.detail.contains("waiting for one supported phone"));
    }

    #[test]
    fn projection_readiness_requires_recording_backed_capabilities() {
        let probe = projection_probe(
            ComponentProbe::new(HealthState::Ready, "carrier".to_owned()),
            ComponentProbe::new(HealthState::Ready, "ncm".to_owned()),
            ComponentProbe::new(HealthState::Ready, "airplay".to_owned()),
            ComponentProbe::new(HealthState::Ready, "iap2".to_owned()),
            ComponentProbe::new(HealthState::Ready, "start".to_owned()),
            ProjectionCapabilities {
                control: true,
                video: false,
                audio: false,
                microphone: false,
                input: true,
            },
        );
        assert_eq!(probe.state, HealthState::NotReady);

        let probe = projection_probe(
            ComponentProbe::new(HealthState::Ready, "carrier".to_owned()),
            ComponentProbe::new(HealthState::Ready, "ncm".to_owned()),
            ComponentProbe::new(HealthState::Ready, "airplay".to_owned()),
            ComponentProbe::new(HealthState::Ready, "iap2".to_owned()),
            ComponentProbe::new(HealthState::Ready, "start".to_owned()),
            ProjectionCapabilities {
                control: true,
                video: true,
                audio: false,
                microphone: false,
                input: true,
            },
        );
        assert_eq!(probe.state, HealthState::Ready);
        assert!(probe.detail.contains("optional audio output is disabled"));

        let probe = projection_probe(
            ComponentProbe::new(HealthState::Ready, "carrier".to_owned()),
            ComponentProbe::new(HealthState::Ready, "ncm".to_owned()),
            ComponentProbe::new(HealthState::Ready, "airplay".to_owned()),
            ComponentProbe::new(HealthState::Ready, "iap2".to_owned()),
            ComponentProbe::new(HealthState::Ready, "start".to_owned()),
            ProjectionCapabilities {
                control: true,
                video: true,
                audio: true,
                microphone: true,
                input: true,
            },
        );
        assert_eq!(probe.state, HealthState::Ready);
    }

    #[test]
    fn identification_and_start_gates_are_diagnosed_in_order() {
        let (_directory, mut config) = experimental_config();
        config.iap2.enable_live_session = true;
        let mut runtime = WiredCarkitRuntime::from_config(&config).expect("runtime");
        let probe = runtime.probe();
        assert!(
            probe
                .authentication_detail
                .contains("platform accessory identity")
        );

        config.iap2.accessory_name = Some("Prelude".into());
        config.iap2.model_identifier = Some("Prelude".into());
        config.iap2.manufacturer = Some("Vanta Labs".into());
        config.iap2.serial_number = Some("serial".into());
        config.iap2.firmware_version = Some("0.1.0".into());
        config.iap2.hardware_version = Some("rev-a".into());
        runtime = WiredCarkitRuntime::from_config(&config).expect("runtime");
        let probe = runtime.probe();
        assert!(
            !probe
                .authentication_detail
                .contains("platform accessory identity")
        );
        assert!(probe.session_detail.contains("link-local IPv6"));
    }

    #[test]
    fn runtime_debug_redacts_configured_identifiers() {
        let (_directory, mut config) = experimental_config();
        config.iap2.serial_number = Some("sensitive-serial".into());
        let runtime = WiredCarkitRuntime::from_config(&config).expect("runtime");
        let debug = format!("{runtime:?}");
        assert!(!debug.contains("sensitive-serial"));
    }

    #[test]
    fn bounded_stop_cancels_worker_then_disconnects_and_restores() {
        let cancellation = CancellationToken::default();
        let worker_cancellation = cancellation.clone();
        let disconnected = Arc::new(AtomicBool::new(false));
        let restored = Arc::new(AtomicBool::new(false));
        let worker_disconnected = Arc::clone(&disconnected);
        let worker_restored = Arc::clone(&restored);
        let handle = std::thread::spawn(move || {
            while !worker_cancellation.is_cancelled() {
                std::thread::sleep(Duration::from_millis(2));
            }
            worker_disconnected.store(true, Ordering::Release);
            worker_restored.store(true, Ordering::Release);
        });

        let started = Instant::now();
        cancel_and_join(cancellation, handle).expect("bounded worker stop");
        assert!(started.elapsed() < Duration::from_millis(250));
        assert!(disconnected.load(Ordering::Acquire));
        assert!(restored.load(Ordering::Acquire));
    }

    #[test]
    fn declared_default_worker_stop_bound_covers_every_blocking_stage_and_restore() {
        let config = DaemonConfig::default();
        let timeouts = prelude_iap2::SessionTimeouts::default();
        assert!(timeouts.link_negotiation <= MAX_IAP2_BLOCKING_STAGE);
        assert!(timeouts.setup <= MAX_IAP2_BLOCKING_STAGE);
        assert!(timeouts.availability <= MAX_IAP2_BLOCKING_STAGE);
        assert_eq!(
            configured_worker_stop_bound(&config),
            DEFAULT_MAX_WORKER_STOP_WAIT
        );
    }

    #[test]
    fn live_image_reconnect_policy_requires_a_48_second_stop_bound() {
        let mut config = DaemonConfig::default();
        config.usb.configuration_reconnect_timeout_ms = 15_000;
        config.usb.mux_io_timeout_ms = 5_000;
        assert_eq!(
            configured_worker_stop_bound(&config),
            Duration::from_secs(48)
        );
    }
}
