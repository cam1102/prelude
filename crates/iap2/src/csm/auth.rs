use bytes::Bytes;

use super::{CsmError, CsmFrame, CsmParameter, take_optional_unique, take_required_unique};
use prelude_mfi::{CERTIFICATE_SERIAL_LEN, CHALLENGE_LEN, RESPONSE_LEN};

pub const REQUEST_AUTHENTICATION_CERTIFICATE: u16 = 0xaa00;
pub const AUTHENTICATION_CERTIFICATE: u16 = 0xaa01;
pub const REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE: u16 = 0xaa02;
pub const AUTHENTICATION_RESPONSE: u16 = 0xaa03;
pub const AUTHENTICATION_FAILED: u16 = 0xaa04;
pub const AUTHENTICATION_SUCCEEDED: u16 = 0xaa05;
pub const ACCESSORY_AUTHENTICATION_SERIAL_NUMBER: u16 = 0xaa06;

pub const SENT_BY_ACCESSORY_CONTROL_V2: &[u16] = &[
    AUTHENTICATION_CERTIFICATE,
    AUTHENTICATION_RESPONSE,
    ACCESSORY_AUTHENTICATION_SERIAL_NUMBER,
];
pub const RECEIVED_BY_ACCESSORY_CONTROL_V2: &[u16] = &[
    REQUEST_AUTHENTICATION_CERTIFICATE,
    REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE,
    AUTHENTICATION_FAILED,
    AUTHENTICATION_SUCCEEDED,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthenticationMessage {
    RequestCertificate { request_serial_number: bool },
    Certificate(Bytes),
    RequestChallengeResponse([u8; CHALLENGE_LEN]),
    Response([u8; RESPONSE_LEN]),
    Failed,
    Succeeded,
    AccessorySerialNumber([u8; CERTIFICATE_SERIAL_LEN]),
}

impl AuthenticationMessage {
    pub fn to_frame(&self) -> CsmFrame {
        match self {
            Self::RequestCertificate {
                request_serial_number,
            } => CsmFrame {
                message_id: REQUEST_AUTHENTICATION_CERTIFICATE,
                parameters: if *request_serial_number {
                    vec![CsmParameter::presence(0)]
                } else {
                    Vec::new()
                },
            },
            Self::Certificate(certificate) => CsmFrame {
                message_id: AUTHENTICATION_CERTIFICATE,
                parameters: vec![CsmParameter::new(0, certificate.clone())],
            },
            Self::RequestChallengeResponse(challenge) => CsmFrame {
                message_id: REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE,
                parameters: vec![CsmParameter::new(0, Bytes::copy_from_slice(challenge))],
            },
            Self::Response(response) => CsmFrame {
                message_id: AUTHENTICATION_RESPONSE,
                parameters: vec![CsmParameter::new(0, Bytes::copy_from_slice(response))],
            },
            Self::Failed => CsmFrame::empty(AUTHENTICATION_FAILED),
            Self::Succeeded => CsmFrame::empty(AUTHENTICATION_SUCCEEDED),
            Self::AccessorySerialNumber(serial) => CsmFrame {
                message_id: ACCESSORY_AUTHENTICATION_SERIAL_NUMBER,
                parameters: vec![CsmParameter::new(0, Bytes::copy_from_slice(serial))],
            },
        }
    }

    pub fn from_frame(frame: &CsmFrame) -> Result<Self, CsmError> {
        let mut parameters = frame.parameters.clone();
        let message = match frame.message_id {
            REQUEST_AUTHENTICATION_CERTIFICATE => {
                let serial = take_optional_unique(&mut parameters, 0)?;
                if let Some(payload) = &serial {
                    decode_array::<0>(0, payload)?;
                }
                Self::RequestCertificate {
                    request_serial_number: serial.is_some(),
                }
            }
            AUTHENTICATION_CERTIFICATE => {
                let certificate = take_required_unique(&mut parameters, 0)?;
                if certificate.is_empty() {
                    return Err(CsmError::InvalidPayloadLength {
                        id: 0,
                        expected: 1,
                        actual: 0,
                    });
                }
                Self::Certificate(certificate)
            }
            REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE => {
                let challenge = take_required_unique(&mut parameters, 0)?;
                Self::RequestChallengeResponse(decode_array(0, &challenge)?)
            }
            AUTHENTICATION_RESPONSE => {
                let response = take_required_unique(&mut parameters, 0)?;
                Self::Response(decode_array(0, &response)?)
            }
            AUTHENTICATION_FAILED => Self::Failed,
            AUTHENTICATION_SUCCEEDED => Self::Succeeded,
            ACCESSORY_AUTHENTICATION_SERIAL_NUMBER => {
                let serial = take_required_unique(&mut parameters, 0)?;
                Self::AccessorySerialNumber(decode_array(0, &serial)?)
            }
            actual => return Err(CsmError::UnsupportedMessage(actual)),
        };
        if let Some(parameter) = parameters.first() {
            return Err(CsmError::UnexpectedParameter(parameter.id));
        }
        Ok(message)
    }
}

fn decode_array<const N: usize>(id: u16, payload: &[u8]) -> Result<[u8; N], CsmError> {
    payload
        .try_into()
        .map_err(|_| CsmError::InvalidPayloadLength {
            id,
            expected: N,
            actual: payload.len(),
        })
}
