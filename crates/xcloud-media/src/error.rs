//! Error type for `xcloud-media`. Replaces `H264Decoder::lastError()` (a `QString`) with
//! variants the caller can act on: `BadData` and `WaitingForParameterSets` mean "request a
//! keyframe", `InvalidSession` means the session was torn down and rebuilt.

/// Everything that can go wrong in the media crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No SPS/PPS have been seen yet, so there is no format description to decode
    /// against. The caller should request a keyframe (PLI).
    #[error("waiting for SPS/PPS before the first decodable access unit")]
    WaitingForParameterSets,

    /// The SPS could not be parsed.
    #[error("invalid SPS: {0}")]
    InvalidSps(String),

    /// The decoder rejected the access unit (`kVTVideoDecoderBadDataErr`). The stream
    /// is corrupt or a reference is missing; the caller should request a keyframe.
    #[error("decoder rejected the access unit (bad data); request a keyframe")]
    BadData,

    /// The decoder session was invalidated by the system (`kVTInvalidSessionErr`, e.g.
    /// GPU reset or app switch). The backend recreates the session; the access unit that
    /// hit the error is lost, so the caller should request a keyframe.
    #[error("decoder session was invalidated and recreated; request a keyframe")]
    InvalidSession,

    /// No suitable decoder exists on this machine (e.g. hardware decoding required but
    /// unavailable, or an unsupported profile/level).
    #[error("no video decoder available: {0}")]
    Unavailable(String),

    /// A platform call failed with an `OSStatus` we do not map more precisely.
    #[error("{what} failed with OSStatus {status}")]
    Os { what: &'static str, status: i32 },

    /// A code path that is not implemented for this platform or buffer kind.
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;
