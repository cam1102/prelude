use std::time::Duration;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("USB operation failed while {operation}: {source}")]
    Usb {
        operation: &'static str,
        #[source]
        source: rusb::Error,
    },
    #[error("operation was cancelled while {operation}")]
    Cancelled { operation: &'static str },
    #[error("timed out after {timeout:?} while {operation}")]
    Timeout {
        operation: &'static str,
        timeout: Duration,
    },
    #[error("Android accessory disconnected while {operation}")]
    Disconnected { operation: &'static str },
    #[error("AOAP protocol response was malformed: expected 2 bytes, received {actual}")]
    MalformedProtocolResponse { actual: usize },
    #[error("AOAP protocol version {0} is unsupported")]
    UnsupportedProtocol(u16),
    #[error("AOAP identity field {field} is invalid")]
    InvalidIdentity { field: &'static str },
    #[error("USB device has no stable physical port path")]
    MissingPortPath,
    #[error("selected USB device is not an Android Open Accessory candidate")]
    NotAoapCandidate,
    #[error("Android accessory did not re-enumerate at {topology} within {timeout:?}")]
    ReenumerationTimeout {
        topology: crate::UsbTopology,
        timeout: Duration,
    },
    #[error("Android accessory exposes no interface with bulk IN and OUT endpoints")]
    BulkEndpointsMissing,
    #[error("USB transfer made no progress while {operation}")]
    NoProgress { operation: &'static str },
}

impl Error {
    pub(crate) fn usb(operation: &'static str, source: rusb::Error) -> Self {
        if source == rusb::Error::NoDevice {
            Self::Disconnected { operation }
        } else {
            Self::Usb { operation, source }
        }
    }
}
