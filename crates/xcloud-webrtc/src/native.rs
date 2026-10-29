//! Portable replacement for qt/src/streaming/LibPeerWebRtcBridge.{h,cpp}.
//!
//! A dedicated Tokio thread drives webrtc-rs independently of the caller's runtime.
//! Signaling calls wait for bounded operations on that thread; input and RTCP use
//! bounded queues and never wait for network I/O. Each offer owns its callbacks and
//! cancellation token, so a closed session cannot deliver media into a new one.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::feedback::{ReportObserver, SenderReports, prepare_feedback};
use crate::sink::PlainSink;
use crate::{
    ConfigurableMediaSink, Error, MediaSink, Result, RtcpSender, Transport, TransportEvent, now_ms,
};
use bytes::BytesMut;
use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use rtc::rtcp::{
    self,
    payload_feedbacks::{
        picture_loss_indication::PictureLossIndication,
        receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate,
    },
    receiver_report::ReceiverReport,
    reception_report::ReceptionReport,
    transport_feedbacks::transport_layer_nack::{NackPair, TransportLayerNack},
};
use rtc::rtp_transceiver::rtp_sender::{
    RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RtpCodecKind,
};
use rtc::shared::marshal::Marshal;
use tokio::sync::{mpsc, watch};
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelInit};
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
    RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceGatheringState, RTCIceServer,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState, RTCSessionDescription,
};
use webrtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use xcloud_proto::models::IceCandidatePayload;
use xcloud_proto::streaming_config::StreamingConfig;
use xcloud_proto::streaming_types::{
    DataChannelKind, DataChannelRuntimeStats, PeerConnectionState, SessionDescription,
    StreamingStatsSnapshot, data_channel_kind_name, data_channel_specs,
};

const OPERATION_TIMEOUT: Duration = Duration::from_secs(15);
const SEND_CAPACITY: usize = 64;
const LOG: &str = "WebRTC";
type Sink = Arc<Mutex<Option<Box<dyn ConfigurableMediaSink>>>>;
type Feedback = Box<dyn rtcp::Packet>;

struct Worker {
    handle: tokio::runtime::Handle,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}
impl Worker {
    fn new() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(other)?;
        let handle = runtime.handle().clone();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("webrtc".into())
            .spawn(move || {
                runtime.block_on(async {
                    let _ = stopped.await;
                });
                runtime.shutdown_timeout(Duration::from_secs(2));
            })
            .map_err(other)?;
        Ok(Self {
            handle,
            stop: Some(stop),
            thread: Some(thread),
        })
    }
    fn call<T: Send + 'static>(
        &self,
        future: impl Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        // Callbacks must use the nonblocking feedback/send paths, never signal.
        if self
            .thread
            .as_ref()
            .is_some_and(|t| t.thread().id() == std::thread::current().id())
        {
            return Err(Error::Other(
                "synchronous signaling called from WebRTC worker".into(),
            ));
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.handle.spawn(async move {
            let result = tokio::time::timeout(OPERATION_TIMEOUT, future)
                .await
                .unwrap_or_else(|_| Err(Error::Other("WebRTC operation timed out".into())));
            let _ = tx.send(result);
        });
        rx.recv_timeout(OPERATION_TIMEOUT + Duration::from_secs(1))
            .map_err(other)?
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take()
            && thread.thread().id() != std::thread::current().id()
        {
            let _ = thread.join();
        }
    }
}
fn other(error: impl std::fmt::Display) -> Error {
    Error::Other(error.to_string())
}

