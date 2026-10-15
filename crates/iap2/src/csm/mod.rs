use bytes::{Buf, BufMut, Bytes, BytesMut};
use thiserror::Error;

pub mod auth;
pub mod carplay;
pub mod identification;
pub mod wifi;

pub const START_MARKER: u16 = 0x4040;
pub const OUTER_HEADER_LEN: usize = 6;
pub const PARAMETER_HEADER_LEN: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsmParameter {
    pub id: u16,
    pub payload: Bytes,
}

impl CsmParameter {
    pub fn new(id: u16, payload: impl Into<Bytes>) -> Self {
        Self {
            id,
            payload: payload.into(),
        }
    }

    pub fn presence(id: u16) -> Self {
        Self::new(id, Bytes::new())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsmFrame {
    pub message_id: u16,
    pub parameters: Vec<CsmParameter>,
}

impl CsmFrame {
    pub fn empty(message_id: u16) -> Self {
        Self {
            message_id,
            parameters: Vec::new(),
        }
    }

    pub fn parameter(&self, id: u16) -> Option<&CsmParameter> {
        self.parameters.iter().find(|parameter| parameter.id == id)
    }

    pub fn encode(&self) -> Result<Bytes, CsmError> {
        let body = encode_parameters(&self.parameters)?;
        let total = OUTER_HEADER_LEN
            .checked_add(body.len())
            .ok_or(CsmError::LengthOverflow)?;
        let total = u16::try_from(total).map_err(|_| CsmError::LengthOverflow)?;
        let mut output = BytesMut::with_capacity(usize::from(total));
        output.put_u16(START_MARKER);
        output.put_u16(total);
        output.put_u16(self.message_id);
        output.extend_from_slice(&body);
        Ok(output.freeze())
    }

    pub fn decode_prefix(input: &[u8]) -> Result<Option<(Self, usize)>, CsmError> {
        if input.len() < OUTER_HEADER_LEN {
            return Ok(None);
        }
        let marker = u16::from_be_bytes([input[0], input[1]]);
        if marker != START_MARKER {
            return Err(CsmError::BadStartMarker(marker));
        }
        let length = usize::from(u16::from_be_bytes([input[2], input[3]]));
        if length < OUTER_HEADER_LEN {
            return Err(CsmError::InvalidLength(length));
        }
        if input.len() < length {
            return Ok(None);
        }
        let message_id = u16::from_be_bytes([input[4], input[5]]);
        let parameters = decode_parameters(&input[OUTER_HEADER_LEN..length])?;
        Ok(Some((
            Self {
                message_id,
                parameters,
            },
            length,
        )))
    }

    pub fn decode_exact(input: &[u8]) -> Result<Self, CsmError> {
        let (frame, consumed) = Self::decode_prefix(input)?.ok_or(CsmError::Incomplete)?;
        if consumed != input.len() {
            return Err(CsmError::TrailingBytes(input.len() - consumed));
        }
        Ok(frame)
    }
}

pub(crate) fn encode_parameters(parameters: &[CsmParameter]) -> Result<Bytes, CsmError> {
    let length = parameters.iter().try_fold(0usize, |total, parameter| {
        total
            .checked_add(PARAMETER_HEADER_LEN)
            .and_then(|value| value.checked_add(parameter.payload.len()))
            .ok_or(CsmError::LengthOverflow)
    })?;
    if length > usize::from(u16::MAX) {
        return Err(CsmError::LengthOverflow);
    }
    let mut output = BytesMut::with_capacity(length);
    for parameter in parameters {
        let parameter_length = PARAMETER_HEADER_LEN
            .checked_add(parameter.payload.len())
            .ok_or(CsmError::LengthOverflow)?;
        output.put_u16(u16::try_from(parameter_length).map_err(|_| CsmError::LengthOverflow)?);
        output.put_u16(parameter.id);
        output.extend_from_slice(&parameter.payload);
    }
    Ok(output.freeze())
}

pub fn encode_parameter_group(parameters: &[CsmParameter]) -> Result<Bytes, CsmError> {
    encode_parameters(parameters)
}

pub(crate) fn decode_parameters(input: &[u8]) -> Result<Vec<CsmParameter>, CsmError> {
    let mut input = Bytes::copy_from_slice(input);
    let mut offset = 0;
    let mut parameters = Vec::new();
    while input.has_remaining() {
        if input.remaining() < PARAMETER_HEADER_LEN {
            return Err(CsmError::TruncatedParameterHeader { offset });
        }
        let length = usize::from(input.get_u16());
        let id = input.get_u16();
        if length < PARAMETER_HEADER_LEN {
            return Err(CsmError::InvalidParameterLength { id, length });
        }
        let payload_length = length - PARAMETER_HEADER_LEN;
        if input.remaining() < payload_length {
            return Err(CsmError::TruncatedParameter {
                id,
                declared: payload_length,
                available: input.remaining(),
            });
        }
        parameters.push(CsmParameter {
            id,
            payload: input.copy_to_bytes(payload_length),
        });
        offset += length;
    }
    Ok(parameters)
}

pub fn decode_parameter_group(input: &[u8]) -> Result<Vec<CsmParameter>, CsmError> {
    decode_parameters(input)
}

pub(crate) fn take_optional_unique(
    parameters: &mut Vec<CsmParameter>,
    id: u16,
) -> Result<Option<Bytes>, CsmError> {
    let Some(index) = parameters.iter().position(|parameter| parameter.id == id) else {
        return Ok(None);
    };
    let payload = parameters.remove(index).payload;
    if parameters.iter().any(|parameter| parameter.id == id) {
        return Err(CsmError::DuplicateParameter(id));
    }
    Ok(Some(payload))
}

pub(crate) fn take_required_unique(
    parameters: &mut Vec<CsmParameter>,
    id: u16,
) -> Result<Bytes, CsmError> {
    take_optional_unique(parameters, id)?.ok_or(CsmError::MissingParameter(id))
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CsmError {
    #[error("incomplete CSM frame")]
    Incomplete,
    #[error("bad CSM start marker 0x{0:04x}")]
    BadStartMarker(u16),
    #[error("invalid CSM length {0}")]
    InvalidLength(usize),
    #[error("CSM frame contains {0} trailing bytes")]
    TrailingBytes(usize),
    #[error("CSM frame exceeds its wire length field")]
    LengthOverflow,
    #[error("truncated CSM parameter header at body offset {offset}")]
    TruncatedParameterHeader { offset: usize },
    #[error("parameter 0x{id:04x} has invalid total length {length}")]
    InvalidParameterLength { id: u16, length: usize },
    #[error(
        "parameter 0x{id:04x} declares {declared} payload bytes but only {available} are present"
    )]
    TruncatedParameter {
        id: u16,
        declared: usize,
        available: usize,
    },
    #[error("missing required CSM parameter 0x{0:04x}")]
    MissingParameter(u16),
    #[error("duplicate CSM parameter 0x{0:04x}")]
    DuplicateParameter(u16),
    #[error("unexpected CSM parameter 0x{0:04x}")]
    UnexpectedParameter(u16),
    #[error("CSM parameter 0x{id:04x} has {actual} bytes; expected {expected}")]
    InvalidPayloadLength {
        id: u16,
        expected: usize,
        actual: usize,
    },
    #[error(
        "CSM parameter 0x{id:04x} has {actual} bytes; expected an array of {element_size}-byte elements"
    )]
    InvalidArrayPayloadLength {
        id: u16,
        element_size: usize,
        actual: usize,
    },
    #[error("CSM parameter 0x{parameter_id:04x} repeats message identifier 0x{message_id:04x}")]
    DuplicateMessageIdentifier { parameter_id: u16, message_id: u16 },
    #[error("CSM message id 0x{actual:04x} does not match expected 0x{expected:04x}")]
    WrongMessageId { expected: u16, actual: u16 },
    #[error("unsupported CSM message id 0x{0:04x}")]
    UnsupportedMessage(u16),
    #[error("string parameter 0x{0:04x} is not NUL-terminated UTF-8")]
    InvalidString(u16),
    #[error("boolean parameter 0x{id:04x} has invalid value 0x{value:02x}")]
    InvalidBoolean { id: u16, value: u8 },
}
