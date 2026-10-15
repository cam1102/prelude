use std::{
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{
    CancellationToken, Config6Activator, Config6Policy, Config6Timeouts, DeviceIdentity, Error,
    FirstTimePairingPolicy, IphoneCandidate, MuxHost, PairRecordStore, Result, RusbBulkTransport,
    lockdown::{LockdownServiceWire, LockdownSession, start_carkit_service},
    tls::TlsMuxStream,
};

pub const CARKIT_SERVICE: &str = "com.apple.carkit.service";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WiredCarrierConfig {
    pub config6_policy: Config6Policy,
    pub config6_timeouts: Config6Timeouts,
    pub mux_handshake_timeout: Duration,
    pub mux_io_timeout: Duration,
    pub tcp_connect_timeout: Duration,
    pub pairing_records_directory: PathBuf,
    pub first_time_pairing: FirstTimePairingPolicy,
    pub allow_private_lockdown_service: bool,
    pub enable_syslog_tap: bool,
}

impl Default for WiredCarrierConfig {
    fn default() -> Self {
        Self {
            config6_policy: Config6Policy::default(),
            config6_timeouts: Config6Timeouts::default(),
            mux_handshake_timeout: Duration::from_secs(3),
            mux_io_timeout: Duration::from_millis(250),
            tcp_connect_timeout: Duration::from_secs(5),
            pairing_records_directory: PathBuf::from("/var/lib/lockdown"),
            first_time_pairing: FirstTimePairingPolicy::default(),
            allow_private_lockdown_service: false,
            enable_syslog_tap: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WiredCarrier {
    config: WiredCarrierConfig,
}

impl WiredCarrier {
    pub fn new(config: WiredCarrierConfig) -> Result<Self> {
        if config.mux_handshake_timeout.is_zero()
            || config.mux_io_timeout.is_zero()
            || config.tcp_connect_timeout.is_zero()
            || config.first_time_pairing.dialog_timeout.is_zero()
            || config.first_time_pairing.retry_interval.is_zero()
        {
            return Err(Error::InvalidMux(
                "wired carrier and pairing timeouts must be non-zero".to_owned(),
            ));
        }
        if config.first_time_pairing.retry_interval > config.first_time_pairing.dialog_timeout {
            return Err(Error::InvalidPairingRecord(
                "pairing retry interval exceeds dialog timeout".to_owned(),
            ));
        }
        if !config.pairing_records_directory.is_absolute() {
            return Err(Error::InvalidPairingRecord(
                "pairing record directory must be absolute".to_owned(),
            ));
        }
        Ok(Self { config })
    }

    pub fn connect(&self, candidate: &IphoneCandidate) -> Result<CarkitByteStream> {
        self.connect_cancellable(candidate, &CancellationToken::default())
    }

    pub fn connect_cancellable(
        &self,
        candidate: &IphoneCandidate,
        cancellation: &CancellationToken,
    ) -> Result<CarkitByteStream> {
        if !self.config.allow_private_lockdown_service {
            return Err(Error::Unsupported(
                "private Lockdown carkit service is disabled by policy",
            ));
        }
        let previous_configuration = candidate.active_configuration;
        let mut activator = Config6Activator::new(
            crate::config6::LibUsbConfig6Backend::default(),
            self.config.config6_timeouts,
        );
        let active = activator.activate(candidate, self.config.config6_policy)?;
        let result = (|| {
            cancellation.check("opening the configuration-6 carkit carrier")?;
            let device_identifier =
                active
                    .identity
                    .serial_number
                    .as_deref()
                    .ok_or(Error::NotReady {
                        component: "wired carkit carrier",
                        state: "device_serial_unavailable",
                    })?;
            let transport = RusbBulkTransport::open(&active)?;
            let usbmux_interface_number = transport.interface_number();
            let host = MuxHost::start(
                transport,
                self.config.mux_handshake_timeout,
                self.config.mux_io_timeout,
            )?;
            cancellation.check("starting the configuration-6 usbmux carrier")?;
            let store = PairRecordStore::new(&self.config.pairing_records_directory);
            let (lockdown, service, syslog) = start_carkit_service(
                &host,
                device_identifier,
                &store,
                self.config.tcp_connect_timeout,
                self.config.first_time_pairing,
                self.config.enable_syslog_tap,
                cancellation,
            )?;
            let syslog_tap = syslog.map(SyslogTap::start).transpose()?;
            Ok(CarkitByteStream {
                lockdown,
                service,
                active_candidate: active.clone(),
                previous_configuration,
                usbmux_interface_number,
                syslog_tap,
            })
        })();
        if result.is_err() {
            activator.restore(&active, previous_configuration, self.config.config6_policy)?;
        }
        result
    }

    pub fn disconnect(&self, stream: CarkitByteStream) -> Result<()> {
        let CarkitByteStream {
            lockdown,
            service,
            active_candidate,
            previous_configuration,
            usbmux_interface_number: _,
            syslog_tap: _,
        } = stream;
        drop(service);
        drop(lockdown);
        let mut activator = Config6Activator::new(
            crate::config6::LibUsbConfig6Backend::default(),
            self.config.config6_timeouts,
        );
        activator.restore(
            &active_candidate,
            previous_configuration,
            self.config.config6_policy,
        )?;
        Ok(())
    }

    pub fn config(&self) -> &WiredCarrierConfig {
        &self.config
    }
}

struct SyslogTap {
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl SyslogTap {
    fn start(mut stream: LockdownServiceWire<RusbBulkTransport>) -> Result<Self> {
        stream.set_read_timeout(Duration::from_millis(100))?;
        tracing::info!(target: "prelude::iphone_syslog", "iPhone syslog tap started");
        let cancelled = Arc::new(AtomicBool::new(false));
        let thread_cancelled = Arc::clone(&cancelled);
        let worker = thread::Builder::new()
            .name("prelude-iphone-syslog".to_owned())
            .spawn(move || run_syslog_tap(&mut stream, &thread_cancelled))
            .map_err(|error| Error::io("starting iPhone syslog tap", error))?;
        Ok(Self {
            cancelled,
            worker: Some(worker),
        })
    }
}

impl Drop for SyslogTap {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_syslog_tap(stream: &mut LockdownServiceWire<RusbBulkTransport>, cancelled: &AtomicBool) {
    const MAX_BUFFER: usize = 1024 * 1024;
    const KEYWORDS: &[&str] = &[
        "carplay",
        "carkit",
        "airplay",
        "sharingd",
        "mediaremoted",
        "mediaserverd",
        "iap2",
        "iaptransport",
        "externalaccessory",
        "accessory",
        "screenreceiver",
        "iap",
        "avconference",
        "rapport",
        "wirelessproxd",
        "stream",
        "record",
        "setup",
        "teardown",
        "disconnect",
        "timeout",
        "terminat",
        "reject",
        "failed",
        "failure",
    ];
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut bytes_received = 0_u64;
    let mut lines_received = 0_u64;
    let mut lines_matched = 0_u64;
    while !cancelled.load(Ordering::Acquire) {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(length) => {
                bytes_received = bytes_received.saturating_add(length as u64);
                buffer.extend_from_slice(&chunk[..length]);
                while let Some(end) = buffer.windows(2).position(|window| window == b"\n\0") {
                    lines_received = lines_received.saturating_add(1);
                    let line = String::from_utf8_lossy(&buffer[..end]);
                    let lowercase = line.to_ascii_lowercase();
                    if KEYWORDS.iter().any(|keyword| lowercase.contains(keyword)) {
                        lines_matched = lines_matched.saturating_add(1);
                        tracing::info!(target: "prelude::iphone_syslog", message = %line);
                    }
                    buffer.drain(..end + 2);
                }
                if buffer.len() > MAX_BUFFER {
                    buffer.clear();
                    tracing::warn!(target: "prelude::iphone_syslog", "discarded an oversized partial iPhone syslog line");
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => {
                tracing::warn!(target: "prelude::iphone_syslog", %error, "iPhone syslog tap ended");
                break;
            }
        }
    }
    tracing::info!(
        target: "prelude::iphone_syslog",
        bytes_received,
        lines_received,
        lines_matched,
        "iPhone syslog tap stopped"
    );
}

pub struct CarkitByteStream {
    lockdown: LockdownSession<RusbBulkTransport>,
    service: TlsMuxStream<RusbBulkTransport>,
    active_candidate: IphoneCandidate,
    previous_configuration: Option<u8>,
    usbmux_interface_number: u8,
    syslog_tap: Option<SyslogTap>,
}

impl std::fmt::Debug for CarkitByteStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CarkitByteStream")
            .field("device", self.device())
            .field("tls", &true)
            .field("usbmux_interface_number", &self.usbmux_interface_number)
            .field("syslog_tap", &self.syslog_tap.is_some())
            .field("alive", &self.is_alive())
            .finish_non_exhaustive()
    }
}

impl CarkitByteStream {
    pub fn device(&self) -> &DeviceIdentity {
        &self.active_candidate.identity
    }

    pub fn active_candidate(&self) -> &IphoneCandidate {
        &self.active_candidate
    }

    pub fn is_alive(&self) -> bool {
        self.lockdown.is_alive() && self.service.get_ref().is_alive()
    }

    pub fn usbmux_interface_number(&self) -> u8 {
        self.usbmux_interface_number
    }

    pub fn set_read_timeout(&mut self, timeout: Duration) -> Result<()> {
        self.service.get_mut().set_read_timeout(timeout)
    }

    pub fn set_write_timeout(&mut self, timeout: Duration) -> Result<()> {
        self.service.get_mut().set_write_timeout(timeout)
    }

    pub fn read_timeout(&self) -> Duration {
        self.service.get_ref().read_timeout()
    }

    pub fn write_timeout(&self) -> Duration {
        self.service.get_ref().write_timeout()
    }

    pub fn reset_read_timeout(&mut self) {
        self.service.get_mut().reset_read_timeout();
    }

    pub fn reset_write_timeout(&mut self) {
        self.service.get_mut().reset_write_timeout();
    }
}

impl Read for CarkitByteStream {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        self.service.read(output)
    }
}

impl Write for CarkitByteStream {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.service.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.service.flush()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockdownTrust {
    Unknown,
    Paired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CarkitState {
    DeviceSelected,
    UsbmuxReady,
    LockdownReady,
    ServiceReady,
    Closed,
    Failed,
}

impl CarkitState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DeviceSelected => "device_selected",
            Self::UsbmuxReady => "usbmux_ready",
            Self::LockdownReady => "lockdown_ready",
            Self::ServiceReady => "service_ready",
            Self::Closed => "closed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CarkitEvent {
    UsbmuxConnected,
    LockdownConnected { trust: LockdownTrust },
    ServiceOpened { name: String, tls: bool },
    Disconnected,
    Fatal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CarkitPolicy {
    pub allow_private_lockdown_service: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarkitSession {
    state: CarkitState,
    policy: CarkitPolicy,
}

impl CarkitSession {
    pub fn new(policy: CarkitPolicy) -> Self {
        Self {
            state: CarkitState::DeviceSelected,
            policy,
        }
    }

    pub fn state(&self) -> CarkitState {
        self.state
    }

    pub fn apply(&mut self, event: CarkitEvent) -> Result<CarkitState> {
        if matches!(event, CarkitEvent::Fatal) {
            self.state = CarkitState::Failed;
            return Ok(self.state);
        }
        if matches!(event, CarkitEvent::Disconnected) {
            self.state = CarkitState::Closed;
            return Ok(self.state);
        }
        let next = match (self.state, event) {
            (CarkitState::DeviceSelected, CarkitEvent::UsbmuxConnected) => CarkitState::UsbmuxReady,
            (
                CarkitState::UsbmuxReady,
                CarkitEvent::LockdownConnected {
                    trust: LockdownTrust::Paired,
                },
            ) => CarkitState::LockdownReady,
            (CarkitState::UsbmuxReady, CarkitEvent::LockdownConnected { .. }) => {
                return Err(Error::NotReady {
                    component: "lockdown",
                    state: "unpaired",
                });
            }
            (CarkitState::LockdownReady, CarkitEvent::ServiceOpened { name, tls }) => {
                if !self.policy.allow_private_lockdown_service {
                    return Err(Error::Unsupported(
                        "private Lockdown carkit service is disabled by policy",
                    ));
                }
                if name != CARKIT_SERVICE {
                    return Err(Error::Unsupported("unexpected Lockdown service"));
                }
                if !tls {
                    return Err(Error::NotReady {
                        component: "carkit service",
                        state: "unencrypted",
                    });
                }
                CarkitState::ServiceReady
            }
            (state, _) => {
                return Err(Error::NotReady {
                    component: "carkit session",
                    state: state.as_str(),
                });
            }
        };
        self.state = next;
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_service_is_disabled_by_default() {
        let mut session = CarkitSession::new(CarkitPolicy::default());
        session.apply(CarkitEvent::UsbmuxConnected).expect("usbmux");
        session
            .apply(CarkitEvent::LockdownConnected {
                trust: LockdownTrust::Paired,
            })
            .expect("lockdown");
        let error = session
            .apply(CarkitEvent::ServiceOpened {
                name: CARKIT_SERVICE.to_owned(),
                tls: true,
            })
            .expect_err("policy must reject private service");
        assert!(matches!(error, Error::Unsupported(_)));
        assert_eq!(session.state(), CarkitState::LockdownReady);
    }

    #[test]
    fn trusted_tls_service_reaches_ready_when_explicitly_enabled() {
        let mut session = CarkitSession::new(CarkitPolicy {
            allow_private_lockdown_service: true,
        });
        session.apply(CarkitEvent::UsbmuxConnected).expect("usbmux");
        session
            .apply(CarkitEvent::LockdownConnected {
                trust: LockdownTrust::Paired,
            })
            .expect("lockdown");
        let state = session
            .apply(CarkitEvent::ServiceOpened {
                name: CARKIT_SERVICE.to_owned(),
                tls: true,
            })
            .expect("service");
        assert_eq!(state, CarkitState::ServiceReady);
    }

    #[test]
    fn untrusted_lockdown_is_not_ready() {
        let mut session = CarkitSession::new(CarkitPolicy::default());
        session.apply(CarkitEvent::UsbmuxConnected).expect("usbmux");
        assert!(matches!(
            session.apply(CarkitEvent::LockdownConnected {
                trust: LockdownTrust::Unknown,
            }),
            Err(Error::NotReady { .. })
        ));
    }
}
