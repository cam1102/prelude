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
    #[error("invalid video data: {0}")]
    InvalidVideo(String),
    #[error("invalid audio data: {0}")]
    InvalidAudio(String),
    #[error("invalid timing data: {0}")]
    InvalidTiming(String),
    #[error("media cryptographic operation failed: {0}")]
    Crypto(&'static str),
    #[error("media backend failed: {0}")]
    Backend(String),
}
