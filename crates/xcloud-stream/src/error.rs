//! Crate-wide error type.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Api(#[from] xcloud_api::Error),
    #[error(transparent)]
    Transport(#[from] xcloud_webrtc::Error),
    #[error(transparent)]
    Media(#[from] xcloud_media::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
