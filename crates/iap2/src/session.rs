use std::{
    collections::VecDeque,
    fmt,
    io::{self, Read, Write},
    net::IpAddr,
    time::{Duration, Instant},
};

use bytes::{Buf, Bytes, BytesMut};
use thiserror::Error;

use crate::{
    auth::{
        AuthAction, AuthError, AuthMachine, AuthProvider, AuthState, CERTIFICATE_RESPONSE_DEADLINE,
        CHALLENGE_RESPONSE_DEADLINE,
    },
    csm::{
        CsmError, CsmFrame,
        auth::{
            AUTHENTICATION_FAILED, AUTHENTICATION_SUCCEEDED, REQUEST_AUTHENTICATION_CERTIFICATE,
            REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE,
        },
        carplay::{
            CARPLAY_AVAILABILITY, CARPLAY_START_SESSION, CarPlayAvailability, CarPlayStartSession,
            DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION, DeviceTransportIdentifiers,
            WiredCarPlayAvailability, WiredCarPlayStart, WirelessCarPlayAvailability,
            WirelessCarPlayStart,
        },
        identification::{
            IDENTIFICATION_ACCEPTED, IDENTIFICATION_REJECTED, IdentificationError,
            IdentificationInformation, START_IDENTIFICATION,
        },
        wifi::{
            ACCESSORY_WIFI_CONFIGURATION_INFORMATION, AccessoryWifiConfiguration,
            REQUEST_ACCESSORY_WIFI_CONFIGURATION_INFORMATION,
        },
    },
    frame::{
        CONTROL_SESSION_ID, ControlBits, DETECT_MARKER, FrameError, LINK_HEADER_LEN, LinkPacket,
        LinkSynchronization,
    },
};

const INITIAL_ACCESSORY_SEQUENCE: u8 = 99;
const DEFAULT_MAX_PACKET_LENGTH: usize = 128;
const MAXIMUM_PEER_DETECT_MARKERS: usize = 4;

