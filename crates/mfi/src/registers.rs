pub(crate) const DEVICE_VERSION: u8 = 0x00;
pub(crate) const PROTOCOL_MAJOR: u8 = 0x02;
pub(crate) const PROTOCOL_MINOR: u8 = 0x03;
pub(crate) const ERROR_CODE: u8 = 0x05;
pub(crate) const AUTH_CONTROL_STATUS: u8 = 0x10;
pub(crate) const RESPONSE_LENGTH: u8 = 0x11;
pub(crate) const RESPONSE_DATA: u8 = 0x12;
pub(crate) const CHALLENGE_LENGTH: u8 = 0x20;
pub(crate) const CHALLENGE_DATA: u8 = 0x21;
pub(crate) const CERTIFICATE_LENGTH: u8 = 0x30;
pub(crate) const CERTIFICATE_DATA: u8 = 0x31;
pub(crate) const CERTIFICATE_SERIAL: u8 = 0x4e;

pub(crate) const START_AUTHENTICATION: u8 = 0x01;
pub(crate) const AUTHENTICATION_READY: u8 = 0x10;
