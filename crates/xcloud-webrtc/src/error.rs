//! Crate-wide error type.

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("peer connection is not created")]
    NoPeer,
    #[error("data channel not open: {0}")]
    ChannelNotOpen(String),
    #[error("data channel send failed: {0}")]
    SendFailed(String),
    #[error("WebRTC rejected the ICE candidate")]
    IceCandidateRejected,
    #[error("WebRTC failed to create an offer")]
    OfferFailed,
    #[error("transport unavailable in this build")]
    Unavailable,
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
