use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    #[error("operation is unsupported: {0}")]
    Unsupported(&'static str),
    #[error("{component} is not ready (state: {state})")]
    NotReady {
        component: &'static str,
        state: &'static str,
    },
    #[error("invalid RTSP message: {0}")]
    InvalidRtsp(String),
    #[error("RTSP {section} is {actual} bytes; maximum is {maximum}")]
    MessageTooLarge {
        section: &'static str,
        actual: usize,
        maximum: usize,
    },
    #[error("invalid MFiSAP request: {0}")]
    InvalidMfiSap(String),
    #[error("MFi signer failed: {0}")]
    Signer(String),
    #[error("cryptographic operation failed: {0}")]
    Crypto(&'static str),
    #[error("invalid TLV8 message: {0}")]
    InvalidTlv8(String),
    #[error("pairing protocol failed: {0}")]
    Pairing(String),
    #[error("pairing transaction timed out")]
    PairingTimeout,
    #[error("persistent pairing storage failed: {0}")]
    Storage(String),
    #[error("encrypted control framing failed: {0}")]
    ControlCipher(&'static str),
}
