use bytes::Bytes;

use super::{CsmError, CsmFrame, CsmParameter, carplay::string_parameter};

pub const REQUEST_ACCESSORY_WIFI_CONFIGURATION_INFORMATION: u16 = 0x5702;
pub const ACCESSORY_WIFI_CONFIGURATION_INFORMATION: u16 = 0x5703;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WifiSecurityType {
    None = 0,
    Wep = 1,
    WpaWpa2 = 2,
    Wpa3Transition = 3,
    Wpa3Only = 4,
}

#[derive(Clone, PartialEq, Eq)]
pub struct AccessoryWifiConfiguration {
    pub ssid: String,
    pub passphrase: String,
    pub security_type: WifiSecurityType,
    pub channel: u8,
}

impl std::fmt::Debug for AccessoryWifiConfiguration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AccessoryWifiConfiguration")
            .field("ssid", &self.ssid)
            .field("passphrase", &"[redacted]")
            .field("security_type", &self.security_type)
            .field("channel", &self.channel)
            .finish()
    }
}

impl AccessoryWifiConfiguration {
    pub fn to_frame(&self) -> Result<CsmFrame, CsmError> {
        Ok(CsmFrame {
            message_id: ACCESSORY_WIFI_CONFIGURATION_INFORMATION,
            parameters: vec![
                string_parameter(1, &self.ssid)?,
                string_parameter(2, &self.passphrase)?,
                CsmParameter::new(3, Bytes::copy_from_slice(&[self.security_type as u8])),
                CsmParameter::new(4, Bytes::copy_from_slice(&[self.channel])),
            ],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_uses_the_documented_parameter_ids() {
        let frame = AccessoryWifiConfiguration {
            ssid: "Prelude-1A2B".into(),
            passphrase: "abcdefgh12345678".into(),
            security_type: WifiSecurityType::WpaWpa2,
            channel: 36,
        }
        .to_frame()
        .expect("frame");
        assert_eq!(frame.message_id, 0x5703);
        let ids: Vec<_> = frame
            .parameters
            .iter()
            .map(|parameter| parameter.id)
            .collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
        assert_eq!(frame.parameters[0].payload.as_ref(), b"Prelude-1A2B\0");
        assert_eq!(frame.parameters[2].payload.as_ref(), &[2]);
        assert_eq!(frame.parameters[3].payload.as_ref(), &[36]);
    }
}
