use std::{
    io::{self, Read, Write},
    net::SocketAddrV4,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use prost::Message;

use crate::{Error, Result};

pub const ANDROID_AUTO_RFCOMM_UUID: &str = "4de17a00-52cb-11e6-bdf4-0800200c9a66";
pub const DEFAULT_PROJECTION_PORT: u16 = 5288;

const HEADER_LEN: usize = 4;
const PROTOCOL_MAJOR_VERSION: i32 = 6;
const PROTOCOL_MINOR_VERSION: i32 = 0;
const VERSION_RESPONSE_GRACE: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

pub mod message_id {
    pub const START_REQUEST: u16 = 1;
    pub const INFO_REQUEST: u16 = 2;
    pub const INFO_RESPONSE: u16 = 3;
    pub const VERSION_REQUEST: u16 = 4;
    pub const VERSION_RESPONSE: u16 = 5;
    pub const CONNECT_STATUS: u16 = 6;
    pub const START_RESPONSE: u16 = 7;
    pub const PING_REQUEST: u16 = 8;
    pub const PING_RESPONSE: u16 = 9;
}

const SECURITY_MODE_WPA2_PERSONAL: i32 = 8;
const ACCESS_POINT_TYPE_STATIC: i32 = 0;
const SETUP_STATUS_SUCCESS: i32 = 0;

#[derive(Clone, PartialEq, Message)]
struct WifiVersionRequest {
    #[prost(int32, optional, tag = "1")]
    major_version: Option<i32>,
    #[prost(int32, optional, tag = "2")]
    minor_version: Option<i32>,
    #[prost(int32, repeated, packed = "false", tag = "4")]
    supported_channel_frequencies_mhz: Vec<i32>,
}

#[derive(Clone, PartialEq, Message)]
struct MobileDeviceIdentity {
    #[prost(string, optional, tag = "1")]
    mobile_device_id: Option<String>,
    #[prost(string, optional, tag = "3")]
    bluetooth_name: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct WifiVersionResponse {
    #[prost(int32, optional, tag = "1")]
    major_version: Option<i32>,
    #[prost(int32, optional, tag = "2")]
    minor_version: Option<i32>,
    #[prost(string, optional, tag = "3")]
    device_serial: Option<String>,
    #[prost(int32, optional, tag = "4")]
    status: Option<i32>,
    #[prost(message, optional, tag = "6")]
    mobile_device_identity: Option<MobileDeviceIdentity>,
}

#[derive(Clone, PartialEq, Message)]
struct WifiStartRequest {
    #[prost(string, optional, tag = "1")]
    ip_address: Option<String>,
    #[prost(int32, optional, tag = "2")]
    port: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
struct WifiInfoResponse {
    #[prost(string, optional, tag = "1")]
    ssid: Option<String>,
    #[prost(string, optional, tag = "2")]
    password: Option<String>,
    #[prost(string, optional, tag = "3")]
    bssid: Option<String>,
    #[prost(int32, optional, tag = "4")]
    security_mode: Option<i32>,
    #[prost(int32, optional, tag = "5")]
    access_point_type: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
struct WifiStatus {
    #[prost(int32, optional, tag = "1")]
    status: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
struct WifiStartResponse {
    #[prost(string, optional, tag = "1")]
    ip_address: Option<String>,
    #[prost(int32, optional, tag = "2")]
    port: Option<i32>,
    #[prost(int32, optional, tag = "3")]
    status: Option<i32>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct AccessPointCredentials {
    pub ssid: String,
    pub passphrase: String,
    pub bssid: String,
    pub frequency_mhz: u32,
}

impl std::fmt::Debug for AccessPointCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccessPointCredentials")
            .field("ssid", &self.ssid)
            .field("passphrase", &"[redacted]")
            .field("bssid", &self.bssid)
            .field("frequency_mhz", &self.frequency_mhz)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapConfig {
    pub endpoint: SocketAddrV4,
    pub access_point: AccessPointCredentials,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapEvent {
    PhoneIdentified {
        serial: Option<String>,
        instance_id: Option<String>,
    },
    CredentialsSent,
    PhoneJoined,
    JoinFailed {
        status: i32,
    },
    ProjectionStarting {
        status: i32,
    },
}

impl BootstrapEvent {
    pub const fn phone_accepted(&self) -> bool {
        matches!(
            self,
            Self::PhoneJoined
                | Self::ProjectionStarting {
                    status: SETUP_STATUS_SUCCESS
                }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub message_id: u16,
    pub body: Vec<u8>,
}

impl Frame {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let length = u16::try_from(self.body.len()).map_err(|_| Error::FrameTooLarge {
            length: self.body.len(),
        })?;
        let mut encoded = Vec::with_capacity(HEADER_LEN + self.body.len());
        encoded.extend_from_slice(&length.to_be_bytes());
        encoded.extend_from_slice(&self.message_id.to_be_bytes());
        encoded.extend_from_slice(&self.body);
        Ok(encoded)
    }

    fn new(message_id: u16, message: &impl Message) -> Self {
        Self {
            message_id,
            body: message.encode_to_vec(),
        }
    }
}

#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Frame> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while self.buffer.len() >= HEADER_LEN {
            let length = usize::from(u16::from_be_bytes([self.buffer[0], self.buffer[1]]));
            if self.buffer.len() < HEADER_LEN + length {
                break;
            }
            let message_id = u16::from_be_bytes([self.buffer[2], self.buffer[3]]);
            let body = self.buffer[HEADER_LEN..HEADER_LEN + length].to_vec();
            self.buffer.drain(..HEADER_LEN + length);
            frames.push(Frame { message_id, body });
        }
        frames
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Created,
    AwaitingVersion,
    Started,
}

#[derive(Debug)]
pub struct Bootstrap {
    config: BootstrapConfig,
    decoder: FrameDecoder,
    phase: Phase,
}

impl Bootstrap {
    pub fn new(config: BootstrapConfig) -> Self {
        Self {
            config,
            decoder: FrameDecoder::default(),
            phase: Phase::Created,
        }
    }

    pub fn start(&mut self) -> Vec<Frame> {
        if self.phase != Phase::Created {
            return Vec::new();
        }
        self.phase = Phase::AwaitingVersion;
        let frequency = i32::try_from(self.config.access_point.frequency_mhz).unwrap_or_default();
        vec![Frame::new(
            message_id::VERSION_REQUEST,
            &WifiVersionRequest {
                major_version: Some(PROTOCOL_MAJOR_VERSION),
                minor_version: Some(PROTOCOL_MINOR_VERSION),
                supported_channel_frequencies_mhz: vec![frequency],
            },
        )]
    }

    pub fn version_timed_out(&mut self) -> Vec<Frame> {
        if self.phase == Phase::AwaitingVersion {
            self.send_start()
        } else {
            Vec::new()
        }
    }

    pub const fn is_started(&self) -> bool {
        matches!(self.phase, Phase::Started)
    }

    pub fn receive(&mut self, bytes: &[u8]) -> Result<(Vec<Frame>, Vec<BootstrapEvent>)> {
        let mut outgoing = Vec::new();
        let mut events = Vec::new();
        for frame in self.decoder.push(bytes) {
            self.handle(frame, &mut outgoing, &mut events)?;
        }
        Ok((outgoing, events))
    }

    fn handle(
        &mut self,
        frame: Frame,
        outgoing: &mut Vec<Frame>,
        events: &mut Vec<BootstrapEvent>,
    ) -> Result<()> {
        match frame.message_id {
            message_id::VERSION_RESPONSE => {
                let response = WifiVersionResponse::decode(frame.body.as_slice())?;
                events.push(BootstrapEvent::PhoneIdentified {
                    serial: response.device_serial.filter(|serial| !serial.is_empty()),
                    instance_id: response
                        .mobile_device_identity
                        .and_then(|identity| identity.mobile_device_id)
                        .filter(|id| !id.is_empty()),
                });
                if self.phase == Phase::AwaitingVersion {
                    outgoing.extend(self.send_start());
                }
            }
            message_id::INFO_REQUEST => {
                let credentials = &self.config.access_point;
                outgoing.push(Frame::new(
                    message_id::INFO_RESPONSE,
                    &WifiInfoResponse {
                        ssid: Some(credentials.ssid.clone()),
                        password: Some(credentials.passphrase.clone()),
                        bssid: Some(credentials.bssid.to_ascii_lowercase()),
                        security_mode: Some(SECURITY_MODE_WPA2_PERSONAL),
                        access_point_type: Some(ACCESS_POINT_TYPE_STATIC),
                    },
                ));
                events.push(BootstrapEvent::CredentialsSent);
            }
            message_id::CONNECT_STATUS => {
                let status = WifiStatus::decode(frame.body.as_slice())?
                    .status
                    .unwrap_or(SETUP_STATUS_SUCCESS);
                events.push(if status == SETUP_STATUS_SUCCESS {
                    BootstrapEvent::PhoneJoined
                } else {
                    BootstrapEvent::JoinFailed { status }
                });
            }
            message_id::START_RESPONSE => {
                let response = WifiStartResponse::decode(frame.body.as_slice())?;
                events.push(BootstrapEvent::ProjectionStarting {
                    status: response.status.unwrap_or(SETUP_STATUS_SUCCESS),
                });
            }
            message_id::PING_REQUEST => outgoing.push(Frame {
                message_id: message_id::PING_RESPONSE,
                body: frame.body,
            }),
            other => tracing::debug!(message_id = other, "ignoring unknown WPP message"),
        }
        Ok(())
    }

    fn send_start(&mut self) -> Vec<Frame> {
        self.phase = Phase::Started;
        let endpoint = self.config.endpoint;
        vec![Frame::new(
            message_id::START_REQUEST,
            &WifiStartRequest {
                ip_address: Some(endpoint.ip().to_string()),
                port: Some(i32::from(endpoint.port())),
            },
        )]
    }
}

pub fn run_bootstrap<S: Read + Write>(
    stream: &mut S,
    config: BootstrapConfig,
    cancel: &Arc<AtomicBool>,
    mut on_event: impl FnMut(BootstrapEvent),
) -> Result<()> {
    let mut bootstrap = Bootstrap::new(config);
    write_frames(stream, bootstrap.start())?;
    let version_deadline = Instant::now() + VERSION_RESPONSE_GRACE;
    let mut buffer = [0_u8; 4096];
    while !cancel.load(Ordering::Relaxed) {
        if !bootstrap.is_started() && Instant::now() >= version_deadline {
            write_frames(stream, bootstrap.version_timed_out())?;
        }
        let read = match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(Error::Io(error)),
        };
        let (outgoing, events) = bootstrap.receive(&buffer[..read])?;
        write_frames(stream, outgoing)?;
        events.into_iter().for_each(&mut on_event);
    }
    Ok(())
}

pub const fn poll_interval() -> Duration {
    POLL_INTERVAL
}

fn write_frames<S: Write>(stream: &mut S, frames: Vec<Frame>) -> Result<()> {
    for frame in frames {
        stream.write_all(&frame.encode()?)?;
    }
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn config() -> BootstrapConfig {
        BootstrapConfig {
            endpoint: SocketAddrV4::new(Ipv4Addr::new(10, 47, 0, 1), DEFAULT_PROJECTION_PORT),
            access_point: AccessPointCredentials {
                ssid: "Prelude-1A2B".into(),
                passphrase: "correct horse".into(),
                bssid: "AA:BB:CC:DD:EE:FF".into(),
                frequency_mhz: 5180,
            },
        }
    }

    fn phone_frame(message_id: u16, message: &impl Message) -> Vec<u8> {
        Frame::new(message_id, message).encode().expect("frame")
    }

    #[test]
    fn opens_with_the_version_and_access_point_frequency() {
        let mut bootstrap = Bootstrap::new(config());
        let frames = bootstrap.start();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].message_id, message_id::VERSION_REQUEST);
        let request = WifiVersionRequest::decode(frames[0].body.as_slice()).expect("decode");
        assert_eq!(request.major_version, Some(6));
        assert_eq!(request.supported_channel_frequencies_mhz, vec![5180]);
        assert!(bootstrap.start().is_empty());
    }

    #[test]
    fn version_response_triggers_the_start_request_once() {
        let mut bootstrap = Bootstrap::new(config());
        bootstrap.start();
        let response = WifiVersionResponse {
            device_serial: Some("SERIAL".into()),
            mobile_device_identity: Some(MobileDeviceIdentity {
                mobile_device_id: Some("instance".into()),
                bluetooth_name: None,
            }),
            ..WifiVersionResponse::default()
        };
        let (outgoing, events) = bootstrap
            .receive(&phone_frame(message_id::VERSION_RESPONSE, &response))
            .expect("receive");
        assert_eq!(
            events,
            vec![BootstrapEvent::PhoneIdentified {
                serial: Some("SERIAL".into()),
                instance_id: Some("instance".into()),
            }]
        );
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].message_id, message_id::START_REQUEST);
        let start = WifiStartRequest::decode(outgoing[0].body.as_slice()).expect("decode");
        assert_eq!(start.ip_address.as_deref(), Some("10.47.0.1"));
        assert_eq!(start.port, Some(5288));
        assert!(bootstrap.version_timed_out().is_empty());
    }

    #[test]
    fn a_silent_phone_still_gets_the_start_request() {
        let mut bootstrap = Bootstrap::new(config());
        bootstrap.start();
        let frames = bootstrap.version_timed_out();
        assert_eq!(frames[0].message_id, message_id::START_REQUEST);
        assert!(bootstrap.is_started());
    }

    #[test]
    fn info_request_returns_wpa2_static_credentials() {
        let mut bootstrap = Bootstrap::new(config());
        bootstrap.start();
        let (outgoing, events) = bootstrap
            .receive(
                &Frame {
                    message_id: message_id::INFO_REQUEST,
                    body: Vec::new(),
                }
                .encode()
                .expect("frame"),
            )
            .expect("receive");
        assert_eq!(events, vec![BootstrapEvent::CredentialsSent]);
        let info = WifiInfoResponse::decode(outgoing[0].body.as_slice()).expect("decode");
        assert_eq!(outgoing[0].message_id, message_id::INFO_RESPONSE);
        assert_eq!(info.ssid.as_deref(), Some("Prelude-1A2B"));
        assert_eq!(info.password.as_deref(), Some("correct horse"));
        assert_eq!(info.bssid.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
        assert_eq!(info.security_mode, Some(8));
        assert_eq!(info.access_point_type, Some(0));
    }

    #[test]
    fn connect_status_and_pings_are_handled() {
        let mut bootstrap = Bootstrap::new(config());
        bootstrap.start();
        let mut bytes = phone_frame(message_id::CONNECT_STATUS, &WifiStatus { status: Some(0) });
        bytes.extend(phone_frame(
            message_id::CONNECT_STATUS,
            &WifiStatus { status: Some(-3) },
        ));
        bytes.extend(
            Frame {
                message_id: message_id::PING_REQUEST,
                body: vec![8, 42],
            }
            .encode()
            .expect("frame"),
        );
        let (outgoing, events) = bootstrap.receive(&bytes).expect("receive");
        assert_eq!(
            events,
            vec![
                BootstrapEvent::PhoneJoined,
                BootstrapEvent::JoinFailed { status: -3 }
            ]
        );
        assert_eq!(
            outgoing,
            vec![Frame {
                message_id: message_id::PING_RESPONSE,
                body: vec![8, 42]
            }]
        );
    }

    #[test]
    fn a_successful_start_response_counts_as_an_accepted_phone() {
        assert!(BootstrapEvent::PhoneJoined.phone_accepted());
        assert!(BootstrapEvent::ProjectionStarting { status: 0 }.phone_accepted());
        assert!(!BootstrapEvent::ProjectionStarting { status: -1 }.phone_accepted());
        assert!(!BootstrapEvent::CredentialsSent.phone_accepted());
    }

    #[test]
    fn decoder_reassembles_split_frames() {
        let encoded = Frame {
            message_id: 7,
            body: vec![1, 2, 3],
        }
        .encode()
        .expect("frame");
        let mut decoder = FrameDecoder::default();
        assert!(decoder.push(&encoded[..3]).is_empty());
        let frames = decoder.push(&encoded[3..]);
        assert_eq!(
            frames,
            vec![Frame {
                message_id: 7,
                body: vec![1, 2, 3]
            }]
        );
    }

    #[test]
    fn oversized_frames_are_rejected() {
        let frame = Frame {
            message_id: 1,
            body: vec![0; usize::from(u16::MAX) + 1],
        };
        assert!(matches!(frame.encode(), Err(Error::FrameTooLarge { .. })));
    }
}
