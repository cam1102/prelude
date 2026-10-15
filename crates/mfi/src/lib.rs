#![forbid(unsafe_code)]

mod error;
mod registers;
mod transport;

pub use error::{Error, Result, TransportError};
pub use transport::{MockOperation, MockTransport, MockTransportState, Transport};

#[cfg(target_os = "linux")]
pub use transport::{LinuxI2c, LinuxI2cConfig};

use std::{fmt, time::Duration};

use registers as reg;

pub const REQUIRED_PROTOCOL_MAJOR: u8 = 3;
pub const CHALLENGE_LEN: usize = 32;
pub const RESPONSE_LEN: usize = 64;
pub const CERTIFICATE_SERIAL_ASCII_LEN: usize = 32;
pub const CERTIFICATE_SERIAL_LEN: usize = CERTIFICATE_SERIAL_ASCII_LEN / 2;
pub const CERTIFICATE_MIN_LEN: usize = 607;
pub const CERTIFICATE_MAX_LEN: usize = 609;

const CERTIFICATE_SETTLE: Duration = Duration::from_millis(10);
const AUTHENTICATION_SETTLE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceInfo {
    pub device_version: u8,
    pub protocol_major: u8,
    pub protocol_minor: u8,
}

pub trait MfiAccess {
    fn certificate(&mut self) -> Result<Vec<u8>>;
    fn certificate_serial(&mut self) -> Result<[u8; CERTIFICATE_SERIAL_LEN]>;
    fn sign(&mut self, challenge: &[u8; CHALLENGE_LEN]) -> Result<[u8; RESPONSE_LEN]>;
}

pub struct MfiAuth<T: Transport> {
    transport: T,
    info: DeviceInfo,
}

impl<T: Transport> fmt::Debug for MfiAuth<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MfiAuth")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl<T: Transport> MfiAuth<T> {
    pub fn new(mut transport: T) -> Result<Self> {
        let info = DeviceInfo {
            device_version: read_byte(&mut transport, reg::DEVICE_VERSION)?,
            protocol_major: read_byte(&mut transport, reg::PROTOCOL_MAJOR)?,
            protocol_minor: read_byte(&mut transport, reg::PROTOCOL_MINOR)?,
        };
        if info.protocol_major != REQUIRED_PROTOCOL_MAJOR {
            return Err(Error::UnsupportedProtocolMajor {
                expected: REQUIRED_PROTOCOL_MAJOR,
                actual: info.protocol_major,
            });
        }
        Ok(Self { transport, info })
    }

    pub fn device_info(&self) -> DeviceInfo {
        self.info
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    pub fn into_transport(self) -> T {
        self.transport
    }

    pub fn last_error(&mut self) -> Result<u8> {
        read_byte(&mut self.transport, reg::ERROR_CODE)
    }

    pub fn status(&mut self) -> Result<u8> {
        read_byte(&mut self.transport, reg::AUTH_CONTROL_STATUS)
    }

    pub fn certificate(&mut self) -> Result<Vec<u8>> {
        let length = self.certificate_len()?;
        self.transport.select(reg::CERTIFICATE_DATA)?;
        self.transport.sleep(CERTIFICATE_SETTLE);
        let mut certificate = vec![0; length];
        self.transport.read_exact(&mut certificate)?;
        Ok(certificate)
    }

    pub fn certificate_len(&mut self) -> Result<usize> {
        self.transport.select(reg::CERTIFICATE_LENGTH)?;
        self.transport.sleep(CERTIFICATE_SETTLE);
        let mut bytes = [0; 2];
        self.transport.read_exact(&mut bytes)?;
        let length = usize::from(u16::from_be_bytes(bytes));
        if !(CERTIFICATE_MIN_LEN..=CERTIFICATE_MAX_LEN).contains(&length) {
            return Err(Error::InvalidCertificateLength {
                min: CERTIFICATE_MIN_LEN,
                max: CERTIFICATE_MAX_LEN,
                actual: length,
            });
        }
        Ok(length)
    }

    pub fn certificate_serial_ascii(&mut self) -> Result<[u8; CERTIFICATE_SERIAL_ASCII_LEN]> {
        self.transport.select(reg::CERTIFICATE_SERIAL)?;
        let mut serial = [0; CERTIFICATE_SERIAL_ASCII_LEN];
        self.transport.read_exact(&mut serial)?;
        Ok(serial)
    }

    pub fn certificate_serial(&mut self) -> Result<[u8; CERTIFICATE_SERIAL_LEN]> {
        let ascii = self.certificate_serial_ascii()?;
        let mut serial = [0; CERTIFICATE_SERIAL_LEN];
        for (index, pair) in ascii.chunks_exact(2).enumerate() {
            let high = decode_hex(index * 2, pair[0])?;
            let low = decode_hex(index * 2 + 1, pair[1])?;
            serial[index] = (high << 4) | low;
        }
        Ok(serial)
    }

    pub fn sign(&mut self, challenge: &[u8; CHALLENGE_LEN]) -> Result<[u8; RESPONSE_LEN]> {
        self.transport.write(reg::CHALLENGE_DATA, challenge)?;

        let mut challenge_length = [0; 2];
        self.transport
            .read_register(reg::CHALLENGE_LENGTH, &mut challenge_length)?;
        let challenge_length = usize::from(u16::from_be_bytes(challenge_length));
        if challenge_length != CHALLENGE_LEN {
            return Err(Error::InvalidChallengeLength {
                expected: CHALLENGE_LEN,
                actual: challenge_length,
            });
        }

        self.transport
            .write(reg::AUTH_CONTROL_STATUS, &[reg::START_AUTHENTICATION])?;
        self.transport.sleep(AUTHENTICATION_SETTLE);

        let status = self.status()?;
        if status != reg::AUTHENTICATION_READY {
            return Err(Error::AuthenticationNotReady { status });
        }

        let mut response_length = [0; 2];
        self.transport
            .read_register(reg::RESPONSE_LENGTH, &mut response_length)?;
        let response_length = usize::from(u16::from_be_bytes(response_length));
        if response_length != RESPONSE_LEN {
            return Err(Error::InvalidResponseLength {
                expected: RESPONSE_LEN,
                actual: response_length,
            });
        }

        self.transport.select(reg::RESPONSE_DATA)?;
        let mut response = [0; RESPONSE_LEN];
        self.transport.read_exact(&mut response)?;
        Ok(response)
    }
}

impl<T: Transport> MfiAccess for MfiAuth<T> {
    fn certificate(&mut self) -> Result<Vec<u8>> {
        MfiAuth::<T>::certificate(self)
    }

    fn certificate_serial(&mut self) -> Result<[u8; CERTIFICATE_SERIAL_LEN]> {
        MfiAuth::<T>::certificate_serial(self)
    }

    fn sign(&mut self, challenge: &[u8; CHALLENGE_LEN]) -> Result<[u8; RESPONSE_LEN]> {
        MfiAuth::<T>::sign(self, challenge)
    }
}

#[cfg(target_os = "linux")]
impl MfiAuth<LinuxI2c> {
    pub fn open_default() -> Result<Self> {
        Self::open(&LinuxI2cConfig::default())
    }

    pub fn open(config: &LinuxI2cConfig) -> Result<Self> {
        Self::new(LinuxI2c::open(config)?)
    }
}

fn read_byte<T: Transport>(transport: &mut T, register: u8) -> Result<u8> {
    let mut value = [0];
    transport.read_register(register, &mut value)?;
    Ok(value[0])
}

fn decode_hex(index: usize, byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(Error::InvalidCertificateSerialHex { index, byte }),
    }
}
