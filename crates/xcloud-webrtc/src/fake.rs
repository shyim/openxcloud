//! Port of the `RecordingBridge` test double in qt/tests/tst_streamcontroller.cpp
//! (a `NullWebRtcBridge` that records every `send`/`sendString` and serves a
//! scripted `collectStats` snapshot), widened into a general [`Transport`] fake.
//!
//! Not `cfg(test)`: the orchestrator, CLI and app tests all drive it.
//!
//! * Every outbound message is recorded per channel ([`FakeTransport::sent`]).
//! * The test pushes [`TransportEvent`]s with [`FakeTransport::push_event`]; they
//!   arrive on the receiver that [`Transport::events`] hands out, exactly like the
//!   events the WebRTC worker would emit.
//! * The installed [`MediaSink`] can be fed RTP and ticked from the test
//!   ([`FakeTransport::feed_video_rtp`], [`FakeTransport::tick_sink`]).
//! * `requires_candidates_before_remote_description`, `is_available`, the offer SDP,
//!   the local candidates, the connection state and send failures are scriptable.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use xcloud_proto::models::IceCandidatePayload;
use xcloud_proto::streaming_config::StreamingConfig;
use xcloud_proto::streaming_types::{
    DataChannelKind, DataChannelRuntimeStats, PeerConnectionState, SessionDescription,
    StreamingStatsSnapshot,
};

use crate::transport::{MediaSink, RtcpSender, Transport, TransportEvent};
use crate::{Error, Result};

/// The offer `create_offer` returns unless [`FakeTransport::set_offer_sdp`] replaces it.
/// A minimal but well-formed offer with the ICE ufrag the orchestrator tags
/// candidates with.
pub const DEFAULT_OFFER_SDP: &str = "v=0\r\n\
o=- 1 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0 1 2\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 102\r\n\
c=IN IP4 0.0.0.0\r\n\
a=ice-ufrag:fakeufrag\r\n\
a=ice-pwd:fakepwdfakepwdfakepwdfake\r\n\
a=mid:0\r\n\
a=recvonly\r\n\
a=rtpmap:102 H264/90000\r\n\
a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
c=IN IP4 0.0.0.0\r\n\
a=mid:1\r\n\
a=recvonly\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=fmtp:111 minptime=10;useinbandfec=1\r\n\
m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
c=IN IP4 0.0.0.0\r\n\
a=mid:2\r\n\
a=sctp-port:5000\r\n";

/// The payload of one recorded send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SentPayload {
    /// `Transport::send` (binary, the `input` channel).
    Binary(Vec<u8>),
    /// `Transport::send_text` (string PPID: control / message / chat JSON).
    Text(String),
}

/// One recorded outbound message. The C++ `RecordingBridge` stored
/// `QPair<DataChannelKind, QByteArray>` and folded strings into bytes; the fake
/// keeps the distinction so a test can assert the PPID too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub kind: DataChannelKind,
    pub payload: SentPayload,
}

impl Sent {
    /// The payload bytes, for both binary and text sends (`QByteArray` view).
    pub fn bytes(&self) -> &[u8] {
        match &self.payload {
            SentPayload::Binary(bytes) => bytes,
            SentPayload::Text(text) => text.as_bytes(),
        }
    }

