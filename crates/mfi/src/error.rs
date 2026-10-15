use std::{error, fmt, io};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum TransportError {
    Io(io::Error),
    Protocol(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I2C I/O error: {error}"),
            Self::Protocol(message) => formatter.write_str(message),
        }
    }
}

impl error::Error for TransportError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Protocol(_) => None,
        }
    }
}

impl From<io::Error> for TransportError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug)]
pub enum Error {
    Transport(TransportError),
    UnsupportedProtocolMajor {
        expected: u8,
        actual: u8,
    },
    InvalidCertificateLength {
        min: usize,
        max: usize,
        actual: usize,
    },
    InvalidCertificateSerialHex {
        index: usize,
        byte: u8,
    },
    InvalidChallengeLength {
        expected: usize,
        actual: usize,
    },
    AuthenticationNotReady {
        status: u8,
    },
    InvalidResponseLength {
        expected: usize,
        actual: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(formatter, "{error}"),
            Self::UnsupportedProtocolMajor { expected, actual } => write!(
                formatter,
                "unsupported MFi authentication protocol major {actual}; expected {expected}"
            ),
            Self::InvalidCertificateLength { min, max, actual } => write!(
                formatter,
                "invalid MFi certificate length {actual}; expected {min}..={max} bytes"
            ),
            Self::InvalidCertificateSerialHex { index, byte } => write!(
                formatter,
                "invalid ASCII-hex byte 0x{byte:02x} at certificate serial offset {index}"
            ),
            Self::InvalidChallengeLength { expected, actual } => write!(
                formatter,
                "coprocessor retained {actual} challenge bytes; expected {expected}"
            ),
            Self::AuthenticationNotReady { status } => write!(
                formatter,
                "coprocessor authentication did not complete successfully (status 0x{status:02x})"
            ),
            Self::InvalidResponseLength { expected, actual } => write!(
                formatter,
                "coprocessor produced {actual} response bytes; expected {expected}"
            ),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            _ => None,
        }
    }
}

impl From<TransportError> for Error {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}
