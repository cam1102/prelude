use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Android Auto transport I/O failed")]
    Io(#[source] io::Error),
    #[error("invalid Android Auto frame: {0}")]
    InvalidFrame(&'static str),
    #[error("invalid Android Auto message: {0}")]
    InvalidMessage(&'static str),
    #[error(
        "invalid Android Auto message: version response is {actual} bytes, expected at least 6"
    )]
    VersionResponseTruncated { actual: usize },
    #[error(
        "Android Auto version 1.7 was not accepted (phone sent {major}.{minor}, status {status})"
    )]
    VersionRejected { major: u16, minor: u16, status: u16 },
    #[error("Android Auto {kind} is too large ({actual} > {limit})")]
    Oversized {
        kind: &'static str,
        actual: usize,
        limit: usize,
    },
    #[error("Android Auto session state does not permit {operation}")]
    InvalidState { operation: &'static str },
    #[error("Android Auto protobuf {context} is malformed")]
    Protobuf {
        context: &'static str,
        #[source]
        source: prost::DecodeError,
    },
    #[error("Android Auto TLS failed: {0}")]
    Tls(&'static str),
    #[error("Android Auto microphone flow-control backlog is full")]
    Backpressure,
    #[error("Android Auto session was cancelled")]
    Cancelled,
}

impl From<io::Error> for Error {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