    /// The payload as text; binary payloads are decoded lossily.
    pub fn text(&self) -> String {
        match &self.payload {
            SentPayload::Binary(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            SentPayload::Text(text) => text.clone(),
        }
    }

    pub fn is_text(&self) -> bool {
        matches!(self.payload, SentPayload::Text(_))
    }

    pub fn is_binary(&self) -> bool {
        matches!(self.payload, SentPayload::Binary(_))
    }
}

#[derive(Debug, Default)]
struct Script {
    sent: Vec<Sent>,
    snapshot: StreamingStatsSnapshot,
    connection_state: PeerConnectionState,
    offer_sdp: String,
    offer_error: Option<String>,
    local_candidates: Vec<IceCandidatePayload>,
    local_description: Option<SessionDescription>,
    remote_description: Option<SessionDescription>,
    remote_candidates: Vec<IceCandidatePayload>,
    failing_sends: HashSet<DataChannelKind>,
    reject_candidates: bool,
    /// Candidates added while no remote description was set yet.
    candidates_before_description: usize,
    close_count: usize,
    offers_created: usize,
    runtime_stats: Vec<(DataChannelKind, DataChannelRuntimeStats)>,
}

/// Scriptable, recording [`Transport`] double.
pub struct FakeTransport {
    script: Mutex<Script>,
    events_tx: Sender<TransportEvent>,
    events_rx: Receiver<TransportEvent>,
    requires_candidates_first: AtomicBool,
    available: AtomicBool,
    rtcp: Mutex<Option<Arc<dyn RtcpSender>>>,
    sink: Mutex<Option<Box<dyn MediaSink>>>,
}

impl std::fmt::Debug for FakeTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let script = self.script.lock();
        f.debug_struct("FakeTransport")
            .field("sent", &script.sent.len())
            .field("connection_state", &script.connection_state)
            .field("available", &self.available.load(Ordering::Relaxed))
            .field(
                "requires_candidates_first",
                &self.requires_candidates_first.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl Default for FakeTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeTransport {
    /// An available transport whose `create_offer` returns [`DEFAULT_OFFER_SDP`]
    /// and whose sends all succeed.
    pub fn new() -> Self {
        let (events_tx, events_rx) = unbounded();
        Self {
            script: Mutex::new(Script {
                offer_sdp: DEFAULT_OFFER_SDP.to_owned(),
                ..Script::default()
            }),
            events_tx,
            events_rx,
            requires_candidates_first: AtomicBool::new(false),
            available: AtomicBool::new(true),
            rtcp: Mutex::new(None),
            sink: Mutex::new(None),
        }
    }

    /// The `NullWebRtcBridge` shape: `is_available()` reports false. Everything
    /// else still records, so tests can observe what a headless build would send.
    pub fn unavailable() -> Self {
        let fake = Self::new();
        fake.set_available(false);
        fake
    }

    // ---- scripting ----------------------------------------------------------

    pub fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::SeqCst);
    }

    /// Mirrors `LibPeerWebRtcBridge::requiresCandidatesBeforeRemoteDescription`.
    pub fn set_requires_candidates_before_remote_description(&self, value: bool) {
        self.requires_candidates_first
            .store(value, Ordering::SeqCst);
    }

    /// Makes `rtcp_sender()` return `sender` (the "native media path" shape the
    /// orchestrator keys `optionsFromConfig(native = true)` on).
    pub fn set_rtcp_sender(&self, sender: Option<Arc<dyn RtcpSender>>) {
        *self.rtcp.lock() = sender;
    }

    /// The `collectStats` snapshot the test scripts (`RecordingBridge::snapshot`).
    pub fn set_stats(&self, snapshot: StreamingStatsSnapshot) {
        self.script.lock().snapshot = snapshot;
    }

    /// Sets the reported connection state and emits `ConnectionStateChanged`.
    pub fn set_connection_state(&self, state: PeerConnectionState) {
        self.script.lock().connection_state = state;
        self.push_event(TransportEvent::ConnectionStateChanged(state));
    }

    /// Sets the reported connection state without emitting an event.
    pub fn set_connection_state_silently(&self, state: PeerConnectionState) {
        self.script.lock().connection_state = state;
    }

    pub fn set_offer_sdp(&self, sdp: impl Into<String>) {
        self.script.lock().offer_sdp = sdp.into();
    }

    /// Makes `create_offer` fail with `Error::Other(message)`; `None` restores success.
    pub fn set_offer_error(&self, message: Option<&str>) {
        self.script.lock().offer_error = message.map(str::to_owned);
    }

    pub fn set_local_candidates(&self, candidates: Vec<IceCandidatePayload>) {
        self.script.lock().local_candidates = candidates;
    }

    /// Makes every `send`/`send_text` on `kind` fail with `Error::SendFailed`.
    pub fn set_send_failing(&self, kind: DataChannelKind, failing: bool) {
        let mut script = self.script.lock();
        if failing {
            script.failing_sends.insert(kind);
        } else {
            script.failing_sends.remove(&kind);
        }
    }

