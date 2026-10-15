use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("failed to read configuration {path}: {source}")]
    ConfigRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse configuration: {0}")]
    ConfigParse(#[from] toml::de::Error),
    #[error("failed to update daemon heartbeat {path}: {source}")]
    Heartbeat {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to update audio runtime status {path}: {detail}")]
    AudioStatus { path: PathBuf, detail: String },
    #[error("USB diagnostics failed: {0}")]
    Usb(#[from] prelude_usb_carkit::Error),
    #[error("operation is unsupported: {0}")]
    Unsupported(&'static str),
    #[error("{component} is not ready (state: {state})")]
    NotReady {
        component: &'static str,
        state: &'static str,
    },
    #[error("projection runtime failed: {0}")]
    Runtime(String),
    #[error("failed to serialize diagnostics: {0}")]
    Diagnostics(#[from] serde_json::Error),
}
