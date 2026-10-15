use serde::{Deserialize, Serialize};

use crate::{Error, Result};

pub const NCM_NTH16_SIGNATURE: u32 = 0x484d_434e;
pub const NCM_NDP16_SIGNATURE: u32 = 0x304d_434e;
const NTH16_LEN: usize = 12;
const NDP16_HEADER_LEN: usize = 8;
const DATAGRAM_POINTER_LEN: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NcmFrame(Vec<u8>);

impl NcmFrame {
    pub fn new(bytes: Vec<u8>) -> Result<Self> {
        if bytes.is_empty() {
            return Err(Error::InvalidNtb("zero-length Ethernet frame".to_owned()));
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ntb16 {
    pub sequence: u16,
    pub frames: Vec<NcmFrame>,
}

impl Ntb16 {
    pub fn parse(block: &[u8]) -> Result<Self> {
        if block.len() < NTH16_LEN {
            return Err(Error::InvalidNtb("truncated NTH16 header".to_owned()));
        }
        if read_u32(block, 0)? != NCM_NTH16_SIGNATURE {
            return Err(Error::InvalidNtb("unexpected NTH16 signature".to_owned()));
        }
        if usize::from(read_u16(block, 4)?) != NTH16_LEN {
            return Err(Error::InvalidNtb(
                "unsupported NTH16 header length".to_owned(),
            ));
        }
        let sequence = read_u16(block, 6)?;
        let block_length = usize::from(read_u16(block, 8)?);
        if block_length != block.len() {
            return Err(Error::InvalidNtb(format!(
                "declared block length {block_length} does not match {} bytes",
                block.len()
            )));
        }
        let ndp_offset = usize::from(read_u16(block, 10)?);
        if ndp_offset < NTH16_LEN || ndp_offset + NDP16_HEADER_LEN > block.len() {
            return Err(Error::InvalidNtb(
                "NDP16 offset is out of bounds".to_owned(),
            ));
        }
        if read_u32(block, ndp_offset)? != NCM_NDP16_SIGNATURE {
            return Err(Error::Unsupported("NCM CRC or vendor NDP signatures"));
        }
        let ndp_length = usize::from(read_u16(block, ndp_offset + 4)?);
        if ndp_length < NDP16_HEADER_LEN + DATAGRAM_POINTER_LEN
            || ndp_length % DATAGRAM_POINTER_LEN != 0
            || ndp_offset + ndp_length > block.len()
        {
            return Err(Error::InvalidNtb("invalid NDP16 length".to_owned()));
        }
        if read_u16(block, ndp_offset + 6)? != 0 {
            return Err(Error::Unsupported("chained NDP16 tables"));
        }

        let mut frames = Vec::new();
        let mut pointer = ndp_offset + NDP16_HEADER_LEN;
        let ndp_end = ndp_offset + ndp_length;
        let mut terminated = false;
        while pointer + DATAGRAM_POINTER_LEN <= ndp_end {
            let index = usize::from(read_u16(block, pointer)?);
            let length = usize::from(read_u16(block, pointer + 2)?);
            pointer += DATAGRAM_POINTER_LEN;
            if index == 0 && length == 0 {
                terminated = true;
                break;
            }
            if index < NTH16_LEN || length == 0 || index + length > block.len() {
                return Err(Error::InvalidNtb(
                    "NDP16 datagram pointer is out of bounds".to_owned(),
                ));
            }
            let range = index..index + length;
            if range.contains(&ndp_offset) || (ndp_offset..ndp_end).contains(&index) {
                return Err(Error::InvalidNtb(
                    "NDP16 datagram overlaps its pointer table".to_owned(),
                ));
            }
            frames.push(NcmFrame::new(block[range].to_vec())?);
        }
        if !terminated {
            return Err(Error::InvalidNtb(
                "NDP16 datagram table has no terminator".to_owned(),
            ));
        }
        Ok(Self { sequence, frames })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NtbParameters {
    pub maximum_size: usize,
    pub alignment: usize,
    pub payload_divisor: usize,
    pub payload_remainder: usize,
    pub maximum_datagrams: usize,
}

impl Default for NtbParameters {
    fn default() -> Self {
        Self {
            maximum_size: 16_384,
            alignment: 4,
            payload_divisor: 4,
            payload_remainder: 0,
            maximum_datagrams: 16,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Ntb16Encoder {
    parameters: NtbParameters,
    sequence: u16,
}

impl Ntb16Encoder {
    pub fn new(parameters: NtbParameters) -> Result<Self> {
        if parameters.maximum_size > usize::from(u16::MAX)
            || parameters.maximum_size < NTH16_LEN + NDP16_HEADER_LEN + DATAGRAM_POINTER_LEN
            || parameters.alignment == 0
            || parameters.alignment > parameters.maximum_size
            || parameters.payload_divisor == 0
            || parameters.payload_divisor > parameters.maximum_size
            || parameters.payload_remainder >= parameters.payload_divisor
            || parameters.maximum_datagrams == 0
            || parameters.maximum_datagrams
                > (parameters.maximum_size - NTH16_LEN - NDP16_HEADER_LEN) / DATAGRAM_POINTER_LEN
        {
            return Err(Error::InvalidNtb("invalid encoder parameters".to_owned()));
        }
        Ok(Self {
            parameters,
            sequence: 0,
        })
    }

    pub fn encode(&mut self, frames: &[NcmFrame]) -> Result<Vec<u8>> {
        if frames.is_empty() {
            return Err(Error::InvalidNtb("cannot encode an empty NTB".to_owned()));
        }
        if frames.len() > self.parameters.maximum_datagrams {
            return Err(Error::InvalidNtb(
                "frame count exceeds negotiated NTB datagram limit".to_owned(),
            ));
        }
        let mut block = vec![0; NTH16_LEN];
        let mut pointers = Vec::with_capacity(frames.len());
        for frame in frames {
            align_to_remainder(
                &mut block,
                self.parameters.payload_divisor,
                self.parameters.payload_remainder,
            );
            let index = block.len();
            block.extend_from_slice(frame.as_bytes());
            pointers.push((index, frame.as_bytes().len()));
        }
        align(&mut block, self.parameters.alignment);
        let ndp_offset = block.len();
        let ndp_length = NDP16_HEADER_LEN + (pointers.len() + 1) * DATAGRAM_POINTER_LEN;
        let total = ndp_offset + ndp_length;
        if total > self.parameters.maximum_size {
            return Err(Error::FrameTooLarge {
                actual: total,
                maximum: self.parameters.maximum_size,
            });
        }
        for (index, length) in &pointers {
            if *index > usize::from(u16::MAX) || *length > usize::from(u16::MAX) {
                return Err(Error::FrameTooLarge {
                    actual: total,
                    maximum: usize::from(u16::MAX),
                });
            }
        }

        block.extend_from_slice(&NCM_NDP16_SIGNATURE.to_le_bytes());
        block.extend_from_slice(&u16_value(ndp_length, "NDP16 length")?.to_le_bytes());
        block.extend_from_slice(&0_u16.to_le_bytes());
        for (index, length) in pointers {
            block.extend_from_slice(&u16_value(index, "datagram index")?.to_le_bytes());
            block.extend_from_slice(&u16_value(length, "datagram length")?.to_le_bytes());
        }
        block.extend_from_slice(&[0; DATAGRAM_POINTER_LEN]);

        block[0..4].copy_from_slice(&NCM_NTH16_SIGNATURE.to_le_bytes());
        block[4..6].copy_from_slice(&(NTH16_LEN as u16).to_le_bytes());
        block[6..8].copy_from_slice(&self.sequence.to_le_bytes());
        let block_length = block.len();
        block[8..10].copy_from_slice(&u16_value(block_length, "NTB length")?.to_le_bytes());
        block[10..12].copy_from_slice(&u16_value(ndp_offset, "NDP16 offset")?.to_le_bytes());
        self.sequence = self.sequence.wrapping_add(1);
        Ok(block)
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| Error::InvalidNtb("truncated 16-bit field".to_owned()))?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| Error::InvalidNtb("truncated 32-bit field".to_owned()))?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn u16_value(value: usize, field: &'static str) -> Result<u16> {
    u16::try_from(value).map_err(|_| Error::InvalidNtb(format!("{field} exceeds NTB16 limits")))
}

fn align(bytes: &mut Vec<u8>, alignment: usize) {
    let padding = (alignment - (bytes.len() % alignment)) % alignment;
    bytes.resize(bytes.len() + padding, 0);
}

fn align_to_remainder(bytes: &mut Vec<u8>, divisor: usize, remainder: usize) {
    let current = bytes.len() % divisor;
    let padding = (remainder + divisor - current) % divisor;
    bytes.resize(bytes.len() + padding, 0);
}

pub trait TapEndpoint: std::fmt::Debug + Send {
    fn interface_name(&self) -> &str;
    fn receive(&mut self, timeout: std::time::Duration) -> std::io::Result<Option<NcmFrame>>;
    fn transmit(&mut self, frame: &NcmFrame) -> std::io::Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NcmBridgeState {
    Detached,
    UsbClaimed,
    TapReady,
    Running,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NcmBridgeEvent {
    UsbInterfacesClaimed,
    TapOpened,
    Started,
    Stopped,
    Fatal,
}

impl NcmBridgeState {
    pub fn apply(self, event: NcmBridgeEvent) -> Result<Self> {
        match (self, event) {
            (_, NcmBridgeEvent::Fatal) => Ok(Self::Failed),
            (_, NcmBridgeEvent::Stopped) => Ok(Self::Stopped),
            (Self::Detached, NcmBridgeEvent::UsbInterfacesClaimed) => Ok(Self::UsbClaimed),
            (Self::UsbClaimed, NcmBridgeEvent::TapOpened) => Ok(Self::TapReady),
            (Self::TapReady, NcmBridgeEvent::Started) => Ok(Self::Running),
            (state, _) => Err(Error::NotReady {
                component: "NCM bridge",
                state: match state {
                    Self::Detached => "detached",
                    Self::UsbClaimed => "usb_claimed",
                    Self::TapReady => "tap_ready",
                    Self::Running => "running",
                    Self::Stopped => "stopped",
                    Self::Failed => "failed",
                },
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn ntb_round_trip_preserves_frames_and_sequence() {
        let frames = vec![
            NcmFrame::new(vec![1, 2, 3]).expect("frame"),
            NcmFrame::new(vec![4; 70]).expect("frame"),
        ];
        let mut encoder = Ntb16Encoder::new(NtbParameters::default()).expect("encoder");
        let encoded = encoder.encode(&frames).expect("encode");
        let decoded = Ntb16::parse(&encoded).expect("decode");
        assert_eq!(decoded.sequence, 0);
        assert_eq!(decoded.frames, frames);
    }

    #[test]
    fn malformed_pointer_is_rejected() {
        let frame = NcmFrame::new(vec![1, 2, 3]).expect("frame");
        let mut encoder = Ntb16Encoder::new(NtbParameters::default()).expect("encoder");
        let mut encoded = encoder.encode(&[frame]).expect("encode");
        let ndp_offset = usize::from(u16::from_le_bytes([encoded[10], encoded[11]]));
        encoded[ndp_offset + 8..ndp_offset + 10].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(matches!(Ntb16::parse(&encoded), Err(Error::InvalidNtb(_))));
    }

    #[test]
    fn negotiated_payload_remainder_and_datagram_limit_are_enforced() {
        let parameters = NtbParameters {
            payload_divisor: 8,
            payload_remainder: 3,
            maximum_datagrams: 1,
            ..NtbParameters::default()
        };
        let frame = NcmFrame::new(vec![1, 2, 3]).expect("frame");
        let mut encoder = Ntb16Encoder::new(parameters).expect("encoder");
        let encoded = encoder
            .encode(std::slice::from_ref(&frame))
            .expect("encode");
        let ndp = usize::from(u16::from_le_bytes([encoded[10], encoded[11]]));
        let index = usize::from(u16::from_le_bytes([encoded[ndp + 8], encoded[ndp + 9]]));
        assert_eq!(index % 8, 3);
        assert!(matches!(
            encoder.encode(&[frame.clone(), frame]),
            Err(Error::InvalidNtb(_))
        ));
    }

    proptest! {
        #[test]
        fn parser_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..1024)) {
            let _ = Ntb16::parse(&bytes);
        }
    }
}
