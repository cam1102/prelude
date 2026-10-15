use bitflags::bitflags;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use thiserror::Error;

pub const LINK_HEADER_LEN: usize = 9;
pub const PAYLOAD_CHECKSUM_LEN: usize = 1;
pub const LINK_MAGIC: [u8; 2] = [0xff, 0x5a];
pub const DETECT_MARKER: [u8; 6] = [0xff, 0x55, 0x02, 0x00, 0xee, 0x10];
pub const CONTROL_SESSION_ID: u8 = 10;
pub const EXTERNAL_ACCESSORY_SESSION_ID: u8 = 11;
pub const FILE_TRANSFER_SESSION_ID: u8 = 12;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct ControlBits: u8 {
        const SYN = 0x80;
        const ACK = 0x40;
        const EAK = 0x20;
        const RST = 0x10;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SessionType {
    Control = 0,
    FileTransfer = 1,
    ExternalAccessory = 2,
}

impl TryFrom<u8> for SessionType {
    type Error = FrameError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Control),
            1 => Ok(Self::FileTransfer),
            2 => Ok(Self::ExternalAccessory),
            other => Err(FrameError::InvalidSessionType(other)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionDescriptor {
    pub id: u8,
    pub session_type: SessionType,
    pub version: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSynchronization {
    pub version: u8,
    pub max_outgoing: u8,
    pub max_packet_length: u16,
    pub retransmission_timeout_ms: u16,
    pub acknowledgement_timeout_ms: u16,
    pub max_retransmissions: u8,
    pub max_cumulative_acknowledgements: u8,
    pub sessions: Vec<SessionDescriptor>,
}

impl LinkSynchronization {
    pub fn bluetooth() -> Self {
        Self {
            retransmission_timeout_ms: 4_000,
            acknowledgement_timeout_ms: 500,
            max_retransmissions: 4,
            max_cumulative_acknowledgements: 3,
            ..Self::wired_carplay()
        }
    }

    pub fn wired_carplay() -> Self {
        Self {
            version: 1,
            max_outgoing: 4,
            max_packet_length: u16::MAX,
            retransmission_timeout_ms: 0,
            acknowledgement_timeout_ms: 0,
            max_retransmissions: 0,
            max_cumulative_acknowledgements: 0,
            sessions: vec![
                SessionDescriptor {
                    id: CONTROL_SESSION_ID,
                    session_type: SessionType::Control,
                    version: 2,
                },
                SessionDescriptor {
                    id: EXTERNAL_ACCESSORY_SESSION_ID,
                    session_type: SessionType::ExternalAccessory,
                    version: 1,
                },
                SessionDescriptor {
                    id: FILE_TRANSFER_SESSION_ID,
                    session_type: SessionType::FileTransfer,
                    version: 2,
                },
            ],
        }
    }

    pub fn validate(&self) -> Result<(), FrameError> {
        if self.version != 1 {
            return Err(FrameError::UnsupportedLinkVersion(self.version));
        }
        if !(1..=127).contains(&self.max_outgoing) || self.max_packet_length < 24 {
            return Err(FrameError::InvalidLinkParameters);
        }
        let mut ids = [false; 256];
        let mut control_v2 = false;
        for session in &self.sessions {
            if session.id == 0 || ids[usize::from(session.id)] {
                return Err(FrameError::DuplicateOrReservedSessionId(session.id));
            }
            ids[usize::from(session.id)] = true;
            if session.session_type == SessionType::Control && session.version == 2 {
                control_v2 = true;
            }
        }
        if !control_v2 {
            return Err(FrameError::MissingControlSessionVersionTwo);
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Bytes, FrameError> {
        self.validate()?;
        let length = 10usize
            .checked_add(self.sessions.len().saturating_mul(3))
            .ok_or(FrameError::LengthOverflow)?;
        if length > usize::from(u16::MAX) {
            return Err(FrameError::LengthOverflow);
        }
        let mut output = BytesMut::with_capacity(length);
        output.put_u8(self.version);
        output.put_u8(self.max_outgoing);
        output.put_u16(self.max_packet_length);
        output.put_u16(self.retransmission_timeout_ms);
        output.put_u16(self.acknowledgement_timeout_ms);
        output.put_u8(self.max_retransmissions);
        output.put_u8(self.max_cumulative_acknowledgements);
        for session in &self.sessions {
            output.put_u8(session.id);
            output.put_u8(session.session_type as u8);
            output.put_u8(session.version);
        }
        Ok(output.freeze())
    }

    pub fn decode(input: &[u8]) -> Result<Self, FrameError> {
        if input.len() < 10 {
            return Err(FrameError::Incomplete);
        }
        if (input.len() - 10) % 3 != 0 {
            return Err(FrameError::InvalidSessionListLength(input.len() - 10));
        }
        let sessions = input[10..]
            .chunks_exact(3)
            .map(|chunk| {
                Ok(SessionDescriptor {
                    id: chunk[0],
                    session_type: SessionType::try_from(chunk[1])?,
                    version: chunk[2],
                })
            })
            .collect::<Result<Vec<_>, FrameError>>()?;
        let synchronization = Self {
            version: input[0],
            max_outgoing: input[1],
            max_packet_length: u16::from_be_bytes([input[2], input[3]]),
            retransmission_timeout_ms: u16::from_be_bytes([input[4], input[5]]),
            acknowledgement_timeout_ms: u16::from_be_bytes([input[6], input[7]]),
            max_retransmissions: input[8],
            max_cumulative_acknowledgements: input[9],
            sessions,
        };
        synchronization.validate()?;
        Ok(synchronization)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkPacket {
    pub control: ControlBits,
    pub sequence: u8,
    pub acknowledgement: u8,
    pub session_id: u8,
    pub payload: Bytes,
}

impl LinkPacket {
    pub fn header_only(control: ControlBits, sequence: u8, acknowledgement: u8) -> Self {
        Self {
            control,
            sequence,
            acknowledgement,
            session_id: 0,
            payload: Bytes::new(),
        }
    }

    pub fn with_payload(
        control: ControlBits,
        sequence: u8,
        acknowledgement: u8,
        session_id: u8,
        payload: impl Into<Bytes>,
    ) -> Self {
        Self {
            control,
            sequence,
            acknowledgement,
            session_id,
            payload: payload.into(),
        }
    }

    pub fn encode(&self) -> Result<Bytes, FrameError> {
        let payload_trailer = usize::from(!self.payload.is_empty());
        let length = LINK_HEADER_LEN
            .checked_add(self.payload.len())
            .and_then(|value| value.checked_add(payload_trailer))
            .ok_or(FrameError::LengthOverflow)?;
        let length_u16 = u16::try_from(length).map_err(|_| FrameError::LengthOverflow)?;

        let mut output = BytesMut::with_capacity(length);
        output.extend_from_slice(&LINK_MAGIC);
        output.put_u16(length_u16);
        output.put_u8(self.control.bits());
        output.put_u8(self.sequence);
        output.put_u8(self.acknowledgement);
        output.put_u8(self.session_id);
        let header_checksum = checksum(&output[..8]);
        output.put_u8(header_checksum);
        if !self.payload.is_empty() {
            output.extend_from_slice(&self.payload);
            output.put_u8(checksum(&self.payload));
        }
        Ok(output.freeze())
    }

    pub fn decode_prefix(input: &[u8]) -> Result<Option<(Self, usize)>, FrameError> {
        if input.len() < LINK_HEADER_LEN {
            return Ok(None);
        }
        if input[..2] != LINK_MAGIC {
            return Err(FrameError::BadMagic([input[0], input[1]]));
        }
        if !checksum_valid(&input[..LINK_HEADER_LEN]) {
            return Err(FrameError::BadHeaderChecksum);
        }
        let length = usize::from(u16::from_be_bytes([input[2], input[3]]));
        if length < LINK_HEADER_LEN || length == LINK_HEADER_LEN + 1 {
            return Err(FrameError::InvalidDeclaredLength(length));
        }
        if input.len() < length {
            return Ok(None);
        }
        let control = ControlBits::from_bits(input[4])
            .ok_or(FrameError::UnknownControlBits(input[4] & 0x0f))?;
        let payload = if length == LINK_HEADER_LEN {
            Bytes::new()
        } else {
            if !checksum_valid(&input[LINK_HEADER_LEN..length]) {
                return Err(FrameError::BadPayloadChecksum);
            }
            Bytes::copy_from_slice(&input[LINK_HEADER_LEN..length - PAYLOAD_CHECKSUM_LEN])
        };
        Ok(Some((
            Self {
                control,
                sequence: input[5],
                acknowledgement: input[6],
                session_id: input[7],
                payload,
            },
            length,
        )))
    }
}

#[derive(Debug, Default)]
pub struct LinkPacketDecoder {
    buffer: BytesMut,
}

impl LinkPacketDecoder {
    pub fn push(&mut self, input: &[u8]) {
        self.buffer.extend_from_slice(input);
    }

    pub fn next_packet(&mut self) -> Result<Option<LinkPacket>, FrameError> {
        let Some((packet, consumed)) = LinkPacket::decode_prefix(&self.buffer)? else {
            return Ok(None);
        };
        self.buffer.advance(consumed);
        Ok(Some(packet))
    }

    pub fn buffered_len(&self) -> usize {
        self.buffer.len()
    }
}

fn checksum(input: &[u8]) -> u8 {
    let sum = input
        .iter()
        .fold(0u8, |accumulator, byte| accumulator.wrapping_add(*byte));
    0u8.wrapping_sub(sum)
}

fn checksum_valid(input: &[u8]) -> bool {
    input
        .iter()
        .fold(0u8, |accumulator, byte| accumulator.wrapping_add(*byte))
        == 0
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FrameError {
    #[error("incomplete iAP2 frame")]
    Incomplete,
    #[error("bad iAP2 link magic {0:02x?}")]
    BadMagic([u8; 2]),
    #[error("invalid iAP2 link header checksum")]
    BadHeaderChecksum,
    #[error("invalid iAP2 link payload checksum")]
    BadPayloadChecksum,
    #[error("invalid iAP2 link packet length {0}")]
    InvalidDeclaredLength(usize),
    #[error("iAP2 packet or synchronization payload exceeds its wire length field")]
    LengthOverflow,
    #[error("unknown low control bits 0x{0:02x}")]
    UnknownControlBits(u8),
    #[error("unsupported iAP2 link version {0}")]
    UnsupportedLinkVersion(u8),
    #[error("invalid iAP2 link parameters")]
    InvalidLinkParameters,
    #[error("invalid iAP2 session type {0}")]
    InvalidSessionType(u8),
    #[error("session id {0} is reserved or duplicated")]
    DuplicateOrReservedSessionId(u8),
    #[error("wired iAP2 profile is missing a control session at version 2")]
    MissingControlSessionVersionTwo,
    #[error("session-list payload length {0} is not divisible by three")]
    InvalidSessionListLength(usize),
}
