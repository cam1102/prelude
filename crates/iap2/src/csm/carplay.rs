use bytes::{BufMut, Bytes, BytesMut};

use super::{
    CsmError, CsmFrame, CsmParameter, decode_parameter_group, encode_parameters,
    take_optional_unique, take_required_unique,
};

pub const CARPLAY_AVAILABILITY: u16 = 0x4300;
pub const CARPLAY_START_SESSION: u16 = 0x4301;
pub const WIRELESS_CARPLAY_UPDATE: u16 = 0x4e0d;
pub const DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION: u16 = 0x4e0e;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WiredCarPlayAvailability {
    pub available: Option<bool>,
    pub usb_transport_identifier: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WirelessCarPlayAvailability {
    pub available: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarPlayAvailability {
    pub wired: Option<WiredCarPlayAvailability>,
    pub wireless: Option<WirelessCarPlayAvailability>,
}

impl CarPlayAvailability {
    pub fn to_frame(&self) -> Result<CsmFrame, CsmError> {
        let mut parameters = Vec::new();
        if let Some(wired) = &self.wired {
            let mut group = Vec::new();
            push_optional_bool(&mut group, 0, wired.available);
            push_optional_string(&mut group, 1, wired.usb_transport_identifier.as_deref())?;
            parameters.push(CsmParameter::new(0, encode_parameters(&group)?));
        }
        if let Some(wireless) = &self.wireless {
            let mut group = Vec::new();
            push_optional_bool(&mut group, 0, wireless.available);
            parameters.push(CsmParameter::new(1, encode_parameters(&group)?));
        }
        Ok(CsmFrame {
            message_id: CARPLAY_AVAILABILITY,
            parameters,
        })
    }

    pub fn from_frame(frame: &CsmFrame) -> Result<Self, CsmError> {
        if frame.message_id != CARPLAY_AVAILABILITY {
            return Err(CsmError::WrongMessageId {
                expected: CARPLAY_AVAILABILITY,
                actual: frame.message_id,
            });
        }
        let mut parameters = frame.parameters.clone();
        let wired = take_optional_unique(&mut parameters, 0)?
            .map(|payload| decode_wired_availability(&payload))
            .transpose()?;
        let wireless = take_optional_unique(&mut parameters, 1)?
            .map(|payload| decode_wireless_availability(&payload))
            .transpose()?;
        reject_unexpected(&parameters)?;
        Ok(Self { wired, wireless })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceTransportIdentifiers {
    pub usb_transport_identifier: String,
}

impl DeviceTransportIdentifiers {
    pub fn from_frame(frame: &CsmFrame) -> Result<Self, CsmError> {
        if frame.message_id != DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION {
            return Err(CsmError::WrongMessageId {
                expected: DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION,
                actual: frame.message_id,
            });
        }
        let mut parameters = frame.parameters.clone();
        take_optional_unique(&mut parameters, 0)?;
        let usb_transport_identifier =
            decode_string(1, &take_required_unique(&mut parameters, 1)?)?;
        reject_unexpected(&parameters)?;
        Ok(Self {
            usb_transport_identifier,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WiredCarPlayStart {
    pub ip_addresses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WirelessCarPlayStart {
    pub ssid: Option<String>,
    pub passphrase: Option<String>,
    pub channel: Option<u8>,
    pub ip_addresses: Vec<String>,
    pub security_type: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarPlayStartSession {
    pub wired: Option<WiredCarPlayStart>,
    pub wireless: Option<WirelessCarPlayStart>,
    pub port: Option<u32>,
    pub device_identifier: Option<String>,
    pub public_key: Option<String>,
    pub source_version: Option<String>,
}

impl CarPlayStartSession {
    pub fn to_frame(&self) -> Result<CsmFrame, CsmError> {
        let mut parameters = Vec::new();
        if let Some(wired) = &self.wired {
            let mut group = Vec::new();
            for address in &wired.ip_addresses {
                group.push(string_parameter(0, address)?);
            }
            parameters.push(CsmParameter::new(0, encode_parameters(&group)?));
        }
        if let Some(wireless) = &self.wireless {
            let mut group = Vec::new();
            push_optional_string(&mut group, 0, wireless.ssid.as_deref())?;
            push_optional_string(&mut group, 1, wireless.passphrase.as_deref())?;
            if let Some(channel) = wireless.channel {
                group.push(CsmParameter::new(2, Bytes::copy_from_slice(&[channel])));
            }
            for address in &wireless.ip_addresses {
                group.push(string_parameter(3, address)?);
            }
            if let Some(security_type) = wireless.security_type {
                group.push(CsmParameter::new(
                    4,
                    Bytes::copy_from_slice(&[security_type]),
                ));
            }
            parameters.push(CsmParameter::new(1, encode_parameters(&group)?));
        }
        if let Some(port) = self.port {
            parameters.push(CsmParameter::new(
                2,
                Bytes::copy_from_slice(&port.to_be_bytes()),
            ));
        }
        push_optional_string(&mut parameters, 3, self.device_identifier.as_deref())?;
        push_optional_string(&mut parameters, 4, self.public_key.as_deref())?;
        push_optional_string(&mut parameters, 5, self.source_version.as_deref())?;
        Ok(CsmFrame {
            message_id: CARPLAY_START_SESSION,
            parameters,
        })
    }
}

fn decode_wired_availability(payload: &[u8]) -> Result<WiredCarPlayAvailability, CsmError> {
    let mut parameters = decode_parameter_group(payload)?;
    let available = take_optional_unique(&mut parameters, 0)?
        .map(|payload| decode_bool(0, &payload))
        .transpose()?;
    let usb_transport_identifier = take_optional_unique(&mut parameters, 1)?
        .map(|payload| decode_string(1, &payload))
        .transpose()?;
    reject_unexpected(&parameters)?;
    Ok(WiredCarPlayAvailability {
        available,
        usb_transport_identifier,
    })
}

fn decode_wireless_availability(payload: &[u8]) -> Result<WirelessCarPlayAvailability, CsmError> {
    let mut parameters = decode_parameter_group(payload)?;
    let available = take_optional_unique(&mut parameters, 0)?
        .map(|payload| decode_bool(0, &payload))
        .transpose()?;
    take_optional_unique(&mut parameters, 1)?;
    reject_unexpected(&parameters)?;
    Ok(WirelessCarPlayAvailability { available })
}

fn decode_bool(id: u16, payload: &[u8]) -> Result<bool, CsmError> {
    if payload.len() != 1 {
        return Err(CsmError::InvalidPayloadLength {
            id,
            expected: 1,
            actual: payload.len(),
        });
    }
    match payload[0] {
        0 => Ok(false),
        1 => Ok(true),
        value => Err(CsmError::InvalidBoolean { id, value }),
    }
}

fn decode_string(id: u16, payload: &[u8]) -> Result<String, CsmError> {
    let Some((&0, value)) = payload.split_last() else {
        return Err(CsmError::InvalidString(id));
    };
    if value.contains(&0) {
        return Err(CsmError::InvalidString(id));
    }
    std::str::from_utf8(value)
        .map(str::to_owned)
        .map_err(|_| CsmError::InvalidString(id))
}

fn reject_unexpected(parameters: &[CsmParameter]) -> Result<(), CsmError> {
    if let Some(parameter) = parameters.first() {
        return Err(CsmError::UnexpectedParameter(parameter.id));
    }
    Ok(())
}

fn push_optional_bool(parameters: &mut Vec<CsmParameter>, id: u16, value: Option<bool>) {
    if let Some(value) = value {
        parameters.push(CsmParameter::new(
            id,
            Bytes::copy_from_slice(&[u8::from(value)]),
        ));
    }
}

fn push_optional_string(
    parameters: &mut Vec<CsmParameter>,
    id: u16,
    value: Option<&str>,
) -> Result<(), CsmError> {
    if let Some(value) = value {
        parameters.push(string_parameter(id, value)?);
    }
    Ok(())
}

pub(crate) fn string_parameter(id: u16, value: &str) -> Result<CsmParameter, CsmError> {
    if value.is_empty() || value.as_bytes().contains(&0) {
        return Err(CsmError::InvalidString(id));
    }
    let mut payload = BytesMut::with_capacity(value.len() + 1);
    payload.extend_from_slice(value.as_bytes());
    payload.put_u8(0);
    Ok(CsmParameter::new(id, payload.freeze()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_wireless_transport_identifier_is_ignored() {
        let wireless = encode_parameters(&[
            CsmParameter::new(0, Bytes::from_static(&[0])),
            CsmParameter::new(1, Bytes::from_static(b"legacy-without-a-nul")),
        ])
        .expect("wireless group");
        let frame = CsmFrame {
            message_id: CARPLAY_AVAILABILITY,
            parameters: vec![CsmParameter::new(1, wireless)],
        };

        let availability = CarPlayAvailability::from_frame(&frame).expect("availability");
        assert_eq!(
            availability.wireless,
            Some(WirelessCarPlayAvailability {
                available: Some(false),
            })
        );
    }

    #[test]
    fn legacy_device_transport_identifier_is_ignored() {
        let frame = CsmFrame {
            message_id: DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION,
            parameters: vec![
                CsmParameter::new(0, Bytes::from_static(b"legacy-without-a-nul")),
                string_parameter(1, "usb-carkit").expect("USB identifier"),
            ],
        };

        assert_eq!(
            DeviceTransportIdentifiers::from_frame(&frame).expect("transport identifiers"),
            DeviceTransportIdentifiers {
                usb_transport_identifier: "usb-carkit".to_owned(),
            }
        );
    }
}
