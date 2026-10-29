//! Streaming runtime: ties the API layer, the WebRTC transport, the media pipeline and the
//! data channels into one session lifecycle. Ports `qt/src/streaming/SignalingOrchestrator`
//! and `NativeMediaPipeline`, plus the decode thread described in docs/archive/PORTING_PLAN.md section 4.

pub mod decode_thread;
pub mod error;
pub mod orchestrator;
pub mod pipeline;

pub use error::{Error, Result};