    /// Makes `add_remote_ice_candidate` fail with `Error::IceCandidateRejected`.
    pub fn set_reject_candidates(&self, reject: bool) {
        self.script.lock().reject_candidates = reject;
    }

    pub fn set_data_channel_runtime_stats(
        &self,
        kind: DataChannelKind,
        stats: DataChannelRuntimeStats,
    ) {
        let mut script = self.script.lock();
        script.runtime_stats.retain(|(k, _)| *k != kind);
        script.runtime_stats.push((kind, stats));
    }

    /// Delivers `event` on the receiver `events()` hands out, like the pump thread.
    pub fn push_event(&self, event: TransportEvent) {
        // The receiver side is kept alive by `self`, so this cannot fail.
        let _ = self.events_tx.send(event);
    }

    // ---- observation --------------------------------------------------------

    /// Every recorded send, in order (`RecordingBridge::sent`).
    pub fn sent(&self) -> Vec<Sent> {
        self.script.lock().sent.clone()
    }

    /// Recorded sends on one channel, in order.
    pub fn sent_on(&self, kind: DataChannelKind) -> Vec<Sent> {
        self.script
            .lock()
            .sent
            .iter()
            .filter(|entry| entry.kind == kind)
            .cloned()
            .collect()
    }

    pub fn sent_count(&self) -> usize {
        self.script.lock().sent.len()
    }

    pub fn clear_sent(&self) {
        self.script.lock().sent.clear();
    }

    pub fn local_description(&self) -> Option<SessionDescription> {
        self.script.lock().local_description.clone()
    }

    pub fn remote_description(&self) -> Option<SessionDescription> {
        self.script.lock().remote_description.clone()
    }

    /// Remote candidates added so far, in order.
    pub fn remote_candidates(&self) -> Vec<IceCandidatePayload> {
        self.script.lock().remote_candidates.clone()
    }

    /// How many remote candidates were added before `set_remote_description`
    /// (all of them for a candidates-first transport, none for a description-first one).
    pub fn candidates_added_before_remote_description(&self) -> usize {
        self.script.lock().candidates_before_description
    }

    pub fn close_count(&self) -> usize {
        self.script.lock().close_count
    }

    pub fn offers_created(&self) -> usize {
        self.script.lock().offers_created
    }

    pub fn has_media_sink(&self) -> bool {
        self.sink.lock().is_some()
    }

    // ---- media sink driving -------------------------------------------------

    /// `MediaSink::on_video_rtp` on the installed sink. Returns false without one.
    pub fn feed_video_rtp(&self, packet: &[u8], now_ms: u64) -> bool {
        match self.sink.lock().as_mut() {
            Some(sink) => {
                sink.on_video_rtp(packet, now_ms);
                true
            }
            None => false,
        }
    }

    /// `MediaSink::on_audio_rtp` on the installed sink. Returns false without one.
    pub fn feed_audio_rtp(&self, packet: &[u8], now_ms: u64) -> bool {
        match self.sink.lock().as_mut() {
            Some(sink) => {
                sink.on_audio_rtp(packet, now_ms);
                true
            }
            None => false,
        }
    }

    /// `MediaSink::tick` on the installed sink. Returns false without one.
    pub fn tick_sink(&self, now_ms: u64) -> bool {
        match self.sink.lock().as_mut() {
            Some(sink) => {
                sink.tick(now_ms);
                true
            }
            None => false,
        }
    }

    /// `MediaSink::stats` of the installed sink, if any.
    pub fn sink_stats(&self) -> Option<StreamingStatsSnapshot> {
        self.sink.lock().as_ref().map(|sink| sink.stats())
    }

    fn record(&self, kind: DataChannelKind, payload: SentPayload) -> Result<()> {
        let mut script = self.script.lock();
        script.sent.push(Sent { kind, payload });
        if script.failing_sends.contains(&kind) {
            return Err(Error::SendFailed(format!("{kind:?} scripted to fail")));
        }
        Ok(())
    }
}

