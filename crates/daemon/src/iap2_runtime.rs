use std::{
    fmt,
    io::{self, Read, Write},
    net::{IpAddr, Ipv6Addr},
    sync::{Arc, Mutex},
    time::Duration,
};

use prelude_iap2::{
    AuthProvider, CarkitIo, SessionError, WiredCarPlayStartRequest, WiredCarkitSession,
    csm::identification::IdentificationInformation,
};
use prelude_usb_carkit::{CarkitByteStream, Error as CarrierError};
use thiserror::Error;

use crate::Iap2Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NotReadyGate {
    pub state: &'static str,
    pub detail: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SessionStartPlan {
    Ready(WiredCarPlayStartRequest),
    NotReady(NotReadyGate),
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ReceiverStartMetadata {
    pub service_port: u16,
    pub device_identifier: String,
    pub public_key: String,
    pub source_version: String,
}

impl fmt::Debug for ReceiverStartMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReceiverStartMetadata")
            .field("service_port", &self.service_port)
            .field("device_identifier", &"[redacted]")
            .field("public_key", &"[redacted]")
            .field("source_version", &self.source_version)
            .finish()
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartDisposition {
    Sent,
    NotReady(NotReadyGate),
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriverStage {
    Setup,
    Availability,
    Start,
}

impl Iap2Config {
    pub(crate) fn identification(
        &self,
        carplay_interface_number: u8,
    ) -> std::result::Result<IdentificationInformation, NotReadyGate> {
        Ok(IdentificationInformation::wired_carplay(
            required(
                &self.accessory_name,
                "accessory_name_missing",
                "iap2.accessory_name is not configured",
            )?,
            required(
                &self.model_identifier,
                "model_identifier_missing",
                "iap2.model_identifier is not configured",
            )?,
            required(
                &self.manufacturer,
                "manufacturer_missing",
                "iap2.manufacturer is not configured",
            )?,
            required(
                &self.serial_number,
                "serial_number_missing",
                "iap2.serial_number is not configured",
            )?,
            required(
                &self.firmware_version,
                "firmware_version_missing",
                "iap2.firmware_version is not configured",
            )?,
            required(
                &self.hardware_version,
                "hardware_version_missing",
                "iap2.hardware_version is not configured",
            )?,
            carplay_interface_number,
        ))
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn wireless_identification(
        &self,
        bluetooth_mac_address: [u8; 6],
        access_point_ssid: &str,
    ) -> std::result::Result<IdentificationInformation, NotReadyGate> {
        let wired = self.identification(0)?;
        Ok(IdentificationInformation::wireless_carplay(
            wired.name,
            wired.model_identifier,
            wired.manufacturer,
            wired.serial_number,
            wired.firmware_version,
            wired.hardware_version,
            bluetooth_mac_address,
            access_point_ssid,
        ))
    }

    pub(crate) fn first_identification_gate(&self) -> Option<NotReadyGate> {
        self.identification(0).err()
    }

    pub(crate) fn start_plan(
        &self,
        ncm_link_local: Option<Ipv6Addr>,
        receiver: Option<&ReceiverStartMetadata>,
    ) -> SessionStartPlan {
        let Some(ncm_link_local) = ncm_link_local else {
            return SessionStartPlan::NotReady(NotReadyGate {
                state: "ncm_link_local_unavailable",
                detail: "the topology-associated live NCM interface has no verified link-local IPv6 address",
            });
        };
        if !ncm_link_local.is_unicast_link_local() {
            return SessionStartPlan::NotReady(NotReadyGate {
                state: "ncm_address_policy_rejected",
                detail: "the observed NCM address is not a link-local IPv6 address",
            });
        }
        let Some(receiver) = receiver else {
            return SessionStartPlan::NotReady(NotReadyGate {
                state: "airplay_receiver_unavailable",
                detail: "a topology-bound AirPlay receiver has not supplied live start metadata",
            });
        };
        if receiver.service_port == 0
            || receiver.device_identifier.is_empty()
            || receiver.device_identifier.as_bytes().contains(&0)
            || receiver.public_key.is_empty()
            || receiver.public_key.as_bytes().contains(&0)
            || receiver.source_version.is_empty()
            || receiver.source_version.as_bytes().contains(&0)
        {
            return SessionStartPlan::NotReady(NotReadyGate {
                state: "airplay_receiver_metadata_invalid",
                detail: "the live AirPlay receiver supplied invalid start metadata",
            });
        }
        SessionStartPlan::Ready(WiredCarPlayStartRequest {
            ip_addresses: vec![IpAddr::V6(ncm_link_local)],
            port: receiver.service_port,
            device_identifier: receiver.device_identifier.clone(),
            public_key: Some(receiver.public_key.clone()),
            source_version: receiver.source_version.clone(),
        })
    }
}

fn required(
    value: &Option<String>,
    state: &'static str,
    detail: &'static str,
) -> std::result::Result<String, NotReadyGate> {
    value.clone().ok_or(NotReadyGate { state, detail })
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) trait CarrierStream: Read + Write + Send + 'static {
    fn update_read_timeout(&mut self, timeout: Duration) -> Result<(), CarrierError>;
    fn update_write_timeout(&mut self, timeout: Duration) -> Result<(), CarrierError>;
    fn restore_read_timeout(&mut self);
    fn restore_write_timeout(&mut self);
    fn is_carrier_alive(&self) -> bool;
}

impl CarrierStream for CarkitByteStream {
    fn update_read_timeout(&mut self, timeout: Duration) -> Result<(), CarrierError> {
        self.set_read_timeout(timeout)
    }

    fn update_write_timeout(&mut self, timeout: Duration) -> Result<(), CarrierError> {
        self.set_write_timeout(timeout)
    }

    fn restore_read_timeout(&mut self) {
        self.reset_read_timeout();
    }

    fn restore_write_timeout(&mut self) {
        self.reset_write_timeout();
    }

    fn is_carrier_alive(&self) -> bool {
        self.is_alive()
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct SharedStream<S>(Arc<Mutex<Option<S>>>);

impl<S> Clone for SharedStream<S> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct CarkitStreamAdapter<S: CarrierStream> {
    shared: SharedStream<S>,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct StreamRecovery<S> {
    shared: SharedStream<S>,
}

impl<S: CarrierStream> fmt::Debug for CarkitStreamAdapter<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CarkitStreamAdapter")
            .field("alive", &self.is_alive())
            .finish_non_exhaustive()
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl<S: CarrierStream> CarkitStreamAdapter<S> {
    pub(crate) fn new(stream: S) -> Self {
        Self {
            shared: SharedStream(Arc::new(Mutex::new(Some(stream)))),
        }
    }

    pub(crate) fn recovery_handle(&self) -> StreamRecovery<S> {
        StreamRecovery {
            shared: self.shared.clone(),
        }
    }

    pub(crate) fn take_inner(self) -> io::Result<S> {
        take_stream(&self.shared)
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.shared
            .0
            .lock()
            .ok()
            .and_then(|stream| stream.as_ref().map(CarrierStream::is_carrier_alive))
            .unwrap_or(false)
    }

    pub(crate) fn reset_timeouts(&mut self) -> io::Result<()> {
        let read = CarkitIo::set_read_timeout(self, None);
        let write = CarkitIo::set_write_timeout(self, None);
        read.and(write)
    }

    fn with_stream_mut<T>(&mut self, operation: impl FnOnce(&mut S) -> T) -> io::Result<T> {
        let mut stream = self
            .shared
            .0
            .lock()
            .map_err(|_| io::Error::other("carkit stream lock poisoned"))?;
        stream
            .as_mut()
            .map(operation)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "carkit stream recovered"))
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl<S> StreamRecovery<S> {
    pub(crate) fn take(self) -> io::Result<S> {
        take_stream(&self.shared)
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn take_stream<S>(shared: &SharedStream<S>) -> io::Result<S> {
    shared
        .0
        .lock()
        .map_err(|_| io::Error::other("carkit stream lock poisoned"))?
        .take()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "carkit stream already recovered",
            )
        })
}

impl<S: CarrierStream> Read for CarkitStreamAdapter<S> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.with_stream_mut(|stream| stream.read(output))?
    }
}

impl<S: CarrierStream> Write for CarkitStreamAdapter<S> {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.with_stream_mut(|stream| stream.write(input))?
    }

    fn flush(&mut self) -> io::Result<()> {
        self.with_stream_mut(Write::flush)?
    }
}

impl<S: CarrierStream> CarkitIo for CarkitStreamAdapter<S> {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.with_stream_mut(|stream| match timeout {
            Some(timeout) => stream
                .update_read_timeout(timeout)
                .map_err(|_| io::Error::other("failed to update carkit read deadline")),
            None => {
                stream.restore_read_timeout();
                Ok(())
            }
        })?
    }

    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.with_stream_mut(|stream| match timeout {
            Some(timeout) => stream
                .update_write_timeout(timeout)
                .map_err(|_| io::Error::other("failed to update carkit write deadline")),
            None => {
                stream.restore_write_timeout();
                Ok(())
            }
        })?
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Error)]
pub(crate) enum DriverError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("failed to restore the carkit stream's bounded default deadlines: {0}")]
    TimeoutReset(io::Error),
    #[error("wired iAP2 session was cancelled before {0:?}")]
    Cancelled(DriverStage),
}