struct Session {
    peer: Arc<dyn PeerConnection>,
    shared: Arc<SessionState>,
    outgoing: mpsc::Sender<Outgoing>,
    feedback: mpsc::Sender<Feedback>,
}
struct Outgoing {
    kind: DataChannelKind,
    data: Vec<u8>,
    text: bool,
}
struct SessionState {
    active: AtomicBool,
    cancel: watch::Sender<bool>,
    gathered: watch::Sender<bool>,
    state: Mutex<PeerConnectionState>,
    candidates: Mutex<Vec<IceCandidatePayload>>,
    channels: Mutex<HashMap<DataChannelKind, Arc<dyn DataChannel>>>,
    open: Mutex<HashMap<DataChannelKind, bool>>,
    video: Mutex<Option<Arc<dyn TrackRemote>>>,
    video_ssrc: AtomicU32,
    reports: SenderReports,
    sink: Sink,
    events: Sender<TransportEvent>,
}
impl SessionState {
    fn emit(&self, event: TransportEvent) {
        if self.active.load(Ordering::Acquire) {
            let _ = self.events.send(event);
        }
    }
}
struct Handler(Arc<SessionState>);
#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let state = match state {
            RTCPeerConnectionState::Connecting => PeerConnectionState::Connecting,
            RTCPeerConnectionState::Connected => PeerConnectionState::Connected,
            RTCPeerConnectionState::Disconnected => PeerConnectionState::Disconnected,
            RTCPeerConnectionState::Failed => PeerConnectionState::Failed,
            RTCPeerConnectionState::Closed => PeerConnectionState::Closed,
            _ => PeerConnectionState::New,
        };
        *self.0.state.lock() = state;
        self.0.emit(TransportEvent::ConnectionStateChanged(state));
    }
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(candidate) = event.candidate.to_json() {
            let candidate = IceCandidatePayload {
                candidate: candidate.candidate,
                sdp_mid: candidate.sdp_mid.unwrap_or_else(|| "0".into()),
                sdp_mline_index: i32::from(candidate.sdp_mline_index.unwrap_or(0)),
                username_fragment: candidate.username_fragment.unwrap_or_default(),
            };
            self.0.candidates.lock().push(candidate.clone());
            self.0.emit(TransportEvent::CandidateGathered(candidate));
        }
    }
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.0.gathered.send_replace(true);
        }
    }
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let video = track.kind().await == RtpCodecKind::Video;
        if video {
            *self.0.video.lock() = Some(track.clone());
        }
        self.0.emit(if video {
            TransportEvent::VideoTrackReceived
        } else {
            TransportEvent::AudioTrackReceived
        });
        let shared = self.0.clone();
        let mut cancel = shared.cancel.subscribe();
        tokio::spawn(async move {
            loop {
                if *cancel.borrow() {
                    break;
                }
                tokio::select! {
                    _ = cancel.changed() => break,
                    event = track.poll() => match event {
                        Some(TrackRemoteEvent::OnRtpPacket(packet)) => {
                            if video { shared.video_ssrc.store(packet.header.ssrc, Ordering::Relaxed); }
                            if let Ok(bytes) = packet.marshal() {
                                let mut sink = shared.sink.lock();
                                if shared.active.load(Ordering::Acquire)
                                    && let Some(sink) = sink.as_mut() {
                                    if video { sink.on_video_rtp(&bytes, now_ms()); }
                                    else { sink.on_audio_rtp(&bytes, now_ms()); }
                                }
                            }
                        }
                        None | Some(TrackRemoteEvent::OnEnded) => break,
                        _ => {}
                    }
                }
            }
        });
    }
}

async fn poll_channel(
    shared: Arc<SessionState>,
    kind: DataChannelKind,
    channel: Arc<dyn DataChannel>,
) {
    let mut cancel = shared.cancel.subscribe();
    loop {
        if *cancel.borrow() {
            break;
        }
        tokio::select! {
            _ = cancel.changed() => break,
            event = channel.poll() => match event {
                Some(DataChannelEvent::OnOpen) => {
                    shared.open.lock().insert(kind, true);
                    shared.emit(TransportEvent::ChannelOpened(kind));
                }
                Some(DataChannelEvent::OnMessage(message)) => {
                    if message.is_string {
                        match String::from_utf8(message.data.to_vec()) {
                            Ok(text) => shared.emit(TransportEvent::TextReceived { kind, text }),
                            Err(error) => tracing::warn!(target: LOG, %error, "invalid data-channel text"),
                        }
                    } else { shared.emit(TransportEvent::DataReceived { kind, data: message.data.to_vec() }); }
                }
                None | Some(DataChannelEvent::OnClose) => break,
                Some(DataChannelEvent::OnError) => shared.emit(TransportEvent::Failed(format!("{} channel error", data_channel_kind_name(kind)))),
                _ => {}
            }
        }
    }
    shared.open.lock().insert(kind, false);
}

