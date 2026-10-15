use std::io;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("{context}: {source}")]
    IoContext {
        context: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("malformed protobuf: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("frame body of {length} bytes exceeds the 16-bit length field")]
    FrameTooLarge { length: usize },
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
    #[error("invalid wireless configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("`{command}` failed with {status}")]
    CommandFailed { command: String, status: String },
    #[error("access point failed to start: {0}")]
    AccessPointStart(&'static str),
    #[error("{name} exited with {status}")]
    ProcessExited { name: &'static str, status: String },
    #[error("D-Bus call failed: {0}")]
    DBus(#[from] zbus::Error),
    #[error("D-Bus value conversion failed: {0}")]
    Variant(#[from] zbus::zvariant::Error),
}

impl Error {
    pub(crate) const fn io_context(context: &'static str, source: io::Error) -> Self {
        Self::IoContext { context, source }
    }
}
