use serde::{Deserialize, Serialize};

use crate::{Error, Result};

pub const TOUCH_HID_UID: u32 = 0x2a2a_2a2a;
pub const KNOB_HID_UID: u32 = 0x2a2a_2a2b;
pub const MEDIA_HID_UID: u32 = 0x2a2a_2a2c;
pub const TELEPHONY_HID_UID: u32 = 0x2a2a_2a2d;
pub const TOUCH_CONTACTS: usize = 2;
pub const TOUCH_REPORT_LENGTH: usize = TOUCH_CONTACTS * 6;

pub const CARPLAY_WIDTH_MAX: u16 = 800;
pub const CARPLAY_HEIGHT_MAX: u16 = 480;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HidDeviceKind {
    Touch,
    Knob,
    Media,
    Telephony,
}

impl HidDeviceKind {
    pub const fn uid(self) -> u32 {
        match self {
            Self::Touch => TOUCH_HID_UID,
            Self::Knob => KNOB_HID_UID,
            Self::Media => MEDIA_HID_UID,
            Self::Telephony => TELEPHONY_HID_UID,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidDeviceDeclaration {
    pub kind: HidDeviceKind,
    pub product_id: u16,
    pub vendor_id: u16,
    pub country_code: u8,
    pub uuid: String,
    pub name: &'static str,
    pub display_uuid: String,
    pub descriptor: Vec<u8>,
}

pub fn carplay_hid_devices(display_uuid: impl Into<String>) -> [HidDeviceDeclaration; 4] {
    let display_uuid = display_uuid.into();
    [
        declaration(
            HidDeviceKind::Touch,
            "LIVI Touchscreen",
            display_uuid.clone(),
            touch_descriptor(),
        ),
        declaration(
            HidDeviceKind::Knob,
            "LIVI Knob",
            display_uuid.clone(),
            knob_descriptor().to_vec(),
        ),
        declaration(
            HidDeviceKind::Media,
            "LIVI Media",
            display_uuid.clone(),
            media_descriptor().to_vec(),
        ),
        declaration(
            HidDeviceKind::Telephony,
            "LIVI Telephony",
            display_uuid,
            telephony_descriptor().to_vec(),
        ),
    ]
}

fn declaration(
    kind: HidDeviceKind,
    name: &'static str,
    display_uuid: String,
    descriptor: Vec<u8>,
) -> HidDeviceDeclaration {
    HidDeviceDeclaration {
        kind,
        product_id: 1,
        vendor_id: 2,
        country_code: 0,
        uuid: format!("{:x}", kind.uid()),
        name,
        display_uuid,
        descriptor,
    }
}

pub fn touch_descriptor() -> Vec<u8> {
    let mut descriptor = vec![0x05, 0x0d, 0x09, 0x04, 0xa1, 0x01];
    for _ in 0..TOUCH_CONTACTS {
        descriptor.extend_from_slice(&finger_descriptor());
    }
    descriptor.push(0xc0);
    descriptor
}

fn finger_descriptor() -> [u8; 51] {
    let [x_lo, x_hi] = CARPLAY_WIDTH_MAX.to_le_bytes();
    let [y_lo, y_hi] = CARPLAY_HEIGHT_MAX.to_le_bytes();
    [
        0x05, 0x0d, 0x09, 0x22, 0xa1, 0x02, 0x09, 0x38, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, 0x15,
        0x00, 0x25, 0x01, 0x09, 0x33, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x95, 0x07, 0x81, 0x03,
        0x05, 0x01, 0x26, x_lo, x_hi, 0x09, 0x30, 0x75, 0x10, 0x95, 0x01, 0x81, 0x02, 0x26, y_lo,
        y_hi, 0x09, 0x31, 0x81, 0x02, 0xc0,
    ]
}

pub const fn knob_descriptor() -> &'static [u8] {
    &[
        0x05, 0x01, 0x09, 0x08, 0xa1, 0x01, 0x05, 0x09, 0x09, 0x01, 0x15, 0x00, 0x25, 0x01, 0x75,
        0x01, 0x95, 0x01, 0x81, 0x02, 0x05, 0x0c, 0x0a, 0x23, 0x02, 0x0a, 0x24, 0x02, 0x95, 0x02,
        0x81, 0x02, 0x95, 0x05, 0x81, 0x01, 0x05, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x09, 0x30, 0x09,
        0x31, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02, 0xc0, 0x09, 0x38, 0x15,
        0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0xc0,
    ]
}

pub const fn media_descriptor() -> &'static [u8] {
    &[
        0x05, 0x0c, 0x09, 0x01, 0xa1, 0x01, 0x15, 0x00, 0x25, 0x06, 0x05, 0x0c, 0x0a, 0x00, 0x00,
        0x0a, 0xb0, 0x00, 0x0a, 0xb1, 0x00, 0x0a, 0xcd, 0x00, 0x0a, 0xb5, 0x00, 0x0a, 0xb6, 0x00,
        0x0a, 0x9e, 0x02, 0x75, 0x08, 0x95, 0x01, 0x81, 0x00, 0xc0,
    ]
}

pub const fn telephony_descriptor() -> &'static [u8] {
    &[
        0x05, 0x0b, 0x09, 0x07, 0xa1, 0x01, 0x15, 0x00, 0x25, 0x11, 0x05, 0x0b, 0x09, 0x00, 0x09,
        0x20, 0x09, 0x21, 0x09, 0x26, 0x09, 0x2f, 0x09, 0xb0, 0x09, 0xb1, 0x09, 0xb2, 0x09, 0xb3,
        0x09, 0xb4, 0x09, 0xb5, 0x09, 0xb6, 0x09, 0xb7, 0x09, 0xb8, 0x09, 0xb9, 0x09, 0xba, 0x09,
        0xbb, 0x05, 0x07, 0x09, 0x2a, 0x75, 0x08, 0x95, 0x01, 0x81, 0x00, 0xc0,
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TouchSample {
    pub down: bool,
    pub x: u16,
    pub y: u16,
}

impl TouchSample {
    fn validate(self) -> Result<Self> {
        if self.x > CARPLAY_WIDTH_MAX || self.y > CARPLAY_HEIGHT_MAX {
            return Err(Error::InvalidInput(format!(
                "touch coordinate ({}, {}) exceeds inclusive HID maximum ({CARPLAY_WIDTH_MAX}, {CARPLAY_HEIGHT_MAX})",
                self.x, self.y
            )));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TouchReport {
    pub contacts: [TouchSample; TOUCH_CONTACTS],
}

impl TouchReport {
    pub fn encode(self) -> Result<[u8; TOUCH_REPORT_LENGTH]> {
        let mut report = [0_u8; TOUCH_REPORT_LENGTH];
        for (index, contact) in self.contacts.into_iter().enumerate() {
            let contact = contact.validate()?;
            let offset = index * 6;
            report[offset] = index as u8;
            report[offset + 1] = u8::from(contact.down);
            report[offset + 2..offset + 4].copy_from_slice(&contact.x.to_le_bytes());
            report[offset + 4..offset + 6].copy_from_slice(&contact.y.to_le_bytes());
        }
        Ok(report)
    }

    pub fn decode(report: &[u8]) -> Result<Self> {
        if report.len() != TOUCH_REPORT_LENGTH {
            return Err(Error::MalformedWire(format!(
                "touch report length is {}, expected {TOUCH_REPORT_LENGTH}",
                report.len()
            )));
        }
        let mut contacts = [TouchSample::default(); TOUCH_CONTACTS];
        for (index, contact) in contacts.iter_mut().enumerate() {
            let offset = index * 6;
            if usize::from(report[offset]) != index {
                return Err(Error::MalformedWire(
                    "touch transducer index does not match its fixed slot".to_owned(),
                ));
            }
            if report[offset + 1] > 1 {
                return Err(Error::MalformedWire(
                    "touch flag contains unsupported bits".to_owned(),
                ));
            }
            *contact = TouchSample {
                down: report[offset + 1] == 1,
                x: u16::from_le_bytes([report[offset + 2], report[offset + 3]]),
                y: u16::from_le_bytes([report[offset + 4], report[offset + 5]]),
            }
            .validate()?;
        }
        Ok(Self { contacts })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnobButton {
    Select,
    Home,
    Back,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KnobReport {
    pub select: bool,
    pub home: bool,
    pub back: bool,
    pub x: i8,
    pub y: i8,
    pub wheel: i8,
}

impl KnobReport {
    pub fn encode(self) -> Result<[u8; 4]> {
        if [self.x, self.y, self.wheel].contains(&i8::MIN) {
            return Err(Error::InvalidInput(
                "knob axes have a -127..=127 logical range".to_owned(),
            ));
        }
        Ok([
            u8::from(self.select) | (u8::from(self.home) << 1) | (u8::from(self.back) << 2),
            self.x.to_ne_bytes()[0],
            self.y.to_ne_bytes()[0],
            self.wheel.to_ne_bytes()[0],
        ])
    }

    pub fn decode(report: &[u8]) -> Result<Self> {
        if report.len() != 4 {
            return Err(Error::MalformedWire(format!(
                "knob report length is {}, expected 4",
                report.len()
            )));
        }
        if report[0] & !0x07 != 0 || report[1..].contains(&0x80) {
            return Err(Error::MalformedWire(
                "knob report contains an undeclared bit or -128 axis".to_owned(),
            ));
        }
        Ok(Self {
            select: report[0] & 0x01 != 0,
            home: report[0] & 0x02 != 0,
            back: report[0] & 0x04 != 0,
            x: i8::from_ne_bytes([report[1]]),
            y: i8::from_ne_bytes([report[2]]),
            wheel: i8::from_ne_bytes([report[3]]),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum MediaButton {
    Play = 1,
    Pause = 2,
    PlayPause = 3,
    Next = 4,
    Previous = 5,
    NavigationGuidance = 6,
}

impl TryFrom<u8> for MediaButton {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Play),
            2 => Ok(Self::Pause),
            3 => Ok(Self::PlayPause),
            4 => Ok(Self::Next),
            5 => Ok(Self::Previous),
            6 => Ok(Self::NavigationGuidance),
            _ => Err(Error::MalformedWire(format!(
                "unsupported media HID array index {value}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum TelephonyButton {
    HookSwitch = 1,
    Flash = 2,
    Drop = 3,
    Mute = 4,
    Key0 = 5,
    Key1 = 6,
    Key2 = 7,
    Key3 = 8,
    Key4 = 9,
    Key5 = 10,
    Key6 = 11,
    Key7 = 12,
    Key8 = 13,
    Key9 = 14,
    Star = 15,
    Pound = 16,
    Delete = 17,
}

impl TryFrom<u8> for TelephonyButton {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::HookSwitch),
            2 => Ok(Self::Flash),
            3 => Ok(Self::Drop),
            4 => Ok(Self::Mute),
            5 => Ok(Self::Key0),
            6 => Ok(Self::Key1),
            7 => Ok(Self::Key2),
            8 => Ok(Self::Key3),
            9 => Ok(Self::Key4),
            10 => Ok(Self::Key5),
            11 => Ok(Self::Key6),
            12 => Ok(Self::Key7),
            13 => Ok(Self::Key8),
            14 => Ok(Self::Key9),
            15 => Ok(Self::Star),
            16 => Ok(Self::Pound),
            17 => Ok(Self::Delete),
            _ => Err(Error::MalformedWire(format!(
                "unsupported telephony HID array index {value}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn hex(bytes: &[u8]) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
            encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        encoded
    }

    #[test]
    fn descriptors_match_livi_goldens() {
        assert_eq!(
            hex(&touch_descriptor()),
            "050d0904a101050d0922a1020938750895018102150025010933750195018102950781030501262003093075109501810226e00109318102c0050d0922a1020938750895018102150025010933750195018102950781030501262003093075109501810226e00109318102c0c0"
        );
        assert_eq!(
            hex(knob_descriptor()),
            "05010908a1010509090115002501750195018102050c0a23020a2402950281029505810105010901a100093009311581257f750895028102c009381581257f750895018106c0"
        );
        assert_eq!(
            hex(media_descriptor()),
            "050c0901a10115002506050c0a00000ab0000ab1000acd000ab5000ab6000a9e02750895018100c0"
        );
        assert_eq!(
            hex(telephony_descriptor()),
            "050b0907a10115002511050b0900092009210926092f09b009b109b209b309b409b509b609b709b809b909ba09bb0507092a750895018100c0"
        );
    }

    #[test]
    fn device_declarations_are_exact() {
        let devices = carplay_hid_devices("main");
        assert_eq!(devices[0].uuid, "2a2a2a2a");
        assert_eq!(devices[0].name, "LIVI Touchscreen");
        assert_eq!(devices[3].uuid, "2a2a2a2d");
        assert!(devices.iter().all(|device| device.product_id == 1));
        assert!(devices.iter().all(|device| device.vendor_id == 2));
    }

    #[test]
    fn touch_report_matches_fixed_slot_golden() {
        let report = TouchReport {
            contacts: [
                TouchSample {
                    down: true,
                    x: 800,
                    y: 480,
                },
                TouchSample::default(),
            ],
        }
        .encode()
        .expect("report");
        assert_eq!(report, [0, 1, 0x20, 0x03, 0xe0, 0x01, 1, 0, 0, 0, 0, 0]);
        assert_eq!(
            TouchReport::decode(&report).expect("decode").contacts[0].x,
            800
        );
    }

    #[test]
    fn malformed_reports_are_rejected() {
        assert!(TouchReport::decode(&[0; 11]).is_err());
        let mut bad_index = [0; TOUCH_REPORT_LENGTH];
        bad_index[6] = 2;
        assert!(TouchReport::decode(&bad_index).is_err());
        assert!(KnobReport::decode(&[8, 0, 0, 0]).is_err());
        assert!(KnobReport::decode(&[0, 0x80, 0, 0]).is_err());
        assert!(MediaButton::try_from(7).is_err());
    }

    #[test]
    fn knob_and_array_reports_match_livi() {
        assert_eq!(
            KnobReport {
                select: true,
                home: true,
                back: true,
                x: 5,
                y: -5,
                wheel: 1,
            }
            .encode()
            .expect("knob"),
            [7, 5, 251, 1]
        );
        assert_eq!(MediaButton::PlayPause as u8, 3);
        assert_eq!(MediaButton::NavigationGuidance as u8, 6);
        assert_eq!(TelephonyButton::Delete as u8, 17);
    }

    proptest! {
        #[test]
        fn valid_touch_reports_round_trip(
            x0 in 0_u16..=CARPLAY_WIDTH_MAX,
            y0 in 0_u16..=CARPLAY_HEIGHT_MAX,
            x1 in 0_u16..=CARPLAY_WIDTH_MAX,
            y1 in 0_u16..=CARPLAY_HEIGHT_MAX,
            down0 in any::<bool>(),
            down1 in any::<bool>(),
        ) {
            let report = TouchReport { contacts: [
                TouchSample { down: down0, x: x0, y: y0 },
                TouchSample { down: down1, x: x1, y: y1 },
            ] };
            let encoded = report.encode().expect("encode");
            prop_assert_eq!(TouchReport::decode(&encoded).expect("decode"), report);
        }

        #[test]
        fn valid_knob_reports_round_trip(
            select in any::<bool>(),
            home in any::<bool>(),
            back in any::<bool>(),
            x in -127_i8..=127,
            y in -127_i8..=127,
            wheel in -127_i8..=127,
        ) {
            let report = KnobReport { select, home, back, x, y, wheel };
            let encoded = report.encode().expect("encode");
            prop_assert_eq!(KnobReport::decode(&encoded).expect("decode"), report);
        }
    }
}
