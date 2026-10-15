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
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("input sink failed: {0}")]
    Sink(String),
    #[error("invalid input state: {0}")]
    InvalidState(&'static str),
    #[error("input session has been torn down")]
    TornDown,
    #[error("event sequence is exhausted")]
    SequenceExhausted,
    #[error("monotonic timestamp moved backwards (previous {previous}, received {received})")]
    NonMonotonicTimestamp { previous: u64, received: u64 },
    #[error("malformed CarPlay wire value: {0}")]
    MalformedWire(String),
    #[error("CarPlay {section} is too large ({actual} bytes, maximum {maximum})")]
    MessageTooLarge {
        section: &'static str,
        actual: usize,
        maximum: usize,
    },
}