fn media_engine(config: &StreamingConfig) -> Result<MediaEngine> {
    let profile = if config.h264_profile_level_id.is_empty() {
        "42e01f"
    } else {
        &config.h264_profile_level_id
    };
    let (max_fs, max_mbps) = if config.scale_video_caps_to_1080 {
        (8160, 489600)
    } else {
        (3600, 108000)
    };
    let mut media = MediaEngine::default();
    media.register_codec(RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: "video/H264".into(), clock_rate: 90_000,
            sdp_fmtp_line: format!("level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile};max-fs={max_fs};max-mbps={max_mbps}"),
            rtcp_feedback: [("nack", ""), ("nack", "pli"), ("goog-remb", "")].into_iter()
                .map(|(typ, parameter)| RTCPFeedback { typ: typ.into(), parameter: parameter.into() }).collect(),
            ..Default::default()
        }, payload_type: 102,
    }, RtpCodecKind::Video).map_err(other)?;
    media
        .register_codec(
            RTCRtpCodecParameters {
                rtp_codec: RTCRtpCodec {
                    mime_type: "audio/opus".into(),
                    clock_rate: 48_000,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;useinbandfec=1;stereo=1".into(),
                    ..Default::default()
                },
                payload_type: 111,
            },
            RtpCodecKind::Audio,
        )
        .map_err(other)?;
    Ok(media)
}

/// The desktop and CLI WebRTC transport. No process-global native initialization.
pub struct WebRtcTransport {
    worker: Worker,
    session: Arc<Mutex<Option<Arc<Session>>>>,
    signaling: Mutex<()>,
    sink: Sink,
    config: Mutex<StreamingConfig>,
    events_tx: Sender<TransportEvent>,
    events_rx: Receiver<TransportEvent>,
    ice_servers: Vec<RTCIceServer>,
}
impl Default for WebRtcTransport {
    fn default() -> Self {
        Self::new()
    }
}
impl WebRtcTransport {
    pub fn new() -> Self {
        Self::try_new().unwrap_or_else(|error| panic!("cannot start WebRTC worker: {error}"))
    }
    pub fn try_new() -> Result<Self> {
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        Ok(Self {
            worker: Worker::new()?,
            session: Arc::new(Mutex::new(None)),
            signaling: Mutex::new(()),
            sink: Arc::new(Mutex::new(None)),
            config: Mutex::new(StreamingConfig::defaults()),
            events_tx,
            events_rx,
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".into()],
                ..Default::default()
            }],
        })
    }
    pub fn set_configurable_media_sink(&self, mut sink: Box<dyn ConfigurableMediaSink>) {
        sink.configure(&self.config.lock());
        let mut current = self.sink.lock();
        if let Some(old) = current.as_mut() {
            old.reset();
        }
        *current = Some(sink);
    }
    pub fn request_keyframe(&self) {
        FeedbackSender(Arc::downgrade(&self.session)).keyframe_request();
    }
    fn current(&self) -> Result<Arc<Session>> {
        self.session.lock().clone().ok_or(Error::NoPeer)
    }
    fn close_session(&self) {
        let session = self.session.lock().take();
        if let Some(session) = session {
            session.shared.active.store(false, Ordering::Release);
            session.shared.cancel.send_replace(true);
            if let Err(error) = self
                .worker
                .call(async move { session.peer.close().await.map_err(other) })
            {
                tracing::warn!(target: LOG, %error, "closing peer failed");
            }
            if let Some(sink) = self.sink.lock().as_mut() {
                sink.reset();
            }
            // Discard queued events from the previous session before announcing closure.
            while self.events_rx.try_recv().is_ok() {}
            let _ = self.events_tx.send(TransportEvent::ConnectionStateChanged(
                PeerConnectionState::Closed,
            ));
        }
    }
}
impl Drop for WebRtcTransport {
    fn drop(&mut self) {
        self.close_session();
    }
}

