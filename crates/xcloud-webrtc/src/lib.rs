//! WebRTC transport abstraction and the portable webrtc-rs implementation.
//! Ports `qt/src/streaming/WebRtcBridge.h` (as the [`Transport`] trait) and
//! `LibPeerWebRtcBridge.{h,cpp}` (as [`native::WebRtcTransport`]).

pub mod error;
pub mod fake;
mod feedback;
pub mod native;
mod sink;
pub use sink::ConfigurableMediaSink;
pub mod null;
pub mod transport;

pub use error::{Error, Result};
pub use transport::{MediaSink, RtcpSender, Transport, TransportEvent, now_ms};
