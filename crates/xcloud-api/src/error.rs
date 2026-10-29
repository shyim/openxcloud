//! Crate-wide error type.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http: {0}")]
    Http(String),
    #[error("status {status}: {body}")]
    Status { status: u16, body: String },
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("auth: {0}")]
    Auth(String),
    #[error("token store: {0}")]
    TokenStore(String),
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
