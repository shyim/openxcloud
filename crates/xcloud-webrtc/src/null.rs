//! Port of qt/src/streaming/NullWebRtcBridge.{h,cpp}: the headless transport used when
//! no WebRTC backend is available. Reports "unsupported" for every operation so the
//! app, controllers and tests build and run without a live WebRTC connection.

use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use xcloud_proto::models::IceCandidatePayload;
use xcloud_proto::streaming_config::StreamingConfig;
use xcloud_proto::streaming_types::{
    DataChannelKind, DataChannelRuntimeStats, PeerConnectionState, SessionDescription,
    StreamingStatsSnapshot,
};

use crate::transport::{MediaSink, RtcpSender, Transport, TransportEvent};
use crate::{Error, Result};

/// Port of `NullWebRtcBridge`. Every operation fails with [`Error::Unavailable`]
/// or is a no-op; the event channel never carries anything.
pub struct NullTransport {
    /// Kept so the receiver handed out by [`Transport::events`] stays connected (an
    /// idle receiver rather than a disconnected one, like a bridge that never signals).
    _events_tx: Sender<TransportEvent>,
    events_rx: Receiver<TransportEvent>,
}

impl Default for NullTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl NullTransport {
    pub fn new() -> Self {
        let (events_tx, events_rx) = unbounded();
        Self {
            _events_tx: events_tx,
            events_rx,
        }
    }
}

impl Transport for NullTransport {
    fn is_available(&self) -> bool {
        false
    }

    fn rtcp_sender(&self) -> Option<Arc<dyn RtcpSender>> {
        None
    }

    fn set_media_sink(&self, _sink: Box<dyn MediaSink>) {
        // No media ever arrives; the sink is dropped.
    }

    fn events(&self) -> Receiver<TransportEvent> {
        self.events_rx.clone()
    }

    fn create_offer(&self, _config: &StreamingConfig) -> Result<SessionDescription> {
        Err(Error::Unavailable)
    }

    fn set_local_description(&self, _description: &SessionDescription) -> Result<()> {
        Err(Error::Unavailable)
    }

    fn set_remote_description(&self, _description: &SessionDescription) -> Result<()> {
        Err(Error::Unavailable)
    }

    fn add_remote_ice_candidate(&self, _candidate: &IceCandidatePayload) -> Result<()> {
        Err(Error::Unavailable)
    }

    fn local_ice_candidates(&self) -> Vec<IceCandidatePayload> {
        Vec::new()
    }

    fn connection_state(&self) -> PeerConnectionState {
        PeerConnectionState::New
    }

    fn send(&self, _kind: DataChannelKind, _data: &[u8]) -> Result<()> {
        Err(Error::Unavailable)
    }

    fn send_text(&self, _kind: DataChannelKind, _text: &str) -> Result<()> {
        Err(Error::Unavailable)
    }

    fn data_channel_runtime_stats(&self, _kind: DataChannelKind) -> DataChannelRuntimeStats {
        DataChannelRuntimeStats::default()
    }

    fn close(&self) {}

    fn collect_stats(&self) -> StreamingStatsSnapshot {
        StreamingStatsSnapshot::default()
    }
}

#[cfg(test)]
mod tests {
    //! The Qt tree has no tst_nullwebrtcbridge; this checks the header's contract.

    use super::*;

    struct NoopSink;

    impl MediaSink for NoopSink {
        fn on_video_rtp(&mut self, _packet: &[u8], _now_ms: u64) {}
        fn on_audio_rtp(&mut self, _packet: &[u8], _now_ms: u64) {}
        fn tick(&mut self, _now_ms: u64) {}
        fn reset(&mut self) {}
        fn stats(&self) -> StreamingStatsSnapshot {
            StreamingStatsSnapshot::default()
        }
    }

    #[test]
    fn null_transport_reports_unavailable() {
        let transport = NullTransport::new();
        assert!(!transport.is_available());
        assert!(!transport.requires_candidates_before_remote_description());
        assert!(transport.rtcp_sender().is_none());
        transport.set_media_sink(Box::new(NoopSink));

        let description = SessionDescription::default();
        let candidate = IceCandidatePayload {
            candidate: String::new(),
            sdp_mline_index: 0,
            sdp_mid: "0".into(),
            username_fragment: String::new(),
        };
        assert!(matches!(
            transport.create_offer(&StreamingConfig::defaults()),
            Err(Error::Unavailable)
        ));
        assert!(matches!(
            transport.set_local_description(&description),
            Err(Error::Unavailable)
        ));
        assert!(matches!(
            transport.set_remote_description(&description),
            Err(Error::Unavailable)
        ));
        assert!(matches!(
            transport.add_remote_ice_candidate(&candidate),
            Err(Error::Unavailable)
        ));
        assert!(matches!(
            transport.send(DataChannelKind::Input, &[0]),
            Err(Error::Unavailable)
        ));
        assert!(matches!(
            transport.send_text(DataChannelKind::Control, "{}"),
            Err(Error::Unavailable)
        ));

        assert!(transport.local_ice_candidates().is_empty());
        assert_eq!(transport.connection_state(), PeerConnectionState::New);
        assert_eq!(
            transport.data_channel_runtime_stats(DataChannelKind::Chat),
            DataChannelRuntimeStats::default()
        );
        assert_eq!(transport.collect_stats(), StreamingStatsSnapshot::default());

        transport.close();
        let events = transport.events();
        assert!(events.try_recv().is_err());
        // The channel is idle, not disconnected: the sender lives with the transport.
        assert!(matches!(
            events.try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty)
        ));
    }
}
