use std::{io, path::PathBuf, time::Duration};

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("USB operation failed: {0}")]
    Usb(#[from] rusb::Error),
    #[error("I/O failed while {operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("timed out after {timeout:?} while {operation}")]
    Timeout {
        operation: &'static str,
        timeout: Duration,
    },
    #[error("USB carrier disconnected")]
    Disconnected,
    #[error("operation was cancelled while {operation}")]
    Cancelled { operation: &'static str },
    #[error("USB device {bus:03}:{address:03} was not found")]
    DeviceNotFound { bus: u8, address: u8 },
    #[error("USB device identity changed while {stage}")]
    IdentityChanged { stage: &'static str },
    #[error("operation is unsupported: {0}")]
    Unsupported(&'static str),
    #[error("{component} is not ready (state: {state})")]
    NotReady {
        component: &'static str,
        state: &'static str,
    },
    #[error("USB configuration {0} is not advertised by the device")]
    ConfigurationMissing(u8),
    #[error("invalid USB descriptor: {0}")]
    InvalidDescriptor(String),
    #[error("invalid usbmux packet: {0}")]
    InvalidMux(String),
    #[error("invalid Lockdown message: {0}")]
    InvalidLockdown(String),
    #[error("pairing record is missing for device {device} in {directory}")]
    PairingRecordMissing { device: String, directory: PathBuf },
    #[error("invalid pairing record: {0}")]
    InvalidPairingRecord(String),
    #[error("iPhone trust confirmation was not received within {timeout:?}")]
    PairingDialogTimeout { timeout: Duration },
    #[error("the iPhone user denied the pairing request")]
    PairingDenied,
    #[error("Lockdown rejected {request}: {reason}")]
    LockdownRejected {
        request: &'static str,
        reason: String,
    },
    #[error("TLS setup failed: {0}")]
    Tls(String),
    #[error("invalid NCM NTB16 block: {0}")]
    InvalidNtb(String),
    #[error("wired NCM setup failed: {0}")]
    Ncm(String),
    #[error("NCM frame is {actual} bytes; maximum is {maximum}")]
    FrameTooLarge { actual: usize, maximum: usize },
}

impl Error {
    pub(crate) fn io(operation: &'static str, source: io::Error) -> Self {
        Self::Io { operation, source }
    }
}