impl Transport for WebRtcTransport {
    fn is_available(&self) -> bool {
        true
    }
    fn rtcp_sender(&self) -> Option<Arc<dyn RtcpSender>> {
        Some(Arc::new(FeedbackSender(Arc::downgrade(&self.session))))
    }
    fn set_media_sink(&self, sink: Box<dyn MediaSink>) {
        self.set_configurable_media_sink(Box::new(PlainSink(sink)));
    }
    fn events(&self) -> Receiver<TransportEvent> {
        self.events_rx.clone()
    }
    fn create_offer(&self, config: &StreamingConfig) -> Result<SessionDescription> {
        let _guard = self.signaling.lock();
        self.close_session();
        while self.events_rx.try_recv().is_ok() {}
        *self.config.lock() = config.clone();
        if let Some(sink) = self.sink.lock().as_mut() {
            sink.configure(config);
        }
        let (cancel, _) = watch::channel(false);
        let (gathered, _) = watch::channel(false);
        let shared = Arc::new(SessionState {
            active: AtomicBool::new(true),
            cancel,
            gathered,
            state: Mutex::new(PeerConnectionState::New),
            candidates: Mutex::new(Vec::new()),
            channels: Mutex::new(HashMap::new()),
            open: Mutex::new(HashMap::new()),
            video: Mutex::new(None),
            video_ssrc: AtomicU32::new(0),
            reports: SenderReports::default(),
            sink: self.sink.clone(),
            events: self.events_tx.clone(),
        });
        let servers = self.ice_servers.clone();
        let config = config.clone();
        let slot = self.session.clone();
        let result = self.worker.call(async move {
            let peer = PeerConnectionBuilder::new().with_media_engine(media_engine(&config)?)
                .with_interceptor_registry(rtc::interceptor::Registry::new().with(rtc::interceptor::Slot::ReceiverReport, ReportObserver::new(shared.reports.clone())))
                .with_configuration(RTCConfigurationBuilder::default().with_ice_servers(servers).build())
                .with_handler(Arc::new(Handler(shared.clone())))
                .with_data_channel_send_buffer_limit(256 * 1024)
                .with_udp_addrs(vec!["0.0.0.0:0"]).build().await.map_err(other)?;
            let peer: Arc<dyn PeerConnection> = Arc::new(peer);
            let (outgoing, mut outgoing_rx) = mpsc::channel::<Outgoing>(SEND_CAPACITY);
            let (feedback, mut feedback_rx) = mpsc::channel::<Feedback>(SEND_CAPACITY);
            let session = Arc::new(Session { peer: peer.clone(), shared: shared.clone(), outgoing, feedback });
            // Store early so a failed offer can always be cleaned up by close/retry/drop.
            *slot.lock() = Some(session);
            for kind in [RtpCodecKind::Video, RtpCodecKind::Audio] {
                peer.add_transceiver_from_kind(kind, Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Recvonly, ..Default::default()
                })).await.map_err(other)?;
            }
            for spec in data_channel_specs() {
                let channel = peer.create_data_channel(spec.label, Some(RTCDataChannelInit {
                    ordered: spec.ordered, max_retransmits: u16::try_from(spec.max_retransmits).ok(),
                    protocol: spec.protocol.into(), ..Default::default()
                })).await.map_err(other)?;
                shared.channels.lock().insert(spec.kind, channel.clone());
                tokio::spawn(poll_channel(shared.clone(), spec.kind, channel));
            }
            let tasks_shared = shared.clone();
            let mut cancel = shared.cancel.subscribe();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(1));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    if *cancel.borrow() { break; }
                    tokio::select! {
                        biased;
                        _ = cancel.changed() => break,
                        Some(packet) = feedback_rx.recv() => {
                            let track = tasks_shared.video.lock().clone();
                            if let Some(track) = track && let Err(error) = track.write_rtcp(prepare_feedback(packet, &tasks_shared.reports, std::time::Instant::now())).await {
                                tracing::debug!(target: LOG, %error, "RTCP feedback failed");
                            }
                        }
                        Some(message) = outgoing_rx.recv() => {
                            let channel = tasks_shared.channels.lock().get(&message.kind).cloned();
                            if let Some(channel) = channel {
                                let result = if message.text {
                                    channel.try_send_text(&String::from_utf8_lossy(&message.data)).await
                                } else { channel.try_send(BytesMut::from(message.data.as_slice())).await };
                                if let Err(error) = result {
                                    tracing::debug!(target: LOG, %error, "data-channel send failed");
                                    if message.kind != DataChannelKind::Input {
                                        tasks_shared.emit(TransportEvent::Failed(error.to_string()));
                                    }
                                }
                            }
                        }
                        _ = tick.tick() => {
                            let mut sink = tasks_shared.sink.lock();
                            if tasks_shared.active.load(Ordering::Acquire) && let Some(sink) = sink.as_mut() { sink.tick(now_ms()); }
                        }
                    }
                }
            });
            let offer = peer.create_offer(None).await.map_err(other)?;
            Ok(SessionDescription { kind: "offer".into(), sdp: offer.sdp })
        });
        if result.is_err() {
            self.close_session();
        }
        result
    }
    fn set_local_description(&self, description: &SessionDescription) -> Result<()> {
        let _guard = self.signaling.lock();
        let session = self.current()?;
        let description = rtc_description(description)?;
        self.worker.call(async move {
            let mut gathered = session.shared.gathered.subscribe();
            session
                .peer
                .set_local_description(description)
                .await
                .map_err(other)?;
            // The signaling API exchanges candidates in a batch, not by trickling.
            gathered.wait_for(|done| *done).await.map_err(other)?;
            Ok(())
        })
    }
    fn set_remote_description(&self, description: &SessionDescription) -> Result<()> {
        let _guard = self.signaling.lock();
        let session = self.current()?;
        let description = rtc_description(description)?;
        self.worker.call(async move {
            session
                .peer
                .set_remote_description(description)
                .await
                .map_err(other)
        })
    }
    fn add_remote_ice_candidate(&self, candidate: &IceCandidatePayload) -> Result<()> {
        let _guard = self.signaling.lock();
        let session = self.current()?;
        let candidate = RTCIceCandidateInit {
            candidate: candidate.candidate.trim().trim_start_matches("a=").into(),
            sdp_mid: Some(candidate.sdp_mid.clone()),
            sdp_mline_index: Some(u16::try_from(candidate.sdp_mline_index).map_err(other)?),
            username_fragment: (!candidate.username_fragment.is_empty())
                .then(|| candidate.username_fragment.clone()),
            ..Default::default()
        };
        self.worker.call(async move {
            session
                .peer
                .add_ice_candidate(candidate)
                .await
                .map_err(other)
        })
    }
    fn local_ice_candidates(&self) -> Vec<IceCandidatePayload> {
        self.current()
            .map(|s| s.shared.candidates.lock().clone())
            .unwrap_or_default()
    }
    fn connection_state(&self) -> PeerConnectionState {
        self.current()
            .map(|s| *s.shared.state.lock())
            .unwrap_or(PeerConnectionState::Closed)
    }
    fn send(&self, kind: DataChannelKind, data: &[u8]) -> Result<()> {
        self.enqueue(kind, data, false)
    }
    fn send_text(&self, kind: DataChannelKind, text: &str) -> Result<()> {
        self.enqueue(kind, text.as_bytes(), true)
    }
    fn data_channel_runtime_stats(&self, kind: DataChannelKind) -> DataChannelRuntimeStats {
        let Ok(session) = self.current() else {
            return DataChannelRuntimeStats::default();
        };
        let channel = session.shared.channels.lock().get(&kind).cloned();
        let Some(channel) = channel else {
            return DataChannelRuntimeStats::default();
        };
        self.worker
            .call(async move {
                Ok(DataChannelRuntimeStats {
                    ready_state: channel.ready_state().await.map_err(other)?.to_string(),
                    buffered_amount_bytes: channel.outstanding_bytes().await.map_err(other)? as u64,
                })
            })
            .unwrap_or_default()
    }
    fn close(&self) {
        let _guard = self.signaling.lock();
        self.close_session();
    }
    fn collect_stats(&self) -> StreamingStatsSnapshot {
        self.sink
            .lock()
            .as_ref()
            .map(|sink| sink.stats())
            .unwrap_or_default()
    }
}
impl WebRtcTransport {
    fn enqueue(&self, kind: DataChannelKind, data: &[u8], text: bool) -> Result<()> {
        let session = self.current()?;
        if !session
            .shared
            .open
            .lock()
            .get(&kind)
            .copied()
            .unwrap_or(false)
        {
            return Err(Error::ChannelNotOpen(data_channel_kind_name(kind).into()));
        }
        session
            .outgoing
            .try_send(Outgoing {
                kind,
                data: data.to_vec(),
                text,
            })
            .map_err(|_| Error::SendFailed(data_channel_kind_name(kind).into()))
    }
}
fn rtc_description(description: &SessionDescription) -> Result<RTCSessionDescription> {
    // The shared SDP processor normalizes line endings to LF; webrtc-rs compares
    // a local offer byte-for-byte against its original CRLF serialization.
    let sdp = description.sdp.lines().collect::<Vec<_>>().join("\r\n") + "\r\n";
    match description.kind.as_str() {
        "offer" => RTCSessionDescription::offer(sdp).map_err(other),
        "answer" => RTCSessionDescription::answer(sdp).map_err(other),
        _ => Err(Error::Other(format!(
            "unsupported SDP type: {}",
            description.kind
        ))),
    }
}