impl Transport for FakeTransport {
    fn is_available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }

    fn requires_candidates_before_remote_description(&self) -> bool {
        self.requires_candidates_first.load(Ordering::SeqCst)
    }

    fn rtcp_sender(&self) -> Option<Arc<dyn RtcpSender>> {
        self.rtcp.lock().clone()
    }

    fn set_media_sink(&self, sink: Box<dyn MediaSink>) {
        *self.sink.lock() = Some(sink);
    }

    fn events(&self) -> Receiver<TransportEvent> {
        self.events_rx.clone()
    }

    fn create_offer(&self, _config: &StreamingConfig) -> Result<SessionDescription> {
        let mut script = self.script.lock();
        script.offers_created += 1;
        if let Some(message) = &script.offer_error {
            return Err(Error::Other(message.clone()));
        }
        Ok(SessionDescription {
            kind: "offer".to_owned(),
            sdp: script.offer_sdp.clone(),
        })
    }

    fn set_local_description(&self, description: &SessionDescription) -> Result<()> {
        self.script.lock().local_description = Some(description.clone());
        Ok(())
    }

    fn set_remote_description(&self, description: &SessionDescription) -> Result<()> {
        self.script.lock().remote_description = Some(description.clone());
        Ok(())
    }

    fn add_remote_ice_candidate(&self, candidate: &IceCandidatePayload) -> Result<()> {
        let mut script = self.script.lock();
        if script.reject_candidates {
            return Err(Error::IceCandidateRejected);
        }
        if script.remote_description.is_none() {
            script.candidates_before_description += 1;
        }
        script.remote_candidates.push(candidate.clone());
        Ok(())
    }

    fn local_ice_candidates(&self) -> Vec<IceCandidatePayload> {
        self.script.lock().local_candidates.clone()
    }

    fn connection_state(&self) -> PeerConnectionState {
        self.script.lock().connection_state
    }

    fn send(&self, kind: DataChannelKind, data: &[u8]) -> Result<()> {
        self.record(kind, SentPayload::Binary(data.to_vec()))
    }

    fn send_text(&self, kind: DataChannelKind, text: &str) -> Result<()> {
        self.record(kind, SentPayload::Text(text.to_owned()))
    }

    fn data_channel_runtime_stats(&self, kind: DataChannelKind) -> DataChannelRuntimeStats {
        self.script
            .lock()
            .runtime_stats
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, stats)| stats.clone())
            .unwrap_or_default()
    }

    fn close(&self) {
        {
            let mut script = self.script.lock();
            script.close_count += 1;
            script.connection_state = PeerConnectionState::Closed;
        }
        if let Some(sink) = self.sink.lock().as_mut() {
            sink.reset();
        }
    }

    fn collect_stats(&self) -> StreamingStatsSnapshot {
        self.script.lock().snapshot.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_sends_per_channel_and_payload_kind() {
        let fake = FakeTransport::new();
        fake.send(DataChannelKind::Input, &[1, 2, 3]).unwrap();
        fake.send_text(DataChannelKind::Control, "{\"a\":1}")
            .unwrap();
        let sent = fake.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].kind, DataChannelKind::Input);
        assert!(sent[0].is_binary());
        assert_eq!(sent[0].bytes(), &[1, 2, 3]);
        assert_eq!(sent[1].kind, DataChannelKind::Control);
        assert!(sent[1].is_text());
        assert_eq!(sent[1].text(), "{\"a\":1}");
        assert_eq!(fake.sent_on(DataChannelKind::Control).len(), 1);
        assert_eq!(fake.sent_on(DataChannelKind::Message).len(), 0);
    }

    #[test]
    fn pushed_events_arrive_on_the_events_receiver() {
        let fake = FakeTransport::new();
        let rx = fake.events();
        fake.push_event(TransportEvent::ChannelOpened(DataChannelKind::Message));
        fake.set_connection_state(PeerConnectionState::Connected);
        assert_eq!(
            rx.try_recv().unwrap(),
            TransportEvent::ChannelOpened(DataChannelKind::Message)
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            TransportEvent::ConnectionStateChanged(PeerConnectionState::Connected)
        );
        assert_eq!(fake.connection_state(), PeerConnectionState::Connected);
    }

    #[test]
    fn scripted_failures_and_flags() {
        let fake = FakeTransport::unavailable();
        assert!(!fake.is_available());
        assert!(!fake.requires_candidates_before_remote_description());
        fake.set_requires_candidates_before_remote_description(true);
        assert!(fake.requires_candidates_before_remote_description());

        fake.set_send_failing(DataChannelKind::Input, true);
        assert!(fake.send(DataChannelKind::Input, &[0]).is_err());
        assert!(fake.send_text(DataChannelKind::Control, "x").is_ok());
        // Failed sends are still recorded.
        assert_eq!(fake.sent_count(), 2);

        fake.set_offer_error(Some("boom"));
        assert!(fake.create_offer(&StreamingConfig::defaults()).is_err());
        fake.set_offer_error(None);
        let offer = fake.create_offer(&StreamingConfig::defaults()).unwrap();
        assert_eq!(offer.kind, "offer");
        assert!(offer.sdp.contains("a=ice-ufrag:fakeufrag"));
        assert_eq!(fake.offers_created(), 2);

        fake.set_reject_candidates(true);
        assert!(
            fake.add_remote_ice_candidate(&IceCandidatePayload::new("candidate:1"))
                .is_err()
        );
        assert!(fake.remote_candidates().is_empty());
    }

    struct CountingSink {
        video: usize,
        audio: usize,
        ticks: usize,
        resets: usize,
    }

    impl MediaSink for CountingSink {
        fn on_video_rtp(&mut self, _packet: &[u8], _now_ms: u64) {
            self.video += 1;
        }
        fn on_audio_rtp(&mut self, _packet: &[u8], _now_ms: u64) {
            self.audio += 1;
        }
        fn tick(&mut self, _now_ms: u64) {
            self.ticks += 1;
        }
        fn reset(&mut self) {
            self.resets += 1;
        }
        fn stats(&self) -> StreamingStatsSnapshot {
            StreamingStatsSnapshot {
                renderer_frames_received: self.video as i32,
                audio_packets_lost: self.audio as i32,
                frames_lost: self.ticks as i32,
                packets_lost: self.resets as i32,
                ..StreamingStatsSnapshot::default()
            }
        }
    }

    #[test]
    fn media_sink_is_driven_and_reset_on_close() {
        let fake = FakeTransport::new();
        assert!(!fake.feed_video_rtp(&[0], 0));
        assert!(!fake.has_media_sink());
        fake.set_media_sink(Box::new(CountingSink {
            video: 0,
            audio: 0,
            ticks: 0,
            resets: 0,
        }));
        assert!(fake.has_media_sink());
        assert!(fake.feed_video_rtp(&[0], 1));
        assert!(fake.feed_video_rtp(&[0], 2));
        assert!(fake.feed_audio_rtp(&[0], 3));
        assert!(fake.tick_sink(4));
        fake.close();
        let stats = fake.sink_stats().unwrap();
        assert_eq!(stats.renderer_frames_received, 2);
        assert_eq!(stats.audio_packets_lost, 1);
        assert_eq!(stats.frames_lost, 1);
        assert_eq!(stats.packets_lost, 1);
        assert_eq!(fake.close_count(), 1);
        assert_eq!(fake.connection_state(), PeerConnectionState::Closed);
    }

    #[test]
    fn stats_snapshot_is_scriptable() {
        let fake = FakeTransport::new();
        assert_eq!(fake.collect_stats(), StreamingStatsSnapshot::default());
        fake.set_stats(StreamingStatsSnapshot {
            negotiated_width: 2560,
            negotiated_height: 1440,
            ..StreamingStatsSnapshot::default()
        });
        assert_eq!(fake.collect_stats().negotiated_height, 1440);
        let runtime = DataChannelRuntimeStats {
            ready_state: "open".to_owned(),
            buffered_amount_bytes: 7,
        };
        fake.set_data_channel_runtime_stats(DataChannelKind::Input, runtime.clone());
        assert_eq!(
            fake.data_channel_runtime_stats(DataChannelKind::Input),
            runtime
        );
        assert_eq!(
            fake.data_channel_runtime_stats(DataChannelKind::Chat),
            DataChannelRuntimeStats::default()
        );
    }
}