pub trait CarkitIo: Read + Write {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()>;
    fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionTimeouts {
    pub link_negotiation: Duration,
    pub link_retry: Duration,
    pub setup: Duration,
    pub availability: Duration,
    pub write: Duration,
}

impl Default for SessionTimeouts {
    fn default() -> Self {
        Self {
            link_negotiation: Duration::from_secs(15),
            link_retry: Duration::from_secs(1),
            setup: Duration::from_secs(30),
            availability: Duration::from_secs(30),
            write: Duration::from_secs(2),
        }
    }
}

impl SessionTimeouts {
    fn validate(self) -> Result<Self, SessionError> {
        let durations = [
            self.link_negotiation,
            self.link_retry,
            self.setup,
            self.availability,
            self.write,
        ];
        let now = Instant::now();
        if durations.contains(&Duration::ZERO)
            || durations
                .into_iter()
                .any(|duration| now.checked_add(duration).is_none())
        {
            return Err(SessionError::InvalidTimeout);
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPhase {
    LinkNegotiation,
    AuthenticationAndIdentification,
    ReadyForCarPlay,
    CarPlayStartSent,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WiredCarPlayStartRequest {
    pub ip_addresses: Vec<IpAddr>,
    pub port: u16,
    pub device_identifier: String,
    pub public_key: Option<String>,
    pub source_version: String,
}

impl WiredCarPlayStartRequest {
    fn to_message(&self) -> Result<CarPlayStartSession, SessionError> {
        if self.ip_addresses.is_empty() {
            return Err(SessionError::InvalidStartRequest(
                "at least one NCM address is required",
            ));
        }
        if self.port == 0 {
            return Err(SessionError::InvalidStartRequest(
                "the CarPlay service port must be nonzero",
            ));
        }
        for (field, value) in [
            ("device identifier", self.device_identifier.as_str()),
            ("source version", self.source_version.as_str()),
        ] {
            if value.is_empty() || value.as_bytes().contains(&0) {
                return Err(SessionError::InvalidStartRequest(field));
            }
        }
        if self
            .public_key
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.as_bytes().contains(&0))
        {
            return Err(SessionError::InvalidStartRequest("public key"));
        }
        Ok(CarPlayStartSession {
            wired: Some(WiredCarPlayStart {
                ip_addresses: self.ip_addresses.iter().map(ToString::to_string).collect(),
            }),
            wireless: None,
            port: Some(u32::from(self.port)),
            device_identifier: Some(self.device_identifier.clone()),
            public_key: self.public_key.clone(),
            source_version: Some(self.source_version.clone()),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkProfile {
    Direct,
    Reliable,
    Tunnel,
}

impl LinkProfile {
    fn proposal(self) -> LinkSynchronization {
        match self {
            Self::Direct | Self::Tunnel => LinkSynchronization::wired_carplay(),
            Self::Reliable => LinkSynchronization::bluetooth(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct WirelessCarPlayStartRequest {
    pub ssid: String,
    pub passphrase: String,
    pub channel: u8,
    pub security_type: crate::csm::wifi::WifiSecurityType,
    pub ip_addresses: Vec<IpAddr>,
    pub port: u16,
    pub device_identifier: String,
    pub public_key: Option<String>,
    pub source_version: String,
}

impl fmt::Debug for WirelessCarPlayStartRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WirelessCarPlayStartRequest")
            .field("ssid", &self.ssid)
            .field("passphrase", &"[redacted]")
            .field("channel", &self.channel)
            .field("security_type", &self.security_type)
            .field("ip_addresses", &self.ip_addresses)
            .field("port", &self.port)
            .field("device_identifier", &"[redacted]")
            .field(
                "public_key",
                &self.public_key.as_ref().map(|_| "[redacted]"),
            )
            .field("source_version", &self.source_version)
            .finish()
    }
}

impl WirelessCarPlayStartRequest {
    fn to_message(&self) -> Result<CarPlayStartSession, SessionError> {
        if self.ip_addresses.is_empty() {
            return Err(SessionError::InvalidStartRequest(
                "at least one access point address is required",
            ));
        }
        if self.port == 0 {
            return Err(SessionError::InvalidStartRequest(
                "the CarPlay service port must be nonzero",
            ));
        }
        for (field, value) in [
            ("SSID", self.ssid.as_str()),
            ("passphrase", self.passphrase.as_str()),
            ("device identifier", self.device_identifier.as_str()),
            ("source version", self.source_version.as_str()),
        ] {
            if value.is_empty() || value.as_bytes().contains(&0) {
                return Err(SessionError::InvalidStartRequest(field));
            }
        }
        Ok(CarPlayStartSession {
            wired: None,
            wireless: Some(WirelessCarPlayStart {
                ssid: Some(self.ssid.clone()),
                passphrase: Some(self.passphrase.clone()),
                channel: Some(self.channel),
                ip_addresses: self.ip_addresses.iter().map(ToString::to_string).collect(),
                security_type: Some(self.security_type as u8),
            }),
            port: Some(u32::from(self.port)),
            device_identifier: Some(self.device_identifier.clone()),
            public_key: self.public_key.clone(),
            source_version: Some(self.source_version.clone()),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentificationState {
    AwaitingStart,
    AwaitingResult,
    Accepted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentificationRejection {
    pub invalid_parameters: Vec<u16>,
    pub unsupported_messages_sent_by_accessory: Vec<u16>,
    pub unsupported_messages_received_from_device: Vec<u16>,
}

pub struct WiredCarkitSession<S, P: AuthProvider> {
    stream: S,
    auth: AuthMachine<P>,
    identification: IdentificationInformation,
    identification_state: IdentificationState,
    phase: SessionPhase,
    timeouts: SessionTimeouts,
    profile: LinkProfile,
    wifi_configuration: Option<AccessoryWifiConfiguration>,
    outgoing_sequence: u8,
    incoming_sequence: u8,
    peer_max_packet_length: usize,
    link_buffer: BytesMut,
    control_buffer: BytesMut,
    queued_control: VecDeque<CsmFrame>,
    availability: Option<CarPlayAvailability>,
    transport_identifiers: Option<DeviceTransportIdentifiers>,
}

impl<S, P> fmt::Debug for WiredCarkitSession<S, P>
where
    P: AuthProvider,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WiredCarkitSession")
            .field("phase", &self.phase)
            .field("auth", &self.auth)
            .field("identification_state", &self.identification_state)
            .field("outgoing_sequence", &self.outgoing_sequence)
            .field("incoming_sequence", &self.incoming_sequence)
            .field("peer_max_packet_length", &self.peer_max_packet_length)
            .field("availability", &self.availability)
            .field("transport_identifiers", &self.transport_identifiers)
            .finish_non_exhaustive()
    }
}

impl<S, P> WiredCarkitSession<S, P>
where
    S: CarkitIo,
    P: AuthProvider,
{
    pub fn new(
        stream: S,
        provider: P,
        identification: IdentificationInformation,
    ) -> Result<Self, SessionError> {
        Self::new_with_timeouts(stream, provider, identification, SessionTimeouts::default())
    }

    pub fn new_with_timeouts(
        stream: S,
        provider: P,
        identification: IdentificationInformation,
        timeouts: SessionTimeouts,
    ) -> Result<Self, SessionError> {
        Self::new_with_profile(
            stream,
            provider,
            identification,
            timeouts,
            LinkProfile::Direct,
        )
    }

    pub fn new_wireless(
        stream: S,
        provider: P,
        identification: IdentificationInformation,
        wifi_configuration: AccessoryWifiConfiguration,
    ) -> Result<Self, SessionError> {
        let mut session = Self::new_with_profile(
            stream,
            provider,
            identification,
            SessionTimeouts::default(),
            LinkProfile::Reliable,
        )?;
        session.wifi_configuration = Some(wifi_configuration);
        Ok(session)
    }

    pub fn new_wireless_tunnel(
        stream: S,
        provider: P,
        identification: IdentificationInformation,
        wifi_configuration: AccessoryWifiConfiguration,
    ) -> Result<Self, SessionError> {
        let mut session = Self::new_with_profile(
            stream,
            provider,
            identification,
            SessionTimeouts::default(),
            LinkProfile::Tunnel,
        )?;
        session.wifi_configuration = Some(wifi_configuration);
        Ok(session)
    }

    pub fn new_with_profile(
        stream: S,
        provider: P,
        identification: IdentificationInformation,
        timeouts: SessionTimeouts,
        profile: LinkProfile,
    ) -> Result<Self, SessionError> {
        match profile {
            LinkProfile::Direct => validate_identification(&identification)?,
            LinkProfile::Reliable | LinkProfile::Tunnel => {
                validate_wireless_identification(&identification)?;
            }
        }
        Ok(Self {
            stream,
            auth: AuthMachine::new(provider),
            identification,
            identification_state: IdentificationState::AwaitingStart,
            phase: SessionPhase::LinkNegotiation,
            timeouts: timeouts.validate()?,
            profile,
            wifi_configuration: None,
            outgoing_sequence: INITIAL_ACCESSORY_SEQUENCE,
            incoming_sequence: 0,
            peer_max_packet_length: DEFAULT_MAX_PACKET_LENGTH,
            link_buffer: BytesMut::new(),
            control_buffer: BytesMut::new(),
            queued_control: VecDeque::new(),
            availability: None,
            transport_identifiers: None,
        })
    }

    pub fn establish(
        stream: S,
        provider: P,
        identification: IdentificationInformation,
    ) -> Result<Self, SessionError> {
        Self::establish_with_timeouts(stream, provider, identification, SessionTimeouts::default())
    }

    pub fn establish_with_timeouts(
        stream: S,
        provider: P,
        identification: IdentificationInformation,
        timeouts: SessionTimeouts,
    ) -> Result<Self, SessionError> {
        let mut session = Self::new_with_timeouts(stream, provider, identification, timeouts)?;
        session.run_setup()?;
        Ok(session)
    }

    pub fn run_setup(&mut self) -> Result<(), SessionError> {
        if self.phase != SessionPhase::LinkNegotiation {
            return Err(SessionError::WrongPhase(self.phase));
        }
        if let Err(error) = self.negotiate_link().and_then(|()| self.complete_setup()) {
            self.phase = SessionPhase::Failed;
            return Err(error);
        }
        Ok(())
    }

    pub fn phase(&self) -> SessionPhase {
        self.phase
    }

    pub fn availability(&self) -> Option<&CarPlayAvailability> {
        self.availability.as_ref()
    }

    pub fn transport_identifiers(&self) -> Option<&DeviceTransportIdentifiers> {
        self.transport_identifiers.as_ref()
    }

    pub fn stream_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    pub fn wait_for_wired_carplay(&mut self) -> Result<WiredCarPlayAvailability, SessionError> {
        if self.phase != SessionPhase::ReadyForCarPlay {
            return Err(SessionError::WrongPhase(self.phase));
        }
        let deadline = Instant::now() + self.timeouts.availability;
        loop {
            if let Some(availability) = &self.availability {
                let wired = availability
                    .wired
                    .clone()
                    .ok_or(SessionError::WiredCarPlayUnavailable)?;
                if wired.available != Some(true) {
                    return Err(SessionError::WiredCarPlayUnavailable);
                }
                return Ok(wired);
            }
            let frame = self.next_control_frame(deadline, TimeoutStage::Availability)?;
            self.process_post_setup_frame(frame)?;
        }
    }

    pub fn start_wired_carplay(
        &mut self,
        request: &WiredCarPlayStartRequest,
    ) -> Result<(), SessionError> {
        if self.phase != SessionPhase::ReadyForCarPlay {
            return Err(SessionError::WrongPhase(self.phase));
        }
        let available = self
            .availability
            .as_ref()
            .and_then(|availability| availability.wired.as_ref())
            .is_some_and(|wired| wired.available == Some(true));
        if !available {
            return Err(SessionError::WiredCarPlayUnavailable);
        }
        let frame = request.to_message()?.to_frame()?;
        let deadline = Instant::now() + self.timeouts.write;
        self.write_control_frame_before(&frame, deadline)?;
        self.phase = SessionPhase::CarPlayStartSent;
        Ok(())
    }

    pub fn wait_for_wireless_carplay(
        &mut self,
    ) -> Result<WirelessCarPlayAvailability, SessionError> {
        if self.phase != SessionPhase::ReadyForCarPlay {
            return Err(SessionError::WrongPhase(self.phase));
        }
        let deadline = Instant::now() + self.timeouts.availability;
        loop {
            if let Some(availability) = &self.availability {
                let wireless = availability
                    .wireless
                    .clone()
                    .ok_or(SessionError::WirelessCarPlayUnavailable)?;
                if wireless.available != Some(true) {
                    return Err(SessionError::WirelessCarPlayUnavailable);
                }
                return Ok(wireless);
            }
            let frame = self.next_control_frame(deadline, TimeoutStage::Availability)?;
            self.process_post_setup_frame(frame)?;
        }
    }

    pub fn start_wireless_carplay(
        &mut self,
        request: &WirelessCarPlayStartRequest,
    ) -> Result<(), SessionError> {
        if self.phase != SessionPhase::ReadyForCarPlay {
            return Err(SessionError::WrongPhase(self.phase));
        }
        let available = self
            .availability
            .as_ref()
            .and_then(|availability| availability.wireless.as_ref())
            .is_some_and(|wireless| wireless.available == Some(true));
        if !available {
            return Err(SessionError::WirelessCarPlayUnavailable);
        }
        let frame = request.to_message()?.to_frame()?;
        let deadline = Instant::now() + self.timeouts.write;
        self.write_control_frame_before(&frame, deadline)?;
        self.phase = SessionPhase::CarPlayStartSent;
        Ok(())
    }

    pub fn service_link(&mut self, timeout: Duration) -> Result<Option<u16>, SessionError> {
        let serviceable = match self.profile {
            LinkProfile::Tunnel => self.phase == SessionPhase::ReadyForCarPlay,
            LinkProfile::Direct | LinkProfile::Reliable => {
                self.phase == SessionPhase::CarPlayStartSent
            }
        };
        if !serviceable {
            return Err(SessionError::WrongPhase(self.phase));
        }
        let frame =
            match self.next_control_frame(Instant::now() + timeout, TimeoutStage::Availability) {
                Ok(frame) => frame,
                Err(SessionError::AvailabilityTimeout) => return Ok(None),
                Err(error) => return Err(error),
            };
        let message_id = frame.message_id;
        match message_id {
            REQUEST_ACCESSORY_WIFI_CONFIGURATION_INFORMATION => self.answer_wifi_request()?,
            CARPLAY_AVAILABILITY => {
                self.availability = Some(CarPlayAvailability::from_frame(&frame)?);
            }
            DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION => {
                self.transport_identifiers = Some(DeviceTransportIdentifiers::from_frame(&frame)?);
            }
            _ => {}
        }
        Ok(Some(message_id))
    }

    fn answer_wifi_request(&mut self) -> Result<(), SessionError> {
        let configuration = self
            .wifi_configuration
            .as_ref()
            .ok_or(SessionError::WifiConfigurationUnavailable)?;
        let response = configuration.to_frame()?;
        self.write_control_frame_before(&response, Instant::now() + self.timeouts.write)
    }

    pub fn into_parts(self) -> (S, P) {
        (self.stream, self.auth.into_provider())
    }

    fn negotiate_link(&mut self) -> Result<(), SessionError> {
        let proposal = self.profile.proposal();
        let synchronization = LinkPacket::with_payload(
            ControlBits::SYN,
            INITIAL_ACCESSORY_SEQUENCE,
            0,
            0,
            proposal.encode()?,
        )
        .encode()?;
        let write_deadline = Instant::now() + self.timeouts.write;
        self.write_raw_before(&DETECT_MARKER, write_deadline)?;
        self.write_raw_before(&synchronization, write_deadline)?;

        let deadline = Instant::now() + self.timeouts.link_negotiation;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(SessionError::LinkNegotiationTimeout);
            }
            let read_for = self
                .timeouts
                .link_retry
                .min(deadline.saturating_duration_since(now));
            match self.read_negotiation_packet(read_for) {
                Ok(packet) => {
                    if packet.control.contains(ControlBits::RST) {
                        return Err(SessionError::PeerReset);
                    }
                    if packet.control != (ControlBits::SYN | ControlBits::ACK) {
                        return Err(SessionError::UnexpectedSynchronizationControl(
                            packet.control.bits(),
                        ));
                    }
                    if packet.acknowledgement != INITIAL_ACCESSORY_SEQUENCE {
                        return Err(SessionError::SynchronizationAcknowledgement {
                            expected: INITIAL_ACCESSORY_SEQUENCE,
                            actual: packet.acknowledgement,
                        });
                    }
                    if packet.session_id != 0 {
                        return Err(SessionError::UnexpectedSession(packet.session_id));
                    }
                    let peer = LinkSynchronization::decode(&packet.payload)?;
                    match self.profile {
                        LinkProfile::Direct | LinkProfile::Tunnel => {
                            validate_direct_peer_profile(&proposal, &peer)?;
                        }
                        LinkProfile::Reliable => validate_reliable_peer_profile(&proposal, &peer)?,
                    }
                    self.incoming_sequence = packet.sequence;
                    self.peer_max_packet_length = usize::from(peer.max_packet_length);
                    let final_ack = LinkPacket::header_only(
                        ControlBits::ACK,
                        self.outgoing_sequence,
                        self.incoming_sequence,
                    )
                    .encode()?;
                    self.write_raw_before(&final_ack, Instant::now() + self.timeouts.write)?;
                    self.phase = SessionPhase::AuthenticationAndIdentification;
                    return Ok(());
                }
                Err(SessionError::Io { source, .. }) if timeout_error(&source) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(SessionError::LinkNegotiationTimeout);
                    }
                    self.write_raw_before(&synchronization, now + self.timeouts.write)?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn complete_setup(&mut self) -> Result<(), SessionError> {
        let setup_deadline = Instant::now() + self.timeouts.setup;
        while self.auth.state() != AuthState::Authenticated
            || self.identification_state != IdentificationState::Accepted
        {
            let deadline = self
                .auth
                .deadline()
                .map_or(setup_deadline, |auth| auth.min(setup_deadline));
            let frame = match self.next_control_frame(deadline, TimeoutStage::Setup) {
                Ok(frame) => frame,
                Err(SessionError::SetupTimeout) if self.auth.deadline().is_some() => {
                    self.auth.check_deadline()?;
                    return Err(SessionError::SetupTimeout);
                }
                Err(error) => return Err(error),
            };
            self.process_setup_frame(frame)?;
        }
        self.phase = SessionPhase::ReadyForCarPlay;
        Ok(())
    }

    fn process_setup_frame(&mut self, frame: CsmFrame) -> Result<(), SessionError> {
        let received_at = Instant::now();
        if self
            .auth
            .deadline()
            .is_some_and(|deadline| received_at > deadline)
        {
            self.auth.check_deadline()?;
        }
        match frame.message_id {
            REQUEST_AUTHENTICATION_CERTIFICATE
            | REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE
            | AUTHENTICATION_FAILED
            | AUTHENTICATION_SUCCEEDED => {
                let response_deadline = match frame.message_id {
                    REQUEST_AUTHENTICATION_CERTIFICATE => {
                        Some(received_at + CERTIFICATE_RESPONSE_DEADLINE)
                    }
                    REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE => {
                        Some(received_at + CHALLENGE_RESPONSE_DEADLINE)
                    }
                    _ => None,
                };
                match self.auth.handle(&frame)? {
                    AuthAction::Send(response) => {
                        let deadline = response_deadline
                            .ok_or(SessionError::UnexpectedControlMessage(frame.message_id))?;
                        self.write_control_frame_before(&response, deadline)?;
                    }
                    AuthAction::Authenticated => {}
                    AuthAction::RejectedByDevice => {
                        return Err(SessionError::AuthenticationRejected);
                    }
                }
            }
            START_IDENTIFICATION => {
                require_empty(&frame)?;
                if self.identification_state != IdentificationState::AwaitingStart {
                    return Err(SessionError::IdentificationOutOfOrder);
                }
                let response = self.identification.to_frame()?;
                self.write_control_frame_before(&response, received_at + self.timeouts.write)?;
                self.identification_state = IdentificationState::AwaitingResult;
            }
            IDENTIFICATION_ACCEPTED => {
                require_empty(&frame)?;
                if self.identification_state != IdentificationState::AwaitingResult {
                    return Err(SessionError::IdentificationOutOfOrder);
                }
                self.identification_state = IdentificationState::Accepted;
            }
            IDENTIFICATION_REJECTED => {
                if self.identification_state != IdentificationState::AwaitingResult {
                    return Err(SessionError::IdentificationOutOfOrder);
                }
                return Err(SessionError::IdentificationRejected(
                    rejected_identification_fields(&frame)?,
                ));
            }
            CARPLAY_AVAILABILITY | DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION => {
                return Err(SessionError::MessageBeforeAuthentication(frame.message_id));
            }
            message_id => return Err(SessionError::UnexpectedControlMessage(message_id)),
        }
        Ok(())
    }

    fn process_post_setup_frame(&mut self, frame: CsmFrame) -> Result<(), SessionError> {
        match frame.message_id {
            CARPLAY_AVAILABILITY => {
                if self.availability.is_some() {
                    return Err(SessionError::DuplicateControlMessage(CARPLAY_AVAILABILITY));
                }
                self.availability = Some(CarPlayAvailability::from_frame(&frame)?);
            }
            DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION => {
                if self.transport_identifiers.is_some() {
                    return Err(SessionError::DuplicateControlMessage(
                        DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION,
                    ));
                }
                self.transport_identifiers = Some(DeviceTransportIdentifiers::from_frame(&frame)?);
            }
            REQUEST_ACCESSORY_WIFI_CONFIGURATION_INFORMATION
                if self.wifi_configuration.is_some() =>
            {
                self.answer_wifi_request()?;
            }
            message_id => return Err(SessionError::UnexpectedControlMessage(message_id)),
        }
        Ok(())
    }

    fn next_control_frame(
        &mut self,
        deadline: Instant,
        stage: TimeoutStage,
    ) -> Result<CsmFrame, SessionError> {
        loop {
            if let Some(frame) = self.queued_control.pop_front() {
                return Ok(frame);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(stage.error());
            }
            let packet = match self.read_packet(deadline.saturating_duration_since(now)) {
                Ok(packet) => packet,
                Err(SessionError::Io { source, .. }) if timeout_error(&source) => {
                    return Err(stage.error());
                }
                Err(error) => return Err(error),
            };
            self.accept_established_packet(packet)?;
        }
    }

    fn accept_established_packet(&mut self, packet: LinkPacket) -> Result<(), SessionError> {
        if packet.control.contains(ControlBits::RST) {
            return Err(SessionError::PeerReset);
        }
        if packet.control.contains(ControlBits::SYN) {
            return Err(SessionError::UnexpectedSynchronizationControl(
                packet.control.bits(),
            ));
        }
        if packet.payload.is_empty() {
            return Ok(());
        }
        if packet.session_id != CONTROL_SESSION_ID {
            return Err(SessionError::UnexpectedSession(packet.session_id));
        }
        let expected = self.incoming_sequence.wrapping_add(1);
        if packet.sequence == self.incoming_sequence {
            self.send_ack()?;
            return Ok(());
        }
        if packet.sequence != expected {
            return Err(SessionError::OutOfSequence {
                expected,
                actual: packet.sequence,
            });
        }
        self.incoming_sequence = packet.sequence;
        if self.profile == LinkProfile::Reliable {
            self.send_ack()?;
        }
        self.control_buffer.extend_from_slice(&packet.payload);
        loop {
            let Some((frame, consumed)) = CsmFrame::decode_prefix(&self.control_buffer)? else {
                break;
            };
            self.control_buffer.advance(consumed);
            self.queued_control.push_back(frame);
        }
        Ok(())
    }

    fn send_ack(&mut self) -> Result<(), SessionError> {
        let packet = LinkPacket::header_only(
            ControlBits::ACK,
            self.outgoing_sequence,
            self.incoming_sequence,
        )
        .encode()?;
        self.write_raw_before(&packet, Instant::now() + self.timeouts.write)
    }

    fn write_control_frame_before(
        &mut self,
        frame: &CsmFrame,
        deadline: Instant,
    ) -> Result<(), SessionError> {
        let encoded = frame.encode()?;
        let maximum_payload = self
            .peer_max_packet_length
            .checked_sub(LINK_HEADER_LEN + 1)
            .filter(|length| *length > 0)
            .ok_or(SessionError::PeerPacketLengthTooSmall(
                self.peer_max_packet_length,
            ))?;
        for payload in encoded.chunks(maximum_payload) {
            self.outgoing_sequence = self.outgoing_sequence.wrapping_add(1);
            let packet = LinkPacket::with_payload(
                ControlBits::ACK,
                self.outgoing_sequence,
                self.incoming_sequence,
                CONTROL_SESSION_ID,
                Bytes::copy_from_slice(payload),
            )
            .encode()?;
            match self.write_raw_before(&packet, deadline) {
                Err(SessionError::Io { source, .. }) if timeout_error(&source) => {
                    return Err(SessionError::ResponseDeadlineExceeded);
                }
                result => result?,
            }
        }
        Ok(())
    }

    fn read_packet(&mut self, timeout: Duration) -> Result<LinkPacket, SessionError> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.link_buffer.len() >= LINK_HEADER_LEN {
                if let Some((packet, consumed)) = LinkPacket::decode_prefix(&self.link_buffer)? {
                    self.link_buffer.advance(consumed);
                    return Ok(packet);
                }
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(SessionError::Io {
                    operation: "read link packet",
                    source: io::Error::new(io::ErrorKind::TimedOut, "link read deadline elapsed"),
                });
            }
            self.stream
                .set_read_timeout(Some(deadline.saturating_duration_since(now)))
                .map_err(|source| SessionError::Io {
                    operation: "set read timeout",
                    source,
                })?;
            let mut chunk = [0; 4096];
            let count = self
                .stream
                .read(&mut chunk)
                .map_err(|source| SessionError::Io {
                    operation: "read link packet",
                    source,
                })?;
            if count == 0 {
                return Err(SessionError::Io {
                    operation: "read link packet",
                    source: io::Error::new(io::ErrorKind::UnexpectedEof, "carkit stream closed"),
                });
            }
            self.link_buffer.extend_from_slice(&chunk[..count]);
        }
    }

    fn read_negotiation_packet(&mut self, timeout: Duration) -> Result<LinkPacket, SessionError> {
        let deadline = Instant::now() + timeout;
        let mut peer_detect_markers = 0usize;
        loop {
            if self.link_buffer.len() >= 2 && self.link_buffer[..2] == DETECT_MARKER[..2] {
                if self.link_buffer.len() >= DETECT_MARKER.len() {
                    if self.link_buffer[..DETECT_MARKER.len()] != DETECT_MARKER {
                        return Err(SessionError::MalformedDetectionMarker);
                    }
                    self.link_buffer.advance(DETECT_MARKER.len());
                    peer_detect_markers += 1;
                    if peer_detect_markers > MAXIMUM_PEER_DETECT_MARKERS {
                        return Err(SessionError::TooManyDetectionMarkers);
                    }
                    continue;
                }
            } else if self.link_buffer.len() >= LINK_HEADER_LEN {
                if let Some((packet, consumed)) = LinkPacket::decode_prefix(&self.link_buffer)? {
                    self.link_buffer.advance(consumed);
                    return Ok(packet);
                }
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(SessionError::Io {
                    operation: "read link negotiation",
                    source: io::Error::new(
                        io::ErrorKind::TimedOut,
                        "link negotiation read deadline elapsed",
                    ),
                });
            }
            self.stream
                .set_read_timeout(Some(deadline.saturating_duration_since(now)))
                .map_err(|source| SessionError::Io {
                    operation: "set read timeout",
                    source,
                })?;
            let mut chunk = [0; 4096];
            let count = self
                .stream
                .read(&mut chunk)
                .map_err(|source| SessionError::Io {
                    operation: "read link negotiation",
                    source,
                })?;
            if count == 0 {
                return Err(SessionError::Io {
                    operation: "read link negotiation",
                    source: io::Error::new(io::ErrorKind::UnexpectedEof, "carkit stream closed"),
                });
            }
            self.link_buffer.extend_from_slice(&chunk[..count]);
        }
    }

    fn write_raw_before(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), SessionError> {
        let now = Instant::now();
        if now >= deadline {
            return Err(SessionError::ResponseDeadlineExceeded);
        }
        self.stream
            .set_write_timeout(Some(deadline.saturating_duration_since(now)))
            .map_err(|source| SessionError::Io {
                operation: "set write timeout",
                source,
            })?;
        self.stream
            .write_all(bytes)
            .and_then(|()| self.stream.flush())
            .map_err(|source| SessionError::Io {
                operation: "write carkit stream",
                source,
            })?;
        if Instant::now() > deadline {
            return Err(SessionError::ResponseDeadlineExceeded);
        }
        Ok(())
    }
}

fn validate_identification(identification: &IdentificationInformation) -> Result<(), SessionError> {
    identification.validate()?;
    let usb = identification
        .usb_host_transport
        .as_ref()
        .filter(|usb| usb.supports_iap2_connection && usb.supports_carplay)
        .ok_or(SessionError::MissingWiredCarPlayTransport)?;
    if usb.carplay_interface_number.is_none() {
        return Err(SessionError::MissingWiredCarPlayTransport);
    }
    let sent = identification.messages_sent_by_accessory();
    let received = identification.messages_received_from_device();
    if !sent.contains(&CARPLAY_START_SESSION)
        || !received.contains(&CARPLAY_AVAILABILITY)
        || !received.contains(&DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION)
    {
        return Err(SessionError::IncompleteControlMessageDeclaration);
    }
    Ok(())
}

fn validate_wireless_identification(
    identification: &IdentificationInformation,
) -> Result<(), SessionError> {
    identification.validate()?;
    let bluetooth = identification
        .bluetooth_transport
        .as_ref()
        .is_some_and(|component| component.supports_iap2_connection);
    let wireless = identification
        .wireless_carplay_transport
        .as_ref()
        .is_some_and(|component| component.supports_carplay);
    if !bluetooth || !wireless {
        return Err(SessionError::MissingWirelessCarPlayTransport);
    }
    let sent = identification.messages_sent_by_accessory();
    let received = identification.messages_received_from_device();
    if !sent.contains(&CARPLAY_START_SESSION)
        || !sent.contains(&ACCESSORY_WIFI_CONFIGURATION_INFORMATION)
        || !received.contains(&CARPLAY_AVAILABILITY)
        || !received.contains(&REQUEST_ACCESSORY_WIFI_CONFIGURATION_INFORMATION)
    {
        return Err(SessionError::IncompleteControlMessageDeclaration);
    }
    Ok(())
}

fn validate_reliable_peer_profile(
    proposal: &LinkSynchronization,
    peer: &LinkSynchronization,
) -> Result<(), SessionError> {
    if peer.sessions != proposal.sessions {
        return Err(SessionError::DirectProfileMismatch(
            "peer did not accept the control-v2 session list",
        ));
    }
    if peer.max_outgoing == 0 {
        return Err(SessionError::DirectProfileMismatch(
            "peer advertised a zero outgoing packet window",
        ));
    }
    Ok(())
}

fn validate_direct_peer_profile(
    proposal: &LinkSynchronization,
    peer: &LinkSynchronization,
) -> Result<(), SessionError> {
    if peer.retransmission_timeout_ms != proposal.retransmission_timeout_ms
        || peer.acknowledgement_timeout_ms != proposal.acknowledgement_timeout_ms
        || peer.max_retransmissions != proposal.max_retransmissions
        || peer.max_cumulative_acknowledgements != proposal.max_cumulative_acknowledgements
    {
        return Err(SessionError::DirectProfileMismatch(
            "peer did not accept the direct-stream zero-timer settings",
        ));
    }
    if peer.sessions != proposal.sessions {
        return Err(SessionError::DirectProfileMismatch(
            "peer did not accept the control-v2 carkit session list",
        ));
    }
    Ok(())
}

fn require_empty(frame: &CsmFrame) -> Result<(), SessionError> {
    if let Some(parameter) = frame.parameters.first() {
        return Err(SessionError::Csm(CsmError::UnexpectedParameter(
            parameter.id,
        )));
    }
    Ok(())
}

fn rejected_identification_fields(
    frame: &CsmFrame,
) -> Result<IdentificationRejection, SessionError> {
    const REPEATABLE_NONE_PARAMETERS: [u16; 2] = [18, 23];
    const NONE_PARAMETERS: [u16; 20] = [
        0, 1, 2, 3, 4, 5, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 22, 23, 29,
    ];
    let mut invalid_parameters = Vec::with_capacity(frame.parameters.len());
    let mut unsupported_messages_sent_by_accessory = None;
    let mut unsupported_messages_received_from_device = None;
    for parameter in &frame.parameters {
        match parameter.id {
            6 => {
                if unsupported_messages_sent_by_accessory.is_some() {
                    return Err(SessionError::Csm(CsmError::DuplicateParameter(6)));
                }
                unsupported_messages_sent_by_accessory =
                    Some(decode_message_identifier_array(6, &parameter.payload)?);
            }
            7 => {
                if unsupported_messages_received_from_device.is_some() {
                    return Err(SessionError::Csm(CsmError::DuplicateParameter(7)));
                }
                unsupported_messages_received_from_device =
                    Some(decode_message_identifier_array(7, &parameter.payload)?);
            }
            id if NONE_PARAMETERS.contains(&id) => {
                if !parameter.payload.is_empty() {
                    return Err(SessionError::Csm(CsmError::InvalidPayloadLength {
                        id,
                        expected: 0,
                        actual: parameter.payload.len(),
                    }));
                }
                if !REPEATABLE_NONE_PARAMETERS.contains(&id) && invalid_parameters.contains(&id) {
                    return Err(SessionError::Csm(CsmError::DuplicateParameter(id)));
                }
                invalid_parameters.push(id);
            }
            id => return Err(SessionError::Csm(CsmError::UnexpectedParameter(id))),
        }
    }
    Ok(IdentificationRejection {
        invalid_parameters,
        unsupported_messages_sent_by_accessory: unsupported_messages_sent_by_accessory
            .unwrap_or_default(),
        unsupported_messages_received_from_device: unsupported_messages_received_from_device
            .unwrap_or_default(),
    })
}

fn decode_message_identifier_array(id: u16, payload: &[u8]) -> Result<Vec<u16>, SessionError> {
    if payload.len() % 2 != 0 {
        return Err(SessionError::Csm(CsmError::InvalidArrayPayloadLength {
            id,
            element_size: 2,
            actual: payload.len(),
        }));
    }
    let mut messages = Vec::with_capacity(payload.len() / 2);
    for bytes in payload.chunks_exact(2) {
        let message = u16::from_be_bytes([bytes[0], bytes[1]]);
        if messages.contains(&message) {
            return Err(SessionError::Csm(CsmError::DuplicateMessageIdentifier {
                parameter_id: id,
                message_id: message,
            }));
        }
        messages.push(message);
    }
    Ok(messages)
}

fn timeout_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

#[derive(Debug, Clone, Copy)]
enum TimeoutStage {
    Setup,
    Availability,
}

impl TimeoutStage {
    const fn error(self) -> SessionError {
        match self {
            Self::Setup => SessionError::SetupTimeout,
            Self::Availability => SessionError::AvailabilityTimeout,
        }
    }
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("carkit I/O failed while attempting to {operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("carkit session timeout is zero or cannot be represented by the monotonic clock")]
    InvalidTimeout,
    #[error("iAP2 link frame failed validation: {0}")]
    Frame(#[from] FrameError),
    #[error("iAP2 control message failed validation: {0}")]
    Csm(#[from] CsmError),
    #[error("accessory identification is invalid: {0}")]
    Identification(#[from] IdentificationError),
    #[error("accessory authentication failed: {0}")]
    Authentication(#[from] AuthError),
    #[error("direct iAP2 link negotiation timed out")]
    LinkNegotiationTimeout,
    #[error("peer sent a malformed iAP2 link-detection marker")]
    MalformedDetectionMarker,
    #[error("peer sent too many iAP2 link-detection markers")]
    TooManyDetectionMarkers,
    #[error("authentication and identification setup timed out")]
    SetupTimeout,
    #[error("waiting for CarPlayAvailability timed out")]
    AvailabilityTimeout,
    #[error("the device reset the iAP2 link")]
    PeerReset,
    #[error("unexpected synchronization packet control byte 0x{0:02x}")]
    UnexpectedSynchronizationControl(u8),
    #[error("synchronization acknowledged packet {actual}; expected {expected}")]
    SynchronizationAcknowledgement { expected: u8, actual: u8 },
    #[error("direct carkit link profile mismatch: {0}")]
    DirectProfileMismatch(&'static str),
    #[error("unexpected iAP2 session id {0}")]
    UnexpectedSession(u8),
    #[error("received packet sequence {actual}; expected {expected}")]
    OutOfSequence { expected: u8, actual: u8 },
    #[error("unexpected control-session message 0x{0:04x}")]
    UnexpectedControlMessage(u16),
    #[error("duplicate control-session message 0x{0:04x}")]
    DuplicateControlMessage(u16),
    #[error("control-session message 0x{0:04x} arrived before authentication and identification")]
    MessageBeforeAuthentication(u16),
    #[error("accessory authentication was rejected by the device")]
    AuthenticationRejected,
    #[error("accessory identification message arrived out of order")]
    IdentificationOutOfOrder,
    #[error("accessory identification was rejected: {0:?}")]
    IdentificationRejected(IdentificationRejection),
    #[error("the peer's maximum packet length {0} cannot carry payload data")]
    PeerPacketLengthTooSmall(usize),
    #[error("the carkit stream exceeded a protocol response deadline")]
    ResponseDeadlineExceeded,
    #[error("wired CarPlay is unavailable")]
    WiredCarPlayUnavailable,
    #[error("invalid wired CarPlay start request: {0}")]
    InvalidStartRequest(&'static str),
    #[error("operation is invalid while the session is in phase {0:?}")]
    WrongPhase(SessionPhase),
    #[error("identification does not declare a CarPlay-capable USB host transport")]
    MissingWiredCarPlayTransport,
    #[error("identification does not exhaustively declare the CarPlay messages")]
    IncompleteControlMessageDeclaration,
    #[error("wireless CarPlay is unavailable")]
    WirelessCarPlayUnavailable,
    #[error("identification does not declare Bluetooth and wireless CarPlay transports")]
    MissingWirelessCarPlayTransport,
    #[error("the device requested Wi-Fi configuration but none is configured")]
    WifiConfigurationUnavailable,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        io,
        sync::{Arc, Mutex},
        thread,
    };

    use prelude_mfi::{CERTIFICATE_SERIAL_LEN, CHALLENGE_LEN, RESPONSE_LEN};

    use super::*;
    use crate::csm::{
        CsmParameter, auth::AuthenticationMessage, carplay::WiredCarPlayAvailability,
    };

    #[derive(Debug)]
    enum ReadStep {
        Data(VecDeque<u8>),
        Timeout,
    }

    #[derive(Debug, Default)]
    struct FakeIoState {
        written: Vec<u8>,
        read_timeouts: Vec<Duration>,
    }

    #[derive(Debug)]
    struct FakeStream {
        reads: VecDeque<ReadStep>,
        state: Arc<Mutex<FakeIoState>>,
        read_timeout: Duration,
        write_timeout: Duration,
    }

    impl FakeStream {
        fn new(steps: impl IntoIterator<Item = ReadStep>) -> (Self, Arc<Mutex<FakeIoState>>) {
            let state = Arc::new(Mutex::new(FakeIoState::default()));
            (
                Self {
                    reads: steps.into_iter().collect(),
                    state: Arc::clone(&state),
                    read_timeout: Duration::from_millis(1),
                    write_timeout: Duration::from_millis(1),
                },
                state,
            )
        }
    }

    impl Read for FakeStream {
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
                        thread::sleep(self.read_timeout.min(Duration::from_millis(10)));
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "injected timeout"));
                    }
                    None => {
                        thread::sleep(self.read_timeout.min(Duration::from_millis(10)));
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "fixture exhausted"));
                    }
                }
            }
        }
    }

    impl Write for FakeStream {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            self.state
                .lock()
                .map_err(|_| io::Error::other("poisoned fixture"))?
                .written
                .extend_from_slice(input);
            Ok(input.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl CarkitIo for FakeStream {
        fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
            self.read_timeout = timeout.unwrap_or(Duration::from_secs(1));
            self.state
                .lock()
                .map_err(|_| io::Error::other("poisoned fixture"))?
                .read_timeouts
                .push(self.read_timeout);
            Ok(())
        }

        fn set_write_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
            self.write_timeout = timeout.unwrap_or(Duration::from_secs(1));
            Ok(())
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
            3,
        )
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
            link_negotiation: Duration::from_millis(20),
            link_retry: Duration::from_millis(2),
            setup: Duration::from_millis(20),
            availability: Duration::from_millis(20),
            write: Duration::from_millis(20),
        }
    }

    #[test]
    fn direct_profile_has_a_byte_exact_golden_prefix() {
        const GOLDEN: &[u8] = &[
            0xff, 0x55, 0x02, 0x00, 0xee, 0x10, 0xff, 0x5a, 0x00, 0x1d, 0x80, 0x63, 0x00, 0x00,
            0xa7, 0x01, 0x04, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x02,
            0x0b, 0x02, 0x01, 0x0c, 0x01, 0x02, 0xd4,
        ];
        let (stream, state) =
            FakeStream::new([ReadStep::Data(successful_input(7).into_iter().collect())]);
        let mut session = WiredCarkitSession::establish_with_timeouts(
            stream,
            FakeProvider,
            identification(),
            fast_timeouts(),
        )
        .expect("establish");
        session.wait_for_wired_carplay().expect("availability");
        session
            .start_wired_carplay(&WiredCarPlayStartRequest {
                ip_addresses: vec!["fe80::1".parse().expect("address")],
                port: 7000,
                device_identifier: "00:11:22:33:44:55".into(),
                public_key: Some("receiver-public-key".into()),
                source_version: "1.0".into(),
            })
            .expect("start");
        let written = state.lock().expect("fixture").written.clone();
        assert_eq!(&written[..GOLDEN.len()], GOLDEN);

        let message_ids = outgoing_control_message_ids(&written[GOLDEN.len()..]);
        for expected in [
            crate::csm::identification::IDENTIFICATION_INFORMATION,
            crate::csm::auth::AUTHENTICATION_CERTIFICATE,
            crate::csm::auth::AUTHENTICATION_RESPONSE,
            CARPLAY_START_SESSION,
        ] {
            assert!(message_ids.contains(&expected), "missing 0x{expected:04x}");
        }
        assert_eq!(session.phase(), SessionPhase::CarPlayStartSent);
    }

    #[test]
    fn peer_detection_marker_is_consumed_before_synchronization() {
        let mut input = DETECT_MARKER.to_vec();
        input.extend_from_slice(&successful_input(7));
        let (prefix, remainder) = input.split_at(3);
        let (stream, _state) = FakeStream::new([
            ReadStep::Data(prefix.iter().copied().collect()),
            ReadStep::Data(remainder.iter().copied().collect()),
        ]);
        let mut session = WiredCarkitSession::establish_with_timeouts(
            stream,
            FakeProvider,
            identification(),
            fast_timeouts(),
        )
        .expect("establish after peer detection response");
        session.wait_for_wired_carplay().expect("availability");
    }

    #[test]
    fn malformed_peer_detection_marker_fails_closed() {
        let mut input = DETECT_MARKER.to_vec();
        input[5] ^= 1;
        input.extend_from_slice(&successful_input(7));
        let (stream, _state) = FakeStream::new([ReadStep::Data(input.into_iter().collect())]);
        let error = WiredCarkitSession::establish_with_timeouts(
            stream,
            FakeProvider,
            identification(),
            fast_timeouts(),
        )
        .expect_err("malformed detect response");
        assert!(matches!(error, SessionError::MalformedDetectionMarker));
    }

    #[test]
    fn identification_rejection_decodes_unsupported_message_sets() {
        let rejection = rejected_identification_fields(&CsmFrame {
            message_id: IDENTIFICATION_REJECTED,
            parameters: vec![
                CsmParameter::new(6, Bytes::from_static(&[0xaa, 0x06, 0x43, 0x01])),
                CsmParameter::new(7, Bytes::from_static(&[0x43, 0x00])),
                CsmParameter::presence(16),
                CsmParameter::presence(18),
                CsmParameter::presence(18),
            ],
        })
        .expect("valid rejection");
        assert_eq!(rejection.invalid_parameters, [16, 18, 18]);
        assert_eq!(
            rejection.unsupported_messages_sent_by_accessory,
            [0xaa06, 0x4301]
        );
        assert_eq!(
            rejection.unsupported_messages_received_from_device,
            [0x4300]
        );
    }

    #[test]
    fn identification_rejection_rejects_odd_message_array() {
        let error = rejected_identification_fields(&CsmFrame {
            message_id: IDENTIFICATION_REJECTED,
            parameters: vec![CsmParameter::new(6, Bytes::from_static(&[0x43]))],
        })
        .expect_err("odd uint16 array");
        assert!(matches!(
            error,
            SessionError::Csm(CsmError::InvalidArrayPayloadLength {
                id: 6,
                element_size: 2,
                actual: 1
            })
        ));
    }

    #[test]
    fn peer_chooses_valid_flow_control_limits_for_the_direct_profile() {
        let proposal = LinkSynchronization::wired_carplay();
        for (maximum_outstanding, maximum_packet_length) in [(1, 24), (127, u16::MAX)] {
            let mut peer = proposal.clone();
            peer.max_outgoing = maximum_outstanding;
            peer.max_packet_length = maximum_packet_length;
            peer.validate().expect("valid peer limits");
            validate_direct_peer_profile(&proposal, &peer).expect("compatible direct profile");
        }

        for (maximum_outstanding, maximum_packet_length) in [(0, 24), (1, 23)] {
            let mut peer = proposal.clone();
            peer.max_outgoing = maximum_outstanding;
            peer.max_packet_length = maximum_packet_length;
            assert_eq!(peer.validate(), Err(FrameError::InvalidLinkParameters));
        }
    }

    #[test]
    fn link_retransmits_the_identical_syn_after_timeout() {
        let (stream, state) = FakeStream::new([
            ReadStep::Timeout,
            ReadStep::Data(successful_input(7).into_iter().collect()),
        ]);
        let mut session = WiredCarkitSession::establish_with_timeouts(
            stream,
            FakeProvider,
            identification(),
            fast_timeouts(),
        )
        .expect("establish after retry");
        session.wait_for_wired_carplay().expect("availability");

        let written = &state.lock().expect("fixture").written;
        let synchronization = &written[DETECT_MARKER.len()..DETECT_MARKER.len() + 29];
        assert!(
            written[DETECT_MARKER.len() + 29..]
                .windows(synchronization.len())
                .any(|window| window == synchronization)
        );
    }

    #[test]
    fn timeout_during_a_partial_packet_preserves_buffered_bytes() {
        let input = successful_input(7);
        let (prefix, remainder) = input.split_at(4);
        let (stream, state) = FakeStream::new([
            ReadStep::Data(prefix.iter().copied().collect()),
            ReadStep::Timeout,
            ReadStep::Data(remainder.iter().copied().collect()),
        ]);
        let mut session = WiredCarkitSession::establish_with_timeouts(
            stream,
            FakeProvider,
            identification(),
            fast_timeouts(),
        )
        .expect("establish after partial read timeout");
        session.wait_for_wired_carplay().expect("availability");

        let written = state.lock().expect("fixture").written.clone();
        let synchronization = &written[DETECT_MARKER.len()..DETECT_MARKER.len() + 29];
        assert!(
            written[DETECT_MARKER.len() + 29..]
                .windows(synchronization.len())
                .any(|window| window == synchronization)
        );
    }

    #[test]
    fn duplicate_data_retransmission_is_acknowledged_without_replay() {
        let mut input = peer_synchronization(7);
        let certificate_request = device_packet(
            8,
            AuthenticationMessage::RequestCertificate {
                request_serial_number: false,
            }
            .to_frame(),
        );
        input.extend_from_slice(&certificate_request);
        input.extend_from_slice(&certificate_request);
        for (sequence, frame) in [
            (9, CsmFrame::empty(START_IDENTIFICATION)),
            (
                10,
                AuthenticationMessage::RequestChallengeResponse([0x56; CHALLENGE_LEN]).to_frame(),
            ),
            (11, CsmFrame::empty(IDENTIFICATION_ACCEPTED)),
            (12, AuthenticationMessage::Succeeded.to_frame()),
            (
                13,
                CarPlayAvailability {
                    wired: Some(WiredCarPlayAvailability {
                        available: Some(true),
                        usb_transport_identifier: Some("usb-carkit".into()),
                    }),
                    wireless: None,
                }
                .to_frame()
                .expect("availability fixture"),
            ),
        ] {
            input.extend_from_slice(&device_packet(sequence, frame));
        }

        let (stream, state) = FakeStream::new([ReadStep::Data(input.into_iter().collect())]);
        let mut session = WiredCarkitSession::establish_with_timeouts(
            stream,
            FakeProvider,
            identification(),
            fast_timeouts(),
        )
        .expect("establish");
        session.wait_for_wired_carplay().expect("availability");

        let written = state.lock().expect("fixture").written.clone();
        let message_ids = outgoing_control_message_ids(&written[DETECT_MARKER.len() + 29..]);
        assert_eq!(
            message_ids
                .iter()
                .filter(|message_id| {
                    **message_id == crate::csm::auth::AUTHENTICATION_CERTIFICATE
                })
                .count(),
            1,
        );
    }

    #[test]
    fn link_timeout_is_bounded_and_retries_syn() {
        let (stream, state) = FakeStream::new([]);
        let error = WiredCarkitSession::establish_with_timeouts(
            stream,
            FakeProvider,
            identification(),
            SessionTimeouts {
                link_negotiation: Duration::from_millis(6),
                link_retry: Duration::from_millis(2),
                ..fast_timeouts()
            },
        )
        .expect_err("missing peer");
        assert!(matches!(error, SessionError::LinkNegotiationTimeout));
        let written = &state.lock().expect("fixture").written;
        assert!(written.len() >= DETECT_MARKER.len() + 29 * 2);
    }

    #[test]
    fn peer_reset_during_setup_fails_closed() {
        let mut input = peer_synchronization(7);
        input.extend_from_slice(
            &LinkPacket::header_only(ControlBits::RST, 8, INITIAL_ACCESSORY_SEQUENCE)
                .encode()
                .expect("reset fixture"),
        );
        let (stream, _state) = FakeStream::new([ReadStep::Data(input.into_iter().collect())]);
        let error = WiredCarkitSession::establish_with_timeouts(
            stream,
            FakeProvider,
            identification(),
            fast_timeouts(),
        )
        .expect_err("reset");
        assert!(matches!(error, SessionError::PeerReset));
    }

    #[test]
    fn malformed_availability_boolean_is_rejected() {
        let mut input = successful_input(7);
        let malformed = CsmFrame {
            message_id: CARPLAY_AVAILABILITY,
            parameters: vec![CsmParameter::new(
                0,
                crate::csm::encode_parameter_group(&[CsmParameter::new(
                    0,
                    Bytes::from_static(&[2]),
                )])
                .expect("group"),
            )],
        };
        let availability_length = device_packet(
            13,
            CarPlayAvailability {
                wired: Some(WiredCarPlayAvailability {
                    available: Some(true),
                    usb_transport_identifier: Some("usb-carkit".into()),
                }),
                wireless: None,
            }
            .to_frame()
            .expect("valid availability"),
        )
        .len();
        input.truncate(input.len() - availability_length);
        input.extend_from_slice(&device_packet(13, malformed));

        let (stream, _state) = FakeStream::new([ReadStep::Data(input.into_iter().collect())]);
        let mut session = WiredCarkitSession::establish_with_timeouts(
            stream,
            FakeProvider,
            identification(),
            fast_timeouts(),
        )
        .expect("setup");
        assert!(matches!(
            session.wait_for_wired_carplay(),
            Err(SessionError::Csm(CsmError::InvalidBoolean { value: 2, .. }))
        ));
    }

    fn wireless_identification() -> IdentificationInformation {
        IdentificationInformation::wireless_carplay(
            "Prelude",
            "Prelude",
            "Vanta Labs",
            "PR-0001",
            "0.1.0",
            "rev-a",
            [0x30, 0xe3, 0xd6, 0x03, 0x96, 0x1e],
            "Prelude-1A2B",
        )
    }

    fn wifi_configuration() -> AccessoryWifiConfiguration {
        AccessoryWifiConfiguration {
            ssid: "Prelude-1A2B".into(),
            passphrase: "abcdefgh12345678".into(),
            security_type: crate::csm::wifi::WifiSecurityType::WpaWpa2,
            channel: 36,
        }
    }

    fn wireless_start_request() -> WirelessCarPlayStartRequest {
        WirelessCarPlayStartRequest {
            ssid: "Prelude-1A2B".into(),
            passphrase: "abcdefgh12345678".into(),
            channel: 36,
            security_type: crate::csm::wifi::WifiSecurityType::WpaWpa2,
            ip_addresses: vec!["fe80::1".parse().expect("address")],
            port: 7000,
            device_identifier: "00:11:22:33:44:55".into(),
            public_key: Some("receiver-public-key".into()),
            source_version: "1.0".into(),
        }
    }

    fn wireless_input(first_sequence: u8) -> (Vec<u8>, Vec<u8>, usize) {
        let mut setup = LinkPacket::with_payload(
            ControlBits::SYN | ControlBits::ACK,
            first_sequence,
            INITIAL_ACCESSORY_SEQUENCE,
            0,
            LinkSynchronization::bluetooth()
                .encode()
                .expect("LSP fixture"),
        )
        .encode()
        .expect("SYN fixture")
        .to_vec();
        let mut sequence = first_sequence;
        let frames = [
            AuthenticationMessage::RequestCertificate {
                request_serial_number: false,
            }
            .to_frame(),
            CsmFrame::empty(START_IDENTIFICATION),
            AuthenticationMessage::RequestChallengeResponse([0x56; CHALLENGE_LEN]).to_frame(),
            CsmFrame::empty(IDENTIFICATION_ACCEPTED),
            AuthenticationMessage::Succeeded.to_frame(),
            CsmFrame::empty(REQUEST_ACCESSORY_WIFI_CONFIGURATION_INFORMATION),
            CarPlayAvailability {
                wired: None,
                wireless: Some(WirelessCarPlayAvailability {
                    available: Some(true),
                }),
            }
            .to_frame()
            .expect("availability fixture"),
        ];
        let data_packets = frames.len() + 1;
        for frame in frames {
            sequence = sequence.wrapping_add(1);
            setup.extend_from_slice(&device_packet(sequence, frame));
        }
        let after_start = device_packet(
            sequence.wrapping_add(1),
            CsmFrame::empty(REQUEST_ACCESSORY_WIFI_CONFIGURATION_INFORMATION),
        );
        (setup, after_start, data_packets)
    }

    fn acknowledgement_only_packets(mut bytes: &[u8]) -> usize {
        let mut count = 0;
        while let Some((packet, consumed)) =
            LinkPacket::decode_prefix(bytes).expect("outgoing packet")
        {
            bytes = &bytes[consumed..];
            if packet.control == ControlBits::ACK && packet.payload.is_empty() {
                count += 1;
            }
        }
        count
    }

    #[test]
    fn wireless_session_acknowledges_every_packet_and_shares_wifi() {
        let (setup, after_start, data_packets) = wireless_input(40);
        let (stream, state) = FakeStream::new([
            ReadStep::Data(setup.into_iter().collect()),
            ReadStep::Timeout,
            ReadStep::Data(after_start.into_iter().collect()),
        ]);
        let mut session = WiredCarkitSession::new_with_profile(
            stream,
            FakeProvider,
            wireless_identification(),
            fast_timeouts(),
            LinkProfile::Reliable,
        )
        .expect("session");
        session.wifi_configuration = Some(wifi_configuration());
        session.run_setup().expect("setup");
        session.wait_for_wireless_carplay().expect("availability");
        session
            .start_wireless_carplay(&wireless_start_request())
            .expect("start");
        assert_eq!(
            session
                .service_link(Duration::from_millis(20))
                .expect("idle"),
            None
        );
        assert_eq!(
            session
                .service_link(Duration::from_millis(20))
                .expect("request"),
            Some(REQUEST_ACCESSORY_WIFI_CONFIGURATION_INFORMATION)
        );

        let written = state.lock().expect("fixture").written.clone();
        let link = &written[DETECT_MARKER.len() + 29..];
        let message_ids = outgoing_control_message_ids(link);
        assert_eq!(
            message_ids
                .iter()
                .filter(|&&id| id == ACCESSORY_WIFI_CONFIGURATION_INFORMATION)
                .count(),
            2
        );
        assert!(message_ids.contains(&crate::csm::identification::IDENTIFICATION_INFORMATION));
        assert_eq!(
            message_ids.last(),
            Some(&ACCESSORY_WIFI_CONFIGURATION_INFORMATION)
        );
        assert!(message_ids.contains(&CARPLAY_START_SESSION));
        assert_eq!(acknowledgement_only_packets(link), 1 + data_packets);
    }

    #[test]
    fn tunnel_session_uses_wired_link_timing_with_wireless_identification() {
        let mut input = LinkPacket::with_payload(
            ControlBits::SYN | ControlBits::ACK,
            70,
            INITIAL_ACCESSORY_SEQUENCE,
            0,
            LinkSynchronization::wired_carplay()
                .encode()
                .expect("LSP fixture"),
        )
        .encode()
        .expect("SYN fixture")
        .to_vec();
        let frames = [
            AuthenticationMessage::RequestCertificate {
                request_serial_number: false,
            }
            .to_frame(),
            CsmFrame::empty(START_IDENTIFICATION),
            AuthenticationMessage::RequestChallengeResponse([0x56; CHALLENGE_LEN]).to_frame(),
            CsmFrame::empty(IDENTIFICATION_ACCEPTED),
            AuthenticationMessage::Succeeded.to_frame(),
            CsmFrame::empty(REQUEST_ACCESSORY_WIFI_CONFIGURATION_INFORMATION),
        ];
        let data_packets = frames.len();
        for (sequence, frame) in (71..).zip(frames) {
            input.extend_from_slice(&device_packet(sequence, frame));
        }
        let (stream, state) = FakeStream::new([ReadStep::Data(input.into_iter().collect())]);
        let mut session = WiredCarkitSession::new_wireless_tunnel(
            stream,
            FakeProvider,
            wireless_identification(),
            wifi_configuration(),
        )
        .expect("session");
        session.run_setup().expect("setup");
        assert_eq!(
            session
                .service_link(Duration::from_millis(20))
                .expect("Wi-Fi request"),
            Some(REQUEST_ACCESSORY_WIFI_CONFIGURATION_INFORMATION)
        );

        let written = state.lock().expect("fixture").written.clone();
        assert!(written.starts_with(&DETECT_MARKER));
        let (syn, _) = LinkPacket::decode_prefix(&written[DETECT_MARKER.len()..])
            .expect("SYN")
            .expect("complete SYN");
        assert_eq!(
            LinkSynchronization::decode(&syn.payload).expect("proposal"),
            LinkSynchronization::wired_carplay()
        );
        let link = &written[DETECT_MARKER.len() + 29..];
        let message_ids = outgoing_control_message_ids(link);
        assert!(message_ids.contains(&crate::csm::identification::IDENTIFICATION_INFORMATION));
        assert_eq!(
            message_ids.last(),
            Some(&ACCESSORY_WIFI_CONFIGURATION_INFORMATION)
        );
        assert!(acknowledgement_only_packets(link) < data_packets);
    }

    #[test]
    fn wireless_identification_declares_bluetooth_and_wifi_sharing() {
        let frame = wireless_identification()
            .to_frame()
            .expect("identification");
        let ids: Vec<_> = frame
            .parameters
            .iter()
            .map(|parameter| parameter.id)
            .collect();
        assert!(ids.contains(&17), "Bluetooth transport component");
        assert!(ids.contains(&24), "wireless CarPlay transport component");
        assert!(!ids.contains(&16), "no USB host transport");
        let bluetooth = frame
            .parameters
            .iter()
            .find(|parameter| parameter.id == 17)
            .expect("Bluetooth component");
        let group = crate::csm::decode_parameter_group(&bluetooth.payload).expect("group");
        let mac = group
            .iter()
            .find(|parameter| parameter.id == 3)
            .expect("MAC");
        assert_eq!(mac.payload.as_ref(), &[0x30, 0xe3, 0xd6, 0x03, 0x96, 0x1e]);
    }

    #[test]
    fn wired_identification_cannot_open_a_wireless_session() {
        let (stream, _) = FakeStream::new([]);
        assert!(matches!(
            WiredCarkitSession::new_with_profile(
                stream,
                FakeProvider,
                identification(),
                fast_timeouts(),
                LinkProfile::Reliable,
            ),
            Err(SessionError::MissingWirelessCarPlayTransport)
        ));
    }

    fn outgoing_control_message_ids(mut bytes: &[u8]) -> Vec<u16> {
        let mut message_ids = Vec::new();
        while let Some((packet, consumed)) =
            LinkPacket::decode_prefix(bytes).expect("outgoing packet")
        {
            bytes = &bytes[consumed..];
            if packet.session_id != CONTROL_SESSION_ID || packet.payload.is_empty() {
                continue;
            }
            let frame = CsmFrame::decode_exact(&packet.payload).expect("outgoing CSM");
            message_ids.push(frame.message_id);
        }
        message_ids
    }
}
