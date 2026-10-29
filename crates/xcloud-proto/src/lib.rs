//! Pure protocol logic for the xCloud client: models, preferences, input protocol,
//! SDP/ICE processing, RTP jitter buffers and data-channel message codecs.
//! No I/O, no async, no platform code. See CLAUDE.md for the C++ → Rust mapping.

pub mod auth_types;
pub mod error;
pub mod library_filter;
pub mod locales;
pub mod models;
pub mod preferences;
pub mod streaming_config;
pub mod streaming_types;

pub mod audio_jitter;
pub mod channels;
pub mod input;
pub mod rtp;
pub mod sdp;
pub mod video_jitter;

pub use error::{Error, Result};
