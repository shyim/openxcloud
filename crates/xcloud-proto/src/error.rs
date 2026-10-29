//! Crate-wide error type.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("malformed packet: {0}")]
    MalformedPacket(&'static str),
    #[error("invalid value: {0}")]
    Invalid(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
