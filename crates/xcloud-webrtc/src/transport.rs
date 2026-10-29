//! Port of qt/src/streaming/WebRtcBridge.h: the transport abstraction over a WebRTC peer
//! connection, expressed as traits instead of a QObject with signals.
//!
//! Threading model (docs/archive/PORTING_PLAN.md section 4):
//! * Signaling methods synchronously wait for bounded operations on a dedicated
//!   WebRTC runtime. Input sends and feedback only enqueue work and do not wait
//!   for network I/O. The orchestrator calls signaling from its tokio task.
//! * Everything the C++ emitted as a Qt signal arrives as a [`TransportEvent`] on the
//!   receiver returned by [`Transport::events`].
//! * Raw RTP never goes through a channel: the transport calls the installed
//!   [`MediaSink`] directly on its WebRTC worker. The sink must never block.
//! * RTCP feedback flows back through [`RtcpSender`], which the sink receives at
//!   construction time from [`Transport::rtcp_sender`].

use std::sync::{Arc, OnceLock};
use std::time::Instant;

use crossbeam_channel::Receiver;
use xcloud_proto::models::IceCandidatePayload;
use xcloud_proto::streaming_config::StreamingConfig;
use xcloud_proto::streaming_types::{
    DataChannelKind, DataChannelRuntimeStats, PeerConnectionState, SessionDescription,
    StreamingStatsSnapshot,
};

use crate::Result;

/// Monotonic milliseconds since the first call, mirroring the C++ `uptime()` QElapsedTimer.
pub fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Everything the C++ `WebRtcBridge` emitted as a signal.
#[derive(Debug, Clone, PartialEq)]
pub enum TransportEvent {
    ConnectionStateChanged(PeerConnectionState),
    CandidateGathered(IceCandidatePayload),
    ChannelOpened(DataChannelKind),
    /// Binary data-channel message (the `input` channel).
    DataReceived {
        kind: DataChannelKind,
        data: Vec<u8>,
    },
    /// Text data-channel message (control / message / chat).
    TextReceived {
        kind: DataChannelKind,
        text: String,
    },
    VideoTrackReceived,
    AudioTrackReceived,
    Failed(String),
}

/// Callback surface the media pipeline uses to send RTCP feedback to the sender.
/// Port of `RtcpSender`. Implemented by the native transport; callable from any
/// thread, including a media callback. Implementations must not wait for network I/O.
pub trait RtcpSender: Send + Sync {
    fn nack(&self, pid: u16, blp: u16);
    fn keyframe_request(&self);
    fn remb(&self, bitrate_bps: u32);
    fn receiver_report(
        &self,
        fraction_lost: u8,
        cumulative_lost: u32,
        highest_seq_ext: u32,
        jitter: u32,
    );
}

/// Receives decrypted RTP on the transport's WebRTC worker. Port of the half of
/// `NativeMediaPipeline` that the bridge drives (`pushVideoRtp`, `pushAudioRtp`, `tick`, `reset`).
///
/// Contract: every method returns quickly and never blocks on I/O, locks held by other
/// threads, or the decoder. `tick` is called once per worker tick (roughly every ms).
pub trait MediaSink: Send {
    fn on_video_rtp(&mut self, packet: &[u8], now_ms: u64);
    fn on_audio_rtp(&mut self, packet: &[u8], now_ms: u64);
    fn tick(&mut self, now_ms: u64);
    /// Called when the peer connection is torn down (new session or close).
    fn reset(&mut self);
    /// Latest pipeline statistics, merged into [`Transport::collect_stats`].
    fn stats(&self) -> StreamingStatsSnapshot;
}

/// Port of `WebRtcBridge`. Object-safe so the orchestrator can hold `Arc<dyn Transport>`.
pub trait Transport: Send + Sync {
    /// Whether this transport can actually open a peer connection in this build.
    fn is_available(&self) -> bool;

    /// True when the transport builds ICE candidate pairs only inside
    /// `set_remote_description`, so all remote candidates must be added first.
    fn requires_candidates_before_remote_description(&self) -> bool {
        false
    }

    /// The RTCP feedback sink, or `None` when the transport handles media itself.
    fn rtcp_sender(&self) -> Option<Arc<dyn RtcpSender>>;

    /// Installs the sink that receives raw RTP on the WebRTC worker. Replaces any previous
    /// sink. The transport calls `reset` on it whenever the peer connection is torn down.
    fn set_media_sink(&self, sink: Box<dyn MediaSink>);

    /// Event stream; one receiver per call to `events()` is not required, callers clone
    /// the receiver they get at construction.
    fn events(&self) -> Receiver<TransportEvent>;

    fn create_offer(&self, config: &StreamingConfig) -> Result<SessionDescription>;
    fn set_local_description(&self, description: &SessionDescription) -> Result<()>;
    fn set_remote_description(&self, description: &SessionDescription) -> Result<()>;
    fn add_remote_ice_candidate(&self, candidate: &IceCandidatePayload) -> Result<()>;

    fn local_ice_candidates(&self) -> Vec<IceCandidatePayload>;
    fn connection_state(&self) -> PeerConnectionState;

    /// Binary send (the `input` channel).
    fn send(&self, kind: DataChannelKind, data: &[u8]) -> Result<()>;
    /// Text send with the WebRTC string PPID (control / message / chat JSON).
    fn send_text(&self, kind: DataChannelKind, text: &str) -> Result<()>;

    fn data_channel_runtime_stats(&self, kind: DataChannelKind) -> DataChannelRuntimeStats;

    /// Tears the peer connection down. Idempotent.
    fn close(&self);
    fn collect_stats(&self) -> StreamingStatsSnapshot;
}