#[cfg(test)]
pub(crate) fn drive_session<S, P>(
    session: &mut WiredCarkitSession<CarkitStreamAdapter<S>, P>,
    start_plan: &SessionStartPlan,
) -> Result<StartDisposition, DriverError>
where
    S: CarrierStream,
    P: AuthProvider,
{
    drive_session_with_observer(session, start_plan, |_| {}, || false)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn drive_session_with_observer<S, P>(
    session: &mut WiredCarkitSession<CarkitStreamAdapter<S>, P>,
    start_plan: &SessionStartPlan,
    mut observer: impl FnMut(DriverStage),
    mut cancelled: impl FnMut() -> bool,
) -> Result<StartDisposition, DriverError>
where
    S: CarrierStream,
    P: AuthProvider,
{
    let result = (|| {
        observer(DriverStage::Setup);
        if cancelled() {
            return Err(DriverError::Cancelled(DriverStage::Setup));
        }
        session.run_setup()?;
        observer(DriverStage::Availability);
        if cancelled() {
            return Err(DriverError::Cancelled(DriverStage::Availability));
        }
        session.wait_for_wired_carplay()?;
        match start_plan {
            SessionStartPlan::Ready(request) => {
                observer(DriverStage::Start);
                if cancelled() {
                    return Err(DriverError::Cancelled(DriverStage::Start));
                }
                session.start_wired_carplay(request)?;
                Ok(StartDisposition::Sent)
            }
            SessionStartPlan::NotReady(gate) => Ok(StartDisposition::NotReady(*gate)),
        }
    })();
    let reset = session.stream_mut().reset_timeouts();
    match (result, reset) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(DriverError::TimeoutReset(error)),
        (Ok(disposition), Ok(())) => Ok(disposition),
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn recover_session_stream<S, P>(
    session: WiredCarkitSession<CarkitStreamAdapter<S>, P>,
) -> io::Result<S>
where
    S: CarrierStream,
    P: AuthProvider,
{
    let (adapter, provider) = session.into_parts();
    drop(provider);
    adapter.take_inner()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
        thread,
    };

    use prelude_iap2::{
        CONTROL_SESSION_ID, ControlBits, LinkPacket, LinkSynchronization, SessionPhase,
        SessionTimeouts,
        csm::{
            CsmFrame,
            auth::AuthenticationMessage,
            carplay::{CarPlayAvailability, WiredCarPlayAvailability},
            identification::{IDENTIFICATION_ACCEPTED, START_IDENTIFICATION},
        },
    };
    use prelude_mfi::{CERTIFICATE_SERIAL_LEN, CHALLENGE_LEN, RESPONSE_LEN};

    use super::*;

    const INITIAL_ACCESSORY_SEQUENCE: u8 = 99;

    #[derive(Debug)]
    enum ReadStep {
        Data(VecDeque<u8>),
        Timeout,
    }

    #[derive(Debug, Default)]
    struct FakeState {
        written: Vec<u8>,
        read_timeouts: Vec<Option<Duration>>,
        write_timeouts: Vec<Option<Duration>>,
    }

    #[derive(Debug)]
    struct FakeCarrier {
        reads: VecDeque<ReadStep>,
        state: Arc<Mutex<FakeState>>,
        read_timeout: Duration,
        alive: bool,
    }

    impl FakeCarrier {
        fn new(steps: impl IntoIterator<Item = ReadStep>) -> (Self, Arc<Mutex<FakeState>>) {
            let state = Arc::new(Mutex::new(FakeState::default()));
            (
                Self {
                    reads: steps.into_iter().collect(),
                    state: Arc::clone(&state),
                    read_timeout: Duration::from_millis(1),
                    alive: true,
                },
                state,
            )
        }
    }

    impl Read for FakeCarrier {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            loop {
                match self.reads.front_mut() {
                    Some(ReadStep::Data(data)) if data.is_empty() => {
                        self.reads.pop_front();
                    }
                    Some(ReadStep::Data(data)) => {
                        let count = output.len().min(data.len());
                        for destination in &mut output[..count] {
                            *destination = data.pop_front().ok_or_else(|| {
                                io::Error::new(io::ErrorKind::UnexpectedEof, "empty fixture")
                            })?;
                        }
                        return Ok(count);
                    }
                    Some(ReadStep::Timeout) => {
                        self.reads.pop_front();
                        thread::sleep(self.read_timeout.min(Duration::from_millis(2)));
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "fixture timeout"));
                    }
                    None => {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "fixture exhausted"));
                    }
                }
            }
        }
    }

    impl Write for FakeCarrier {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            self.state
                .lock()
                .expect("state")
                .written
                .extend_from_slice(input);
            Ok(input.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl CarrierStream for FakeCarrier {
        fn update_read_timeout(&mut self, timeout: Duration) -> Result<(), CarrierError> {
            self.read_timeout = timeout;
            self.state
                .lock()
                .expect("state")
                .read_timeouts
                .push(Some(timeout));
            Ok(())
        }

        fn update_write_timeout(&mut self, timeout: Duration) -> Result<(), CarrierError> {
            self.state
                .lock()
                .expect("state")
                .write_timeouts
                .push(Some(timeout));
            Ok(())
        }

        fn restore_read_timeout(&mut self) {
            self.state.lock().expect("state").read_timeouts.push(None);
        }

        fn restore_write_timeout(&mut self) {
            self.state.lock().expect("state").write_timeouts.push(None);
        }

        fn is_carrier_alive(&self) -> bool {
            self.alive
        }
    }

    #[derive(Debug, Default)]
    struct FakeProvider;

    impl AuthProvider for FakeProvider {
        type Error = io::Error;

        fn certificate(&mut self) -> Result<Vec<u8>, Self::Error> {
            Ok(vec![0x30; 607])
        }

        fn certificate_serial(&mut self) -> Result<[u8; CERTIFICATE_SERIAL_LEN], Self::Error> {
            Ok([0x12; CERTIFICATE_SERIAL_LEN])
        }

        fn sign(
            &mut self,
            _challenge: &[u8; CHALLENGE_LEN],
        ) -> Result<[u8; RESPONSE_LEN], Self::Error> {
            Ok([0x34; RESPONSE_LEN])
        }
    }

    fn identification() -> IdentificationInformation {
        IdentificationInformation::wired_carplay(
            "Prelude",
            "Prelude",
            "Vanta Labs",
            "PR-0001",
            "0.1.0",
            "rev-a",
            1,
        )
    }

    fn start_plan() -> SessionStartPlan {
        SessionStartPlan::Ready(WiredCarPlayStartRequest {
            ip_addresses: vec!["fe80::1".parse().expect("address")],
            port: 7000,
            device_identifier: "00:11:22:33:44:55".into(),
            public_key: Some("receiver-public-key".into()),
            source_version: "1.0".into(),
        })
    }

    #[test]
    fn configured_or_non_link_local_ncm_addresses_cannot_reach_start() {
        let config = Iap2Config::default();
        let receiver = ReceiverStartMetadata {
            service_port: 7000,
            device_identifier: "02:11:22:33:44:55".to_owned(),
            public_key: "receiver-public-key".to_owned(),
            source_version: "1.0".to_owned(),
        };
        assert!(matches!(
            config.start_plan(None, Some(&receiver)),
            SessionStartPlan::NotReady(NotReadyGate {
                state: "ncm_link_local_unavailable",
                ..
            })
        ));
        assert!(matches!(
            config.start_plan(Some("2001:db8::1".parse().expect("IPv6")), Some(&receiver)),
            SessionStartPlan::NotReady(NotReadyGate {
                state: "ncm_address_policy_rejected",
                ..
            })
        ));
        assert!(matches!(
            config.start_plan(Some("fe80::1234".parse().expect("IPv6")), None),
            SessionStartPlan::NotReady(NotReadyGate {
                state: "airplay_receiver_unavailable",
                ..
            })
        ));
        let SessionStartPlan::Ready(request) =
            config.start_plan(Some("fe80::1234".parse().expect("IPv6")), Some(&receiver))
        else {
            panic!("verified link-local address must reach a complete start plan");
        };
        assert_eq!(
            request.ip_addresses,
            vec!["fe80::1234".parse::<IpAddr>().expect("IP")]
        );
    }

    fn device_packet(sequence: u8, frame: CsmFrame) -> Vec<u8> {
        LinkPacket::with_payload(
            ControlBits::ACK,
            sequence,
            INITIAL_ACCESSORY_SEQUENCE,
            CONTROL_SESSION_ID,
            frame.encode().expect("CSM fixture"),
        )
        .encode()
        .expect("link fixture")
        .to_vec()
    }

    fn peer_synchronization(sequence: u8) -> Vec<u8> {
        LinkPacket::with_payload(
            ControlBits::SYN | ControlBits::ACK,
            sequence,
            INITIAL_ACCESSORY_SEQUENCE,
            0,
            LinkSynchronization::wired_carplay()
                .encode()
                .expect("LSP fixture"),
        )
        .encode()
        .expect("SYN fixture")
        .to_vec()
    }

    fn successful_input(first_sequence: u8) -> Vec<u8> {
        let mut input = peer_synchronization(first_sequence);
        let mut sequence = first_sequence;
        for frame in [
            AuthenticationMessage::RequestCertificate {
                request_serial_number: false,
            }
            .to_frame(),
            CsmFrame::empty(START_IDENTIFICATION),
            AuthenticationMessage::RequestChallengeResponse([0x56; CHALLENGE_LEN]).to_frame(),
            CsmFrame::empty(IDENTIFICATION_ACCEPTED),
            AuthenticationMessage::Succeeded.to_frame(),
            CarPlayAvailability {
                wired: Some(WiredCarPlayAvailability {
                    available: Some(true),
                    usb_transport_identifier: Some("usb-carkit".into()),
                }),
                wireless: None,
            }
            .to_frame()
            .expect("availability fixture"),
        ] {
            sequence = sequence.wrapping_add(1);
            input.extend_from_slice(&device_packet(sequence, frame));
        }
        input
    }

    fn fast_timeouts() -> SessionTimeouts {
        SessionTimeouts {
            link_negotiation: Duration::from_millis(30),
            link_retry: Duration::from_millis(3),
            setup: Duration::from_millis(30),
            availability: Duration::from_millis(30),
            write: Duration::from_millis(30),
        }
    }

    #[test]
    fn adapter_maps_some_and_none_to_concrete_deadline_hooks() {
        let (carrier, state) = FakeCarrier::new([]);
        let mut adapter = CarkitStreamAdapter::new(carrier);
        CarkitIo::set_read_timeout(&mut adapter, Some(Duration::from_millis(7)))
            .expect("read deadline");
        CarkitIo::set_write_timeout(&mut adapter, Some(Duration::from_millis(9)))
            .expect("write deadline");
        CarkitIo::set_read_timeout(&mut adapter, None).expect("reset read");
        CarkitIo::set_write_timeout(&mut adapter, None).expect("reset write");
        let state = state.lock().expect("state");
        assert_eq!(
            state.read_timeouts,
            vec![Some(Duration::from_millis(7)), None]
        );
        assert_eq!(
            state.write_timeouts,
            vec![Some(Duration::from_millis(9)), None]
        );
    }

    #[test]
    fn scripted_session_runs_link_auth_ident_availability_and_start() {
        let (carrier, state) =
            FakeCarrier::new([ReadStep::Data(successful_input(7).into_iter().collect())]);
        let adapter = CarkitStreamAdapter::new(carrier);
        let mut session = WiredCarkitSession::new_with_timeouts(
            adapter,
            FakeProvider,
            identification(),
            fast_timeouts(),
        )
        .expect("session");
        assert_eq!(
            drive_session(&mut session, &start_plan()).expect("complete session"),
            StartDisposition::Sent
        );
        assert_eq!(session.phase(), SessionPhase::CarPlayStartSent);
        let state = state.lock().expect("state");
        assert!(state.written.starts_with(&[0xff, 0x55]));
        assert_eq!(state.read_timeouts.last(), Some(&None));
        assert_eq!(state.write_timeouts.last(), Some(&None));
    }

    #[test]
    fn link_timeout_retries_then_succeeds_and_resets_deadlines() {
        let (carrier, state) = FakeCarrier::new([
            ReadStep::Timeout,
            ReadStep::Data(successful_input(7).into_iter().collect()),
        ]);
        let adapter = CarkitStreamAdapter::new(carrier);
        let mut session = WiredCarkitSession::new_with_timeouts(
            adapter,
            FakeProvider,
            identification(),
            fast_timeouts(),
        )
        .expect("session");
        drive_session(&mut session, &start_plan()).expect("retry succeeds");
        let state = state.lock().expect("state");
        assert!(
            state
                .written
                .windows(2)
                .filter(|bytes| *bytes == [0xff, 0x5a])
                .count()
                >= 2
        );
        assert_eq!(state.read_timeouts.last(), Some(&None));
    }

    #[test]
    fn malformed_stream_fails_closed_and_still_resets_deadlines() {
        let (carrier, state) = FakeCarrier::new([ReadStep::Data(
            vec![0xff, 0x5a, 0, 1, 0].into_iter().collect(),
        )]);
        let adapter = CarkitStreamAdapter::new(carrier);
        let mut session = WiredCarkitSession::new_with_timeouts(
            adapter,
            FakeProvider,
            identification(),
            fast_timeouts(),
        )
        .expect("session");
        assert!(matches!(
            drive_session(&mut session, &start_plan()),
            Err(DriverError::Session(_))
        ));
        let state = state.lock().expect("state");
        assert_eq!(state.read_timeouts.last(), Some(&None));
        assert_eq!(state.write_timeouts.last(), Some(&None));
    }

    #[test]
    fn absent_live_ncm_inputs_stop_after_availability_without_start() {
        let (carrier, _state) =
            FakeCarrier::new([ReadStep::Data(successful_input(7).into_iter().collect())]);
        let adapter = CarkitStreamAdapter::new(carrier);
        let mut session = WiredCarkitSession::new_with_timeouts(
            adapter,
            FakeProvider,
            identification(),
            fast_timeouts(),
        )
        .expect("session");
        let gate = NotReadyGate {
            state: "ncm_address_missing",
            detail: "no live address",
        };
        assert_eq!(
            drive_session(&mut session, &SessionStartPlan::NotReady(gate)).expect("ready"),
            StartDisposition::NotReady(gate)
        );
        assert_eq!(session.phase(), SessionPhase::ReadyForCarPlay);
    }

    #[test]
    fn recovery_handle_retains_stream_after_adapter_is_consumed_and_dropped() {
        let (carrier, _state) = FakeCarrier::new([]);
        let adapter = CarkitStreamAdapter::new(carrier);
        let recovery = adapter.recovery_handle();
        drop(adapter);
        assert!(recovery.take().expect("recover stream").alive);
    }

    #[test]
    fn cancelled_worker_stage_resets_and_recovers_stream_for_disconnect() {
        let (carrier, state) = FakeCarrier::new([]);
        let adapter = CarkitStreamAdapter::new(carrier);
        let mut session = WiredCarkitSession::new_with_timeouts(
            adapter,
            FakeProvider,
            identification(),
            fast_timeouts(),
        )
        .expect("session");
        assert!(matches!(
            drive_session_with_observer(&mut session, &start_plan(), |_| {}, || true),
            Err(DriverError::Cancelled(DriverStage::Setup))
        ));
        let mut carrier = recover_session_stream(session).expect("recover cancelled carrier");
        carrier.alive = false;
        assert!(!carrier.is_carrier_alive());
        let state = state.lock().expect("state");
        assert_eq!(state.read_timeouts.last(), Some(&None));
        assert_eq!(state.write_timeouts.last(), Some(&None));
    }
}