struct FeedbackSender(Weak<Mutex<Option<Arc<Session>>>>);
impl FeedbackSender {
    fn send(&self, packet: impl FnOnce(u32) -> Feedback) {
        let Some(slot) = self.0.upgrade() else {
            return;
        };
        let session = slot.lock().clone();
        if let Some(session) = session {
            let ssrc = session.shared.video_ssrc.load(Ordering::Relaxed);
            if ssrc != 0 {
                let _ = session.feedback.try_send(packet(ssrc));
            }
        }
    }
}
impl RtcpSender for FeedbackSender {
    fn nack(&self, pid: u16, blp: u16) {
        self.send(|media_ssrc| {
            Box::new(TransportLayerNack {
                sender_ssrc: 1,
                media_ssrc,
                nacks: vec![NackPair {
                    packet_id: pid,
                    lost_packets: blp,
                }],
            })
        });
    }
    fn keyframe_request(&self) {
        self.send(|media_ssrc| {
            Box::new(PictureLossIndication {
                sender_ssrc: 1,
                media_ssrc,
            })
        });
    }
    fn remb(&self, bitrate_bps: u32) {
        self.send(|ssrc| {
            Box::new(ReceiverEstimatedMaximumBitrate {
                sender_ssrc: 1,
                bitrate: bitrate_bps as f32,
                ssrcs: vec![ssrc],
            })
        });
    }
    fn receiver_report(
        &self,
        fraction_lost: u8,
        cumulative_lost: u32,
        highest_seq_ext: u32,
        jitter: u32,
    ) {
        self.send(|ssrc| {
            Box::new(ReceiverReport {
                ssrc: 1,
                reports: vec![ReceptionReport {
                    ssrc,
                    fraction_lost,
                    total_lost: cumulative_lost.min(0x7fffff),
                    last_sequence_number: highest_seq_ext,
                    jitter,
                    ..Default::default()
                }],
                ..Default::default()
            })
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtc::media_stream::MediaStreamTrack;
    use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodingParameters, RTCRtpEncodingParameters};
    use std::time::Instant;
    use webrtc::media_stream::track_local::{TrackLocal, static_rtp::TrackLocalStaticRTP};

    struct PacketSink(Sender<(bool, Vec<u8>)>);
    impl MediaSink for PacketSink {
        fn on_video_rtp(&mut self, packet: &[u8], _: u64) {
            self.0.send((true, packet.to_vec())).unwrap();
        }
        fn on_audio_rtp(&mut self, packet: &[u8], _: u64) {
            self.0.send((false, packet.to_vec())).unwrap();
        }
        fn tick(&mut self, _: u64) {}
        fn reset(&mut self) {}
        fn stats(&self) -> StreamingStatsSnapshot {
            StreamingStatsSnapshot::default()
        }
    }

    fn transport() -> WebRtcTransport {
        let mut transport = WebRtcTransport::new();
        transport.ice_servers.clear(); // Deterministic host-only ICE; no external STUN.
        transport
    }

    #[test]
    fn offers_preserve_codecs_channels_and_support_retry_inside_tokio() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let transport = transport();
            for scale in [false, true] {
                let config = StreamingConfig {
                    scale_video_caps_to_1080: scale,
                    h264_profile_level_id: if scale {
                        "42e020".into()
                    } else {
                        String::new()
                    },
                    ..StreamingConfig::defaults()
                };
                let offer = transport.create_offer(&config).unwrap();
                assert!(
                    offer.sdp.contains("a=rtpmap:102 H264/90000"),
                    "{}",
                    offer.sdp
                );
                assert!(offer.sdp.contains("a=rtpmap:111 opus/48000/2"));
                assert_eq!(offer.sdp.matches("a=recvonly").count(), 2);
                assert!(offer.sdp.contains("m=application"));
                // Exercise the same SDP processing as the orchestrator.
                let offer = SessionDescription {
                    sdp: xcloud_proto::sdp::processor::process_local_sdp(
                        &offer.sdp,
                        &xcloud_proto::sdp::processor::ProcessOptions {
                            scale_video_caps_to_1080: config.scale_video_caps_to_1080,
                            h264_profile_level_id: config.h264_profile_level_id.clone(),
                            ..Default::default()
                        },
                    ),
                    ..offer
                };
                transport.set_local_description(&offer).unwrap();
                assert!(!transport.local_ice_candidates().is_empty());
                assert!(!transport.requires_candidates_before_remote_description());
                assert!(transport.send(DataChannelKind::Input, &[1]).is_err());
            }
            transport.close();
            transport.close();
            assert_eq!(transport.connection_state(), PeerConnectionState::Closed);
            assert!(transport.local_ice_candidates().is_empty());
        });
    }

    struct EchoHandler {
        gathered: watch::Sender<bool>,
        specs: Sender<(String, String, bool, Option<u16>)>,
    }
    #[async_trait::async_trait]
    impl PeerConnectionEventHandler for EchoHandler {
        async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
            if state == RTCIceGatheringState::Complete {
                self.gathered.send_replace(true);
            }
        }
        async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
            let specs = self.specs.clone();
            tokio::spawn(async move {
                specs
                    .send((
                        channel.label().await.unwrap(),
                        channel.protocol().await.unwrap(),
                        channel.ordered().await.unwrap(),
                        channel.max_retransmits().await.unwrap(),
                    ))
                    .unwrap();
                while let Some(event) = channel.poll().await {
                    if let DataChannelEvent::OnMessage(message) = event {
                        if message.is_string {
                            channel
                                .send_text(std::str::from_utf8(&message.data).unwrap())
                                .await
                                .unwrap();
                        } else {
                            channel
                                .send(BytesMut::from(message.data.as_ref()))
                                .await
                                .unwrap();
                        }
                    }
                }
            });
        }
    }

    #[test]
    fn loopback_negotiates_channels_and_delivers_text_binary_and_rtp() {
        let transport = transport();
        let offer = transport
            .create_offer(&StreamingConfig::defaults())
            .unwrap();
        transport.set_local_description(&offer).unwrap();
        let candidates = transport.local_ice_candidates();
        let (specs_tx, specs_rx) = crossbeam_channel::unbounded();
        let (packets_tx, packets_rx) = crossbeam_channel::unbounded();
        transport.set_media_sink(Box::new(PacketSink(packets_tx)));
        let (server, answer, tracks) = transport.worker.call(async move {
            let (gathered, mut done) = watch::channel(false);
            let peer = PeerConnectionBuilder::new().with_media_engine(media_engine(&StreamingConfig::defaults())?)
                .with_handler(Arc::new(EchoHandler { gathered, specs: specs_tx }))
                .with_udp_addrs(vec!["0.0.0.0:0"]).build().await.map_err(other)?;
            peer.set_remote_description(rtc_description(&offer)?).await.map_err(other)?;
            for candidate in candidates {
                peer.add_ice_candidate(RTCIceCandidateInit { candidate: candidate.candidate,
                    sdp_mid: Some(candidate.sdp_mid), sdp_mline_index: Some(candidate.sdp_mline_index as u16),
                    ..Default::default() }).await.map_err(other)?;
            }
            let mut tracks = Vec::new();
            for (kind, mime, rate, channels, ssrc, pt) in [
                (RtpCodecKind::Video, "video/H264", 90_000, 0, 1234, 102),
                (RtpCodecKind::Audio, "audio/opus", 48_000, 2, 5678, 111),
            ] {
                let track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
                    "test-stream".into(), mime.into(), mime.into(), kind,
                    vec![RTCRtpEncodingParameters {
                        rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
                        codec: RTCRtpCodec { mime_type: mime.into(), clock_rate: rate, channels,
                            sdp_fmtp_line: if pt == 102 { "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".into() } else { "minptime=10;useinbandfec=1;stereo=1".into() },
                            ..Default::default() }, active: true, ..Default::default()
                    }],
                )));
                peer.add_track(track.clone()).await.map_err(other)?;
                tracks.push((track, ssrc, pt));
            }
            let answer = peer.create_answer(None).await.map_err(other)?;
            peer.set_local_description(answer).await.map_err(other)?;
            done.wait_for(|value| *value).await.map_err(other)?;
            let answer = peer.local_description().await.ok_or(Error::OfferFailed)?;
            Ok((peer, SessionDescription { kind: "answer".into(), sdp: answer.sdp }, tracks))
        }).unwrap();
        transport.set_remote_description(&answer).unwrap();
        let events = transport.events();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut open = Vec::new();
        while open.len() < 4 {
            let event = events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            if let TransportEvent::ChannelOpened(kind) = event {
                open.push(kind);
            }
        }
        for _ in 0..4 {
            let (label, protocol, ordered, retransmits) =
                specs_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let spec = data_channel_specs()
                .iter()
                .find(|spec| spec.label == label)
                .unwrap();
            assert_eq!(protocol, spec.protocol);
            assert_eq!(ordered, spec.ordered);
            // rtc 0.21.1 does not populate max_retransmits on received DCEP
            // handles; verify the originating channel instead.
            assert_eq!(retransmits, None);
            let channel = transport
                .current()
                .unwrap()
                .shared
                .channels
                .lock()
                .get(&spec.kind)
                .unwrap()
                .clone();
            let actual = transport
                .worker
                .call(async move { channel.max_retransmits().await.map_err(other) })
                .unwrap();
            assert_eq!(actual, u16::try_from(spec.max_retransmits).ok());
        }
        for kind in open {
            transport.send_text(kind, "hello").unwrap();
            transport.send(kind, &[0, 255, 1, 0]).unwrap();
        }
        let mut text_count = 0;
        let mut binary_count = 0;
        while text_count + binary_count < 8 {
            match events.recv_timeout(Duration::from_secs(5)).unwrap() {
                TransportEvent::TextReceived { text, .. } => {
                    assert_eq!(text, "hello");
                    text_count += 1;
                }
                TransportEvent::DataReceived { data, .. } => {
                    assert_eq!(data, [0, 255, 1, 0]);
                    binary_count += 1;
                }
                _ => {}
            }
        }
        assert_eq!((text_count, binary_count), (4, 4));
        assert_eq!(
            transport
                .data_channel_runtime_stats(DataChannelKind::Input)
                .ready_state,
            "open"
        );
        transport
            .worker
            .call(async move {
                for (track, ssrc, payload_type) in tracks {
                    track
                        .write_rtp(rtc::rtp::Packet {
                            header: rtc::rtp::Header {
                                version: 2,
                                payload_type,
                                sequence_number: 42,
                                timestamp: 9000,
                                ssrc,
                                marker: true,
                                ..Default::default()
                            },
                            payload: bytes::Bytes::from_static(&[0x65, 1, 2, 3]),
                        })
                        .await
                        .map_err(other)?;
                }
                Ok(())
            })
            .unwrap();
        let mut kinds = Vec::new();
        for _ in 0..2 {
            let (video, packet) = packets_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(&packet[packet.len() - 4..], &[0x65, 1, 2, 3]);
            assert_eq!(u16::from_be_bytes([packet[2], packet[3]]), 42);
            kinds.push(video);
        }
        kinds.sort();
        assert_eq!(kinds, [false, true]);
        // Feedback can be submitted from inside the sink without blocking the worker.
        transport.request_keyframe();
        transport.close();
        transport
            .worker
            .call(async move { server.close().await.map_err(other) })
            .unwrap();
    }
}
