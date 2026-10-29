//! Media layer of the xCloud client: frame types, the [`VideoDecoder`] trait, Annex-B
//! bitstream helpers and the VideoToolbox H.264 backend.
//!
//! Ports `qt/src/streaming/H264Decoder.{h,cpp}` (decoder), `NativeVideoFrame.h` (frame),
//! `VideoFrameMailbox.{h,cpp}` (latest-frame hand-off) and the colour-space handling of
//! `qt/src/app/MetalVideoItem.mm`. Opus decoding (`OpusDecoder.{h,cpp}`) lives in [`opus`] and PCM
//! playback (`qt/src/app/AudioOutput.{h,cpp}`) in [`audio_out`].
//!
//! [`fallback`] switches from one backend to another at runtime (VideoToolbox → FFmpeg).
//!
//! Platform code is confined to [`vt`] (macOS). Everything else compiles everywhere.

pub mod annexb;
pub mod audio_out;
pub mod decoder;
pub mod error;
pub mod fallback;
pub mod frame;
pub mod opus;

#[cfg(target_os = "macos")]
pub mod vt;

#[cfg(feature = "ffmpeg")]
pub mod ffmpeg;

pub use audio_out::{
    AudioBackend, AudioCounters, AudioOutput, CpalBackend, OutputStream, PcmFormat, PcmSource,
};
pub use decoder::{FrameMailbox, FrameSink, MailboxReader, MailboxWriter, VideoDecoder};
pub use error::{Error, Result};
pub use fallback::{
    Backend, FallbackConfig, FallbackDecoder, FallbackStatus, SESSION_CREATION_CALLS, SwitchReason,
};
pub use frame::{ColorInfo, Frame, NativeBuffer, YCbCrMatrix};
pub use opus::OpusDecoder;
