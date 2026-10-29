//! Port of qt/src/streaming/NativeMediaPipeline.{h,cpp}, with the frame hand-off of
//! qt/src/streaming/VideoFrameMailbox.cpp and the pipeline wiring of
//! qt/src/streaming/LibPeerWebRtcBridge.cpp (`setRtcpSender`, `configure`, the `tick`
//! call per pump iteration, `pushVideoRtp`/`pushAudioRtp` from the track callbacks).
//!
//! Takes the raw RTP packets the transport delivers on its pump thread, runs them through
//! `VideoJitterBuffer` / `AudioJitterBuffer`, decodes Opus inline and hands complete H.264
//! access units to the decode thread. Also drives the RTCP feedback loop (NACK, PLI, REMB,
//! receiver reports) that the sender needs to keep its bitrate up.
//!
//! Differences from the C++ (docs/archive/PORTING_PLAN.md section 4):
//! * Video decoding is no longer inline: access units go to an [`AuSink`] (normally
//!   [`DecodeThread`]) that never blocks. When the sink drops an access unit, or the decoder
//!   reports bad data, the pipeline gates on the next IDR exactly like after a
//!   jitter-buffer drop and requests a keyframe with the same rate limits. The C++ decoder
//!   saw every access unit, so in-band SPS/PPS (xCloud repeats them without an IDR) always
//!   reached it; the gate keeps that by forwarding the parameter sets of a gated access
//!   unit and discarding only its picture.
//! * A decoder that cannot run at all (`H264Decoder::m_valid == false` in the C++) is a
//!   decode thread that has exited: its access units are discarded without any RTCP or
//!   control-channel recovery, like the null `QImage` the C++ silently ignored.
//! * Decode timing, frame counts and the frame size come back from the decode thread via
//!   [`AuSink::take_feedback`]; the Qt signals became [`PipelineEvent`]s on a channel.

use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::Sender;
use xcloud_media::OpusDecoder;
use xcloud_media::annexb::{find_parameter_sets, is_idr};
use xcloud_proto::audio_jitter::AudioJitterBuffer;
use xcloud_proto::rtp::parse_rtp_packet;
use xcloud_proto::streaming_config::StreamingConfig;
use xcloud_proto::streaming_types::StreamingStatsSnapshot;
use xcloud_proto::video_jitter::{Stats as VideoStats, VideoJitterBuffer};
use xcloud_webrtc::{MediaSink, RtcpSender};

use crate::decode_thread::{DecodeFeedback, DecodeLatency, DecodeThread, SubmitOutcome};

const LOG: &str = "Video";

// REMB asks the sender to ramp to the configured cap. Without it a libwebrtc sender stays
// pinned at its starvation floor.
const REMB_INTERVAL_MS: u64 = 1000;
const REPORT_INTERVAL_MS: u64 = 1000;
const BITRATE_WINDOW_MS: u64 = 1000;
const HEALTH_LOG_INTERVAL_MS: u64 = 5000;
/// During recovery each P-frame asks for an IDR. Rate-limit requests so they cannot
/// provoke a stream of expensive keyframes and more packet loss.
const KEYFRAME_REQUEST_INTERVAL_MS: u64 = 250;
/// Control-channel recovery (`keyframeRecoveryNeeded`) is limited harder than the PLI.
const CONTROL_RECOVERY_INTERVAL_MS: u64 = 1000;

/// What the `NativeMediaPipeline` emitted as Qt signals.
#[derive(Debug, Clone, PartialEq)]
pub enum PipelineEvent {
    /// `statsChanged()`, with the snapshot it refers to.
    StatsChanged(StreamingStatsSnapshot),
    /// `keyframeRecoveryNeeded()`: ask for a keyframe over the control channel.
    KeyframeRecoveryNeeded,
    /// The decoded frame size changed (the C++ wrote it into `negotiatedWidth/Height`).
    Resolution { width: u32, height: u32 },
}

/// Receives complete Annex-B access units from the pipeline on the pump thread.
/// Implemented by [`DecodeThread`] and, for tests, by any `FnMut(&[u8], u64)`.
pub trait AuSink: Send {
    /// Hands over one access unit; must never block.
    fn submit(&mut self, au: &[u8], now_ms: u64) -> SubmitOutcome;
    /// Decoder feedback accumulated since the previous call.
    fn take_feedback(&mut self) -> DecodeFeedback {
        DecodeFeedback::default()
    }
    /// The stream restarts: drop queued work and pending frames.
    fn reset(&mut self) {}
}

impl AuSink for DecodeThread {
    fn submit(&mut self, au: &[u8], _now_ms: u64) -> SubmitOutcome {
        DecodeThread::submit(self, au, Instant::now())
    }

    fn take_feedback(&mut self) -> DecodeFeedback {
        DecodeThread::take_feedback(self)
    }

    fn reset(&mut self) {
        DecodeThread::reset(self);
    }
}

impl<F: FnMut(&[u8], u64) + Send> AuSink for F {
    fn submit(&mut self, au: &[u8], now_ms: u64) -> SubmitOutcome {
        self(au, now_ms);
        SubmitOutcome::Accepted
    }
}

/// Interleaved 16-bit PCM consumer: `(pcm, sample_rate, channels)`.
pub type AudioSink = Box<dyn FnMut(&[i16], u32, u32) + Send>;

/// Decode-side counters the pipeline keeps for the HUD and the health log.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DecodeStats {
    /// Latest window's decode latency (submit -> output callback).
    pub decode: DecodeLatency,
    /// Latest window's queue wait plus synchronous decode call.
    pub queue: DecodeLatency,
    /// Latest window's jitter wait: first RTP packet of an access unit seen by the
    /// `VideoJitterBuffer` → access unit emitted (PORTING_PLAN §4 "jitter wait"). Counted
    /// for every emitted access unit, gated or not; refreshed with the bitrate window.
    pub jitter: DecodeLatency,
    /// Frames the decoder delivered, cumulative since `reset`.
    pub frames_decoded: u32,
    /// Access units the decoder rejected, cumulative since `reset`.
    pub decode_errors: u32,
    /// Access units dropped because the decode queue was full, cumulative.
    pub dropped_full: u32,
    /// Non-IDR access units whose picture was discarded while waiting for an IDR after a
    /// drop, cumulative (their SPS/PPS, if any, still went to the decoder).
    pub gated: u32,
    /// Access units discarded because the decode thread is gone (no usable decoder),
    /// cumulative. These never trigger keyframe recovery.
    pub unavailable: u32,
}

/// Port of `NativeMediaPipeline`.
pub struct NativeMediaPipeline {
    sender: Option<Arc<dyn RtcpSender>>,
    video_sink: Box<dyn AuSink>,
    audio_sink: AudioSink,
    events: Sender<PipelineEvent>,

    video_jitter: VideoJitterBuffer,
    audio_jitter: AudioJitterBuffer,
    audio_decoder: OpusDecoder,
    pcm: Vec<i16>,

    video_stats: VideoStats,
    stats: StreamingStatsSnapshot,
    decode_stats: DecodeStats,

    max_bitrate_bps: u32,
    want_keyframe: bool,
    /// Set after the decode thread dropped or rejected an access unit: discard every
    /// picture until the next IDR, like `VideoJitterBuffer::waiting_keyframe`. Parameter
    /// sets still pass so the decoder is ready for that IDR.
    gate_until_idr: bool,
    last_control_recovery_ms: Option<u64>,
    last_keyframe_request_ms: Option<u64>,
    last_frame_ms: Option<u64>,
    max_frame_gap_ms: u64,
    max_decode_ms: f64,
    last_health_log_ms: Option<u64>,
    last_remb_ms: Option<u64>,
    last_report_ms: Option<u64>,
    bitrate_window_start_ms: Option<u64>,
    bitrate_window_bytes: u64,
    video_width: u32,
    video_height: u32,
    frames_this_window: u32,
    frames_submitted: u32,
    /// Jitter-wait accumulators of the current window (`DecodeStats::jitter`).
    jitter_samples: u32,
    jitter_sum_ms: u64,
    jitter_max_ms: u64,
}

impl NativeMediaPipeline {
    /// `sender` is `transport.rtcp_sender()`; `None` matches the C++ before `setRtcpSender`
    /// (no RTCP is sent).
    pub fn new(
        sender: Option<Arc<dyn RtcpSender>>,
        video_sink: impl AuSink + 'static,
        audio_sink: AudioSink,
        events: Sender<PipelineEvent>,
    ) -> Self {
        Self {
            sender,
            video_sink: Box::new(video_sink),
            audio_sink,
            events,
            video_jitter: VideoJitterBuffer::new(),
            audio_jitter: AudioJitterBuffer::new(),
            audio_decoder: OpusDecoder::new(48_000, 2),
            pcm: Vec::new(),
            video_stats: VideoStats::default(),
            stats: StreamingStatsSnapshot::default(),
            decode_stats: DecodeStats::default(),
            max_bitrate_bps: 20_000_000,
            want_keyframe: false,
            gate_until_idr: false,
            last_control_recovery_ms: None,
            last_keyframe_request_ms: None,
            last_frame_ms: None,
            max_frame_gap_ms: 0,
            max_decode_ms: 0.0,
            last_health_log_ms: None,
            last_remb_ms: None,
            last_report_ms: None,
            bitrate_window_start_ms: None,
            bitrate_window_bytes: 0,
            video_width: 0,
            video_height: 0,
            frames_this_window: 0,
            frames_submitted: 0,
            jitter_samples: 0,
            jitter_sum_ms: 0,
            jitter_max_ms: 0,
        }
    }

    /// `setRtcpSender`.
    pub fn set_rtcp_sender(&mut self, sender: Option<Arc<dyn RtcpSender>>) {
        self.sender = sender;
    }

    /// `configure`: only the REMB ceiling matters here.
    pub fn configure(&mut self, config: &StreamingConfig) {
        self.max_bitrate_bps = config.max_bitrate_kbps.max(1).saturating_mul(1000);
    }

    /// `reset`: new peer connection, everything starts over.
    pub fn reset(&mut self) {
        self.video_jitter.reset();
        self.audio_jitter.reset();
        self.video_sink.reset();
        self.audio_decoder.reset();
        self.pcm.clear();
        self.video_stats = VideoStats::default();
        self.stats = StreamingStatsSnapshot::default();
        self.decode_stats = DecodeStats::default();
        self.want_keyframe = false;
        self.gate_until_idr = false;
        self.last_keyframe_request_ms = None;
        self.last_control_recovery_ms = None;
        self.last_frame_ms = None;
        self.max_frame_gap_ms = 0;
        self.max_decode_ms = 0.0;
        self.last_health_log_ms = None;
        self.video_width = 0;
        self.video_height = 0;
        self.last_remb_ms = None;
        self.last_report_ms = None;
        self.bitrate_window_start_ms = None;
        self.bitrate_window_bytes = 0;
        self.frames_this_window = 0;
        self.frames_submitted = 0;
        self.jitter_samples = 0;
        self.jitter_sum_ms = 0;
        self.jitter_max_ms = 0;
    }

    /// `stats()`: the snapshot of the last completed window.
    pub fn stats(&self) -> StreamingStatsSnapshot {
        self.stats.clone()
    }

    /// `videoStats()`.
    pub fn video_stats(&self) -> VideoStats {
        self.video_stats
    }

    /// `audioPacketsLost()`.
    pub fn audio_packets_lost(&self) -> u32 {
        self.audio_jitter.lost()
    }

    /// Decode-side counters (the decode thread's share of what `maxDecode` logged).
    pub fn decode_stats(&self) -> DecodeStats {
        self.decode_stats
    }

    /// True while the pipeline discards everything until an IDR arrives, either in the
    /// jitter buffer or after a decode-thread drop.
    pub fn waiting_keyframe(&self) -> bool {
        self.video_jitter.waiting_keyframe() || self.gate_until_idr
    }

    /// `pushVideoRtp`.
    pub fn push_video_rtp(&mut self, packet: &[u8], now_ms: u64) {
        if self.bitrate_window_start_ms.is_none() {
            self.bitrate_window_start_ms = Some(now_ms);
        }
        self.bitrate_window_bytes += packet.len() as u64;

        self.want_keyframe = false;
        self.absorb_decoder_feedback();

        let Self {
            video_jitter,
            video_sink,
            sender,
            gate_until_idr,
            decode_stats,
            last_frame_ms,
            max_frame_gap_ms,
            frames_this_window,
            frames_submitted,
            jitter_samples,
            jitter_sum_ms,
            jitter_max_ms,
            ..
        } = self;
        let mut want_keyframe = false;
        let outcome = video_jitter.receive_with_wait(
            packet,
            now_ms,
            |au, wait_ms| {
                *jitter_samples += 1;
                *jitter_sum_ms += wait_ms;
                *jitter_max_ms = (*jitter_max_ms).max(wait_ms);
                if *gate_until_idr {
                    if !is_idr(au) {
                        decode_stats.gated += 1;
                        want_keyframe = true;
                        // The picture is useless without its references, but the C++
                        // decoder saw every access unit and so never missed an in-band
                        // SPS/PPS change. Hand the parameter sets on alone so the IDR
                        // that ends the gate decodes against the current ones. Losing
                        // them to a full queue is tolerable: xCloud repeats them.
                        if let Some(params) = parameter_sets_only(au) {
                            let _ = video_sink.submit(&params, now_ms);
                        }
                        return;
                    }
                    *gate_until_idr = false;
                }
                match video_sink.submit(au, now_ms) {
                    SubmitOutcome::Accepted => {
                        if let Some(last) = *last_frame_ms {
                            *max_frame_gap_ms =
                                (*max_frame_gap_ms).max(now_ms.saturating_sub(last));
                        }
                        *last_frame_ms = Some(now_ms);
                        *frames_this_window += 1;
                        *frames_submitted += 1;
                    }
                    SubmitOutcome::DroppedFull => {
                        decode_stats.dropped_full += 1;
                        *gate_until_idr = true;
                        want_keyframe = true;
                    }
                    // No decoder at all: the C++ produced no frame and asked for nothing.
                    SubmitOutcome::Unavailable => decode_stats.unavailable += 1,
                }
            },
            |pid, blp| {
                if let Some(sender) = sender.as_ref() {
                    sender.nack(pid, blp);
                }
            },
        );
        self.want_keyframe |= want_keyframe || outcome.want_keyframe;

        self.video_stats = self.video_jitter.stats();
        self.maybe_request_keyframe(now_ms);
    }

    /// `pushAudioRtp`.
    pub fn push_audio_rtp(&mut self, packet: &[u8], _now_ms: u64) {
        let Ok(view) = parse_rtp_packet(packet) else {
            return;
        };
        if view.payload.is_empty() {
            return;
        }
        self.audio_jitter.push(view.sequence, view.payload);
        self.drain_audio();
    }

    /// `drainAudio`.
    fn drain_audio(&mut self) {
        if !self.audio_decoder.is_valid() {
            return;
        }
        self.pcm.clear();
        while let Some(payload) = self.audio_jitter.pop() {
            if !payload.is_empty() {
                self.audio_decoder.decode(&payload, &mut self.pcm);
            }
        }
        if !self.pcm.is_empty() {
            (self.audio_sink)(
                &self.pcm,
                self.audio_decoder.sample_rate(),
                u32::from(self.audio_decoder.channels()),
            );
        }
    }

    /// `tick`: periodic housekeeping, called once per pump iteration.
    pub fn tick(&mut self, now_ms: u64) {
        let Some(window_start) = self.bitrate_window_start_ms else {
            self.bitrate_window_start_ms = Some(now_ms);
            self.last_report_ms = Some(now_ms);
            self.last_remb_ms = Some(now_ms);
            return;
        };

        // Decoder trouble (bad data) shows up here even when no RTP arrives.
        self.want_keyframe = false;
        self.absorb_decoder_feedback();
        self.maybe_request_keyframe(now_ms);

        let window_ms = now_ms.saturating_sub(window_start);
        if window_ms >= BITRATE_WINDOW_MS {
            self.stats.bitrate_kbps = ((self.bitrate_window_bytes * 8) / window_ms) as i32;
            self.stats.frames_per_second =
                f64::from(self.frames_this_window) * 1000.0 / window_ms as f64;
            self.stats.packets_lost = self.video_stats.dropped as i32;
            self.stats.frames_lost = self.video_stats.dropped as i32;
            self.stats.renderer_frames_received = self.video_stats.frames as i32;
            self.stats.renderer_frames_drawn = self.frames_submitted as i32;
            self.stats.renderer_frames_dropped = (self.decode_stats.dropped_full
                + self.decode_stats.gated
                + self.decode_stats.unavailable)
                as i32;
            self.stats.renderer_mode = "webrtc-video".to_owned();
            self.stats.audio_packets_lost = self.audio_jitter.lost() as i32;
            self.stats.decode_time_ms = self.decode_stats.decode.avg_ms;
            self.stats.negotiated_width = self.video_width as i32;
            self.stats.negotiated_height = self.video_height as i32;
            self.decode_stats.jitter = DecodeLatency::from_window(
                self.jitter_samples,
                self.jitter_sum_ms * 1000,
                self.jitter_max_ms * 1000,
            );
            self.bitrate_window_start_ms = Some(now_ms);
            self.bitrate_window_bytes = 0;
            self.frames_this_window = 0;
            self.jitter_samples = 0;
            self.jitter_sum_ms = 0;
            self.jitter_max_ms = 0;
            let _ = self
                .events
                .try_send(PipelineEvent::StatsChanged(self.stats.clone()));
            if self
                .last_health_log_ms
                .is_none_or(|last| now_ms.saturating_sub(last) >= HEALTH_LOG_INTERVAL_MS)
            {
                self.last_health_log_ms = Some(now_ms);
                tracing::info!(
                    target: LOG,
                    fps = format_args!("{:.1}", self.stats.frames_per_second),
                    bitrate_kbps = self.stats.bitrate_kbps,
                    max_frame_gap_ms = self.max_frame_gap_ms,
                    max_decode_ms = format_args!("{:.1}", self.max_decode_ms),
                    dropped = self.video_stats.dropped,
                    nacks = self.video_stats.nacks,
                    resyncs = self.video_stats.resyncs,
                    waiting_keyframe = self.waiting_keyframe(),
                    decode_queue_drops = self.decode_stats.dropped_full,
                    decode_errors = self.decode_stats.decode_errors,
                    resolution = format_args!("{}x{}", self.video_width, self.video_height),
                    "fps={:.1} bitrate={}kbps maxFrameGap={}ms maxDecode={:.1}ms dropped={} nacks={} resyncs={} waitingKeyframe={} resolution={}x{}",
                    self.stats.frames_per_second,
                    self.stats.bitrate_kbps,
                    self.max_frame_gap_ms,
                    self.max_decode_ms,
                    self.video_stats.dropped,
                    self.video_stats.nacks,
                    self.video_stats.resyncs,
                    self.waiting_keyframe(),
                    self.video_width,
                    self.video_height,
                );
                self.max_frame_gap_ms = 0;
                self.max_decode_ms = 0.0;
            }
        }

        if let Some(sender) = self.sender.as_ref()
            && due(self.last_remb_ms, now_ms, REMB_INTERVAL_MS)
        {
            self.last_remb_ms = Some(now_ms);
            // Ask for the configured ceiling; the sender's own estimator decides.
            sender.remb(self.max_bitrate_bps);
        }

        if let Some(sender) = self.sender.as_ref()
            && due(self.last_report_ms, now_ms, REPORT_INTERVAL_MS)
        {
            self.last_report_ms = Some(now_ms);
            if let Some(report) = self.video_jitter.report_stats() {
                sender.receiver_report(
                    report.fraction_lost,
                    report.cumulative_lost,
                    report.highest_seq_ext,
                    0,
                );
            }
        }
    }

    /// Folds the decode thread's feedback into the counters; a rejected access unit
    /// gates on the next IDR like a jitter-buffer drop.
    fn absorb_decoder_feedback(&mut self) {
        let feedback = self.video_sink.take_feedback();
        if feedback.want_keyframe {
            self.want_keyframe = true;
            self.gate_until_idr = true;
        }
        self.decode_stats.frames_decoded += feedback.frames;
        self.decode_stats.decode_errors += feedback.errors;
        if feedback.decode.samples > 0 {
            self.decode_stats.decode = feedback.decode;
            self.max_decode_ms = self.max_decode_ms.max(feedback.decode.max_ms);
        }
        if feedback.queue.samples > 0 {
            self.decode_stats.queue = feedback.queue;
        }
        if let Some((width, height)) = feedback.resolution
            && (width, height) != (self.video_width, self.video_height)
        {
            self.video_width = width;
            self.video_height = height;
            self.stats.negotiated_width = width as i32;
            self.stats.negotiated_height = height as i32;
            let _ = self
                .events
                .try_send(PipelineEvent::Resolution { width, height });
        }
    }

    /// The rate-limited PLI + control-channel recovery of `pushVideoRtp`.
    fn maybe_request_keyframe(&mut self, now_ms: u64) {
        if !self.want_keyframe {
            return;
        }
        let Some(sender) = self.sender.as_ref() else {
            return;
        };
        if self
            .last_keyframe_request_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= KEYFRAME_REQUEST_INTERVAL_MS)
        {
            self.last_keyframe_request_ms = Some(now_ms);
            sender.keyframe_request();
            if self
                .last_control_recovery_ms
                .is_none_or(|last| now_ms.saturating_sub(last) >= CONTROL_RECOVERY_INTERVAL_MS)
            {
                self.last_control_recovery_ms = Some(now_ms);
                let _ = self.events.try_send(PipelineEvent::KeyframeRecoveryNeeded);
            }
        }
    }
}

/// The SPS/PPS of `au` as an Annex-B access unit of their own, or `None` when it carries
/// neither. What a gated access unit still hands to the decoder.
fn parameter_sets_only(au: &[u8]) -> Option<Vec<u8>> {
    const START_CODE: [u8; 4] = [0, 0, 0, 1];
    let (sps, pps) = find_parameter_sets(au);
    if sps.is_none() && pps.is_none() {
        return None;
    }
    let mut out = Vec::with_capacity(
        sps.map_or(0, <[u8]>::len) + pps.map_or(0, <[u8]>::len) + 2 * START_CODE.len(),
    );
    for nal in [sps, pps].into_iter().flatten() {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    }
    Some(out)
}

/// The C++ kept `-1` for "never" and tested `nowMs - last >= interval`, so a never-sent
/// REMB/report becomes due once `nowMs + 1 >= interval`. Mirrored exactly.
fn due(last: Option<u64>, now_ms: u64, interval_ms: u64) -> bool {
    match last {
        Some(last) => now_ms.saturating_sub(last) >= interval_ms,
        None => now_ms + 1 >= interval_ms,
    }
}

impl MediaSink for NativeMediaPipeline {
    fn on_video_rtp(&mut self, packet: &[u8], now_ms: u64) {
        self.push_video_rtp(packet, now_ms);
    }

    fn on_audio_rtp(&mut self, packet: &[u8], now_ms: u64) {
        self.push_audio_rtp(packet, now_ms);
    }

    fn tick(&mut self, now_ms: u64) {
        NativeMediaPipeline::tick(self, now_ms);
    }

    fn reset(&mut self) {
        NativeMediaPipeline::reset(self);
    }

    fn stats(&self) -> StreamingStatsSnapshot {
        NativeMediaPipeline::stats(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::Receiver;
    use parking_lot::Mutex;

    /// Port of the test's `FakeRtcpSender`.
    #[derive(Default)]
    struct FakeRtcpSender {
        nacks: Mutex<Vec<(u16, u16)>>,
        rembs: Mutex<Vec<u32>>,
        keyframe_requests: Mutex<u32>,
        reports: Mutex<u32>,
    }

    impl RtcpSender for FakeRtcpSender {
        fn nack(&self, pid: u16, blp: u16) {
            self.nacks.lock().push((pid, blp));
        }
        fn keyframe_request(&self) {
            *self.keyframe_requests.lock() += 1;
        }
        fn remb(&self, bitrate_bps: u32) {
            self.rembs.lock().push(bitrate_bps);
        }
        fn receiver_report(&self, _: u8, _: u32, _: u32, _: u32) {
            *self.reports.lock() += 1;
        }
    }

    /// Port of the test's `rtpPacket` helper (payload type 96, SSRC 1).
    fn rtp_packet(seq: u16, timestamp: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0u8; 12 + payload.len()];
        packet[0] = 0x80;
        packet[1] = (if marker { 0x80 } else { 0x00 }) | 96;
        packet[2] = (seq >> 8) as u8;
        packet[3] = seq as u8;
        packet[4] = (timestamp >> 24) as u8;
        packet[5] = (timestamp >> 16) as u8;
        packet[6] = (timestamp >> 8) as u8;
        packet[7] = timestamp as u8;
        packet[11] = 1;
        packet[12..].copy_from_slice(payload);
        packet
    }

    type Aus = Arc<Mutex<Vec<(Vec<u8>, u64)>>>;

    struct Harness {
        pipeline: NativeMediaPipeline,
        sender: Arc<FakeRtcpSender>,
        events: Receiver<PipelineEvent>,
        aus: Aus,
        pcm: Arc<Mutex<Vec<(usize, u32, u32)>>>,
    }

    fn harness(with_sender: bool) -> Harness {
        let sender = Arc::new(FakeRtcpSender::default());
        let (tx, events) = crossbeam_channel::unbounded();
        let aus: Aus = Arc::default();
        let pcm: Arc<Mutex<Vec<(usize, u32, u32)>>> = Arc::default();
        let (aus2, pcm2) = (Arc::clone(&aus), Arc::clone(&pcm));
        let rtcp: Option<Arc<dyn RtcpSender>> = with_sender.then(|| {
            let s: Arc<dyn RtcpSender> = sender.clone();
            s
        });
        let pipeline = NativeMediaPipeline::new(
            rtcp,
            move |au: &[u8], now_ms: u64| aus2.lock().push((au.to_vec(), now_ms)),
            Box::new(move |samples: &[i16], rate, channels| {
                pcm2.lock().push((samples.len(), rate, channels));
            }),
            tx,
        );
        Harness {
            pipeline,
            sender,
            events,
            aus,
            pcm,
        }
    }

    fn recovery_events(events: &Receiver<PipelineEvent>) -> usize {
        events
            .try_iter()
            .filter(|e| *e == PipelineEvent::KeyframeRecoveryNeeded)
            .count()
    }

    /// `TestNativePipeline::drivesRtcpFeedback`.
    #[test]
    fn drives_rtcp_feedback() {
        let mut h = harness(true);

        // A gap in the video sequence produces a NACK.
        h.pipeline
            .push_video_rtp(&rtp_packet(1, 1000, false, &[0x65, 0x01]), 0);
        h.pipeline
            .push_video_rtp(&rtp_packet(4, 1000, true, &[0x65, 0x04]), 5);
        let nacks = h.sender.nacks.lock().clone();
        assert!(!nacks.is_empty());
        assert_eq!(nacks[0].0, 2);

        // A tick past the REMB/report interval emits both.
        h.pipeline.tick(10);
        h.pipeline.tick(1500);
        assert!(!h.sender.rembs.lock().is_empty());
        assert_eq!(h.sender.rembs.lock()[0], 20_000_000);
        assert!(*h.sender.reports.lock() >= 1);
    }

    /// `TestNativePipeline::reportsBitrate`.
    #[test]
    fn reports_bitrate() {
        let mut h = harness(false);
        h.pipeline.tick(0);
        for i in 0..100u64 {
            h.pipeline
                .push_video_rtp(&rtp_packet(i as u16, i as u32, false, &[0x41; 100]), i);
        }
        h.pipeline.tick(1000);
        let stats = h.pipeline.stats();
        assert!(stats.bitrate_kbps > 0);
        assert_eq!(stats.bitrate_kbps, 89, "100 packets of 112 bytes in 1 s");
        assert_eq!(stats.renderer_mode, "webrtc-video");
        assert!(h.pipeline.video_stats().packets >= 100);
        assert!(matches!(
            h.events.try_recv(),
            Ok(PipelineEvent::StatsChanged(s)) if s.bitrate_kbps == 89
        ));
    }

    /// `TestNativePipeline::limitsKeyframeRequestsDuringRecovery`.
    #[test]
    fn limits_keyframe_requests_during_recovery() {
        let mut h = harness(true);
        // Complete P-frames while waiting for the initial IDR must not cause a PLI storm.
        for i in 0..60u64 {
            h.pipeline.push_video_rtp(
                &rtp_packet(i as u16, (i * 3000) as u32, true, &[0x41, 0x01]),
                i * 16,
            );
        }
        assert_eq!(*h.sender.keyframe_requests.lock(), 4);
        assert_eq!(recovery_events(&h.events), 1);
        h.pipeline.reset();
        h.pipeline
            .push_video_rtp(&rtp_packet(0, 0, true, &[0x41, 0x01]), 0);
        assert_eq!(*h.sender.keyframe_requests.lock(), 5);
        assert_eq!(recovery_events(&h.events), 1);
        assert!(h.aus.lock().is_empty(), "no P-frame reached the decoder");
    }

    #[test]
    fn complete_idr_reaches_the_au_sink_with_fps_accounting() {
        let mut h = harness(true);
        h.pipeline.tick(0);
        for i in 0..30u64 {
            h.pipeline.push_video_rtp(
                &rtp_packet(i as u16, (i * 3000) as u32, true, &[0x65, 0x01, 0x02]),
                i * 33,
            );
        }
        let aus = h.aus.lock().clone();
        assert_eq!(aus.len(), 30);
        assert_eq!(aus[0].0, vec![0, 0, 0, 1, 0x65, 0x01, 0x02]);
        assert_eq!(aus[5].1, 5 * 33);
        h.pipeline.tick(1000);
        let stats = h.pipeline.stats();
        assert!((stats.frames_per_second - 30.0).abs() < f64::EPSILON);
        assert_eq!(stats.renderer_frames_received, 30);
        assert_eq!(stats.renderer_frames_drawn, 30);
        assert_eq!(*h.sender.keyframe_requests.lock(), 0);
    }

    /// A sink that drops like a full decode queue, dies like a decode thread without a
    /// decoder, and reports decoder feedback.
    struct ScriptedSink {
        aus: Aus,
        drop_next: Arc<Mutex<bool>>,
        unavailable: Arc<Mutex<bool>>,
        feedback: Arc<Mutex<DecodeFeedback>>,
        resets: Arc<Mutex<u32>>,
    }

    impl AuSink for ScriptedSink {
        fn submit(&mut self, au: &[u8], now_ms: u64) -> SubmitOutcome {
            if *self.unavailable.lock() {
                return SubmitOutcome::Unavailable;
            }
            if std::mem::take(&mut *self.drop_next.lock()) {
                return SubmitOutcome::DroppedFull;
            }
            self.aus.lock().push((au.to_vec(), now_ms));
            SubmitOutcome::Accepted
        }
        fn take_feedback(&mut self) -> DecodeFeedback {
            std::mem::take(&mut *self.feedback.lock())
        }
        fn reset(&mut self) {
            *self.resets.lock() += 1;
        }
    }

    struct Scripted {
        pipeline: NativeMediaPipeline,
        sender: Arc<FakeRtcpSender>,
        events: Receiver<PipelineEvent>,
        aus: Aus,
        drop_next: Arc<Mutex<bool>>,
        unavailable: Arc<Mutex<bool>>,
        feedback: Arc<Mutex<DecodeFeedback>>,
        resets: Arc<Mutex<u32>>,
    }

    fn scripted() -> Scripted {
        let sender = Arc::new(FakeRtcpSender::default());
        let (tx, events) = crossbeam_channel::unbounded();
        let aus: Aus = Arc::default();
        let drop_next = Arc::new(Mutex::new(false));
        let unavailable = Arc::new(Mutex::new(false));
        let feedback = Arc::new(Mutex::new(DecodeFeedback::default()));
        let resets = Arc::new(Mutex::new(0));
        let sink = ScriptedSink {
            aus: Arc::clone(&aus),
            drop_next: Arc::clone(&drop_next),
            unavailable: Arc::clone(&unavailable),
            feedback: Arc::clone(&feedback),
            resets: Arc::clone(&resets),
        };
        let rtcp: Arc<dyn RtcpSender> = sender.clone();
        let pipeline = NativeMediaPipeline::new(Some(rtcp), sink, Box::new(|_, _, _| {}), tx);
        Scripted {
            pipeline,
            sender,
            events,
            aus,
            drop_next,
            unavailable,
            feedback,
            resets,
        }
    }

    const SPS: &[u8] = &[0x67, 0xaa];
    const PPS: &[u8] = &[0x68, 0xbb];

    /// One RTP packet carrying `nals` as a STAP-A aggregate (how xCloud ships SPS/PPS).
    fn stap_a(seq: u16, now_ms: u64, nals: &[&[u8]]) -> (Vec<u8>, u64) {
        let mut payload = vec![0x78u8]; // F=0, NRI=3, type 24
        for nal in nals {
            payload.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            payload.extend_from_slice(nal);
        }
        (
            rtp_packet(seq, u32::from(seq) * 3000, true, &payload),
            now_ms,
        )
    }

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|nal| [0u8, 0, 0, 1].into_iter().chain(nal.iter().copied()))
            .collect()
    }

    fn idr(seq: u16, now_ms: u64) -> (Vec<u8>, u64) {
        (
            rtp_packet(seq, u32::from(seq) * 3000, true, &[0x65, seq as u8]),
            now_ms,
        )
    }

    fn p_frame(seq: u16, now_ms: u64) -> (Vec<u8>, u64) {
        (
            rtp_packet(seq, u32::from(seq) * 3000, true, &[0x41, seq as u8]),
            now_ms,
        )
    }

    #[test]
    fn decode_queue_full_gates_until_the_next_idr_and_requests_a_keyframe() {
        let mut s = scripted();
        let (p, t) = idr(0, 0);
        s.pipeline.push_video_rtp(&p, t);
        assert_eq!(s.aus.lock().len(), 1);

        *s.drop_next.lock() = true;
        let (p, t) = p_frame(1, 16);
        s.pipeline.push_video_rtp(&p, t);
        assert_eq!(s.aus.lock().len(), 1, "dropped by the full queue");
        assert_eq!(*s.sender.keyframe_requests.lock(), 1);
        assert_eq!(recovery_events(&s.events), 1);
        assert!(s.pipeline.waiting_keyframe());

        // Following P-frames are gated, with rate-limited PLIs.
        for seq in 2..10u16 {
            let (p, t) = p_frame(seq, u64::from(seq) * 16);
            s.pipeline.push_video_rtp(&p, t);
        }
        assert_eq!(s.aus.lock().len(), 1);
        assert_eq!(s.pipeline.decode_stats().gated, 8);
        assert_eq!(s.pipeline.decode_stats().dropped_full, 1);
        assert_eq!(
            *s.sender.keyframe_requests.lock(),
            1,
            "within 250 ms of the first"
        );

        // The IDR opens the gate again.
        let (p, t) = idr(10, 400);
        s.pipeline.push_video_rtp(&p, t);
        assert_eq!(s.aus.lock().len(), 2);
        assert!(!s.pipeline.waiting_keyframe());
        let (p, t) = p_frame(11, 416);
        s.pipeline.push_video_rtp(&p, t);
        assert_eq!(s.aus.lock().len(), 3);
        assert_eq!(*s.sender.keyframe_requests.lock(), 1);
    }

    #[test]
    fn gated_access_units_still_forward_in_band_parameter_sets() {
        let mut s = scripted();
        let (p, t) = idr(0, 0);
        s.pipeline.push_video_rtp(&p, t);
        *s.drop_next.lock() = true;
        let (p, t) = p_frame(1, 16);
        s.pipeline.push_video_rtp(&p, t);
        assert!(s.pipeline.waiting_keyframe());
        assert_eq!(s.aus.lock().len(), 1);

        // Standalone SPS/PPS (xCloud repeats them without an IDR) reach the decoder.
        let (p, t) = stap_a(2, 32, &[SPS, PPS]);
        s.pipeline.push_video_rtp(&p, t);
        assert_eq!(s.aus.lock().len(), 2);
        assert_eq!(s.aus.lock()[1].0, annexb(&[SPS, PPS]));
        assert!(
            s.pipeline.waiting_keyframe(),
            "parameter sets do not end the gate"
        );

        // A P-frame with new parameter sets in front: the slice stays out, the sets go in.
        let new_sps: &[u8] = &[0x67, 0xcc];
        let (p, t) = stap_a(3, 48, &[new_sps, PPS, &[0x41, 0x03]]);
        s.pipeline.push_video_rtp(&p, t);
        assert_eq!(s.aus.lock().len(), 3);
        assert_eq!(s.aus.lock()[2].0, annexb(&[new_sps, PPS]));

        // A plain P-frame is discarded whole.
        let (p, t) = p_frame(4, 64);
        s.pipeline.push_video_rtp(&p, t);
        assert_eq!(s.aus.lock().len(), 3);
        assert_eq!(s.pipeline.decode_stats().gated, 3);
        assert_eq!(s.pipeline.decode_stats().dropped_full, 1);

        // The IDR opens the gate.
        let (p, t) = idr(5, 400);
        s.pipeline.push_video_rtp(&p, t);
        assert_eq!(s.aus.lock().len(), 4);
        assert!(!s.pipeline.waiting_keyframe());

        // Forwarded parameter sets are not frames.
        s.pipeline.tick(1000);
        let stats = s.pipeline.stats();
        assert_eq!(stats.renderer_frames_drawn, 2);
        assert_eq!(stats.renderer_frames_dropped, 4, "1 queue drop + 3 gated");
    }

    #[test]
    fn a_dead_decoder_discards_without_requesting_keyframes() {
        let mut s = scripted();
        let (p, t) = idr(0, 0);
        s.pipeline.push_video_rtp(&p, t);
        assert_eq!(s.aus.lock().len(), 1);

        // The decode thread exits (no decoder on this machine). The C++ `H264Decoder`
        // returned null images while `!m_valid`: no PLI, no control-channel recovery.
        *s.unavailable.lock() = true;
        for seq in 1..40u16 {
            let (p, t) = if seq % 10 == 0 {
                idr(seq, u64::from(seq) * 16)
            } else {
                p_frame(seq, u64::from(seq) * 16)
            };
            s.pipeline.push_video_rtp(&p, t);
        }
        s.pipeline.tick(1000);
        assert_eq!(s.aus.lock().len(), 1);
        assert_eq!(*s.sender.keyframe_requests.lock(), 0);
        assert_eq!(recovery_events(&s.events), 0);
        assert!(!s.pipeline.waiting_keyframe());
        let decode = s.pipeline.decode_stats();
        assert_eq!(decode.unavailable, 39);
        assert_eq!((decode.dropped_full, decode.gated), (0, 0));
        let stats = s.pipeline.stats();
        assert_eq!(stats.renderer_frames_drawn, 1);
        assert_eq!(stats.renderer_frames_dropped, 39);
    }

    #[test]
    fn an_idr_still_opens_the_gate_when_the_decoder_is_gone() {
        let mut s = scripted();
        let (p, t) = idr(0, 0);
        s.pipeline.push_video_rtp(&p, t);
        *s.drop_next.lock() = true;
        let (p, t) = p_frame(1, 16);
        s.pipeline.push_video_rtp(&p, t);
        assert!(s.pipeline.waiting_keyframe());
        assert_eq!(*s.sender.keyframe_requests.lock(), 1);

        // The decoder dies while the gate is up: gated P-frames keep asking for the IDR
        // (that is the jitter-buffer behaviour the C++ had too) ...
        *s.unavailable.lock() = true;
        for seq in 2..20u16 {
            let (p, t) = p_frame(seq, u64::from(seq) * 16);
            s.pipeline.push_video_rtp(&p, t);
        }
        assert_eq!(*s.sender.keyframe_requests.lock(), 2, "16 ms * 18 > 250 ms");

        // ... the IDR ends the gate even though nobody decodes it, and recovery stops.
        let (p, t) = idr(20, 320);
        s.pipeline.push_video_rtp(&p, t);
        assert!(!s.pipeline.waiting_keyframe());
        for seq in 21..60u16 {
            let (p, t) = p_frame(seq, u64::from(seq) * 16);
            s.pipeline.push_video_rtp(&p, t);
        }
        assert_eq!(*s.sender.keyframe_requests.lock(), 2);
        assert_eq!(recovery_events(&s.events), 1);
        assert_eq!(s.aus.lock().len(), 1);
    }

    #[test]
    fn decoder_bad_data_feedback_gates_and_requests_a_keyframe_from_tick() {
        let mut s = scripted();
        s.pipeline.tick(0);
        let (p, t) = idr(0, 0);
        s.pipeline.push_video_rtp(&p, t);
        s.feedback.lock().want_keyframe = true;
        s.feedback.lock().errors = 1;
        s.pipeline.tick(5);
        assert_eq!(*s.sender.keyframe_requests.lock(), 1);
        assert_eq!(recovery_events(&s.events), 1);
        assert!(s.pipeline.waiting_keyframe());
        assert_eq!(s.pipeline.decode_stats().decode_errors, 1);
        let (p, t) = p_frame(1, 16);
        s.pipeline.push_video_rtp(&p, t);
        assert_eq!(s.aus.lock().len(), 1, "gated until the IDR");
        let (p, t) = idr(2, 32);
        s.pipeline.push_video_rtp(&p, t);
        assert_eq!(s.aus.lock().len(), 2);
    }

    #[test]
    fn decoder_feedback_feeds_resolution_and_decode_stats() {
        let mut s = scripted();
        s.pipeline.tick(0);
        *s.feedback.lock() = DecodeFeedback {
            frames: 3,
            decode: DecodeLatency {
                samples: 3,
                avg_ms: 4.0,
                max_ms: 6.5,
            },
            resolution: Some((1920, 1080)),
            ..DecodeFeedback::default()
        };
        s.pipeline.tick(10);
        assert_eq!(
            s.events.try_recv(),
            Ok(PipelineEvent::Resolution {
                width: 1920,
                height: 1080
            })
        );
        // Same size again: no second event.
        s.feedback.lock().resolution = Some((1920, 1080));
        s.pipeline.tick(20);
        assert!(s.events.try_recv().is_err());
        s.pipeline.tick(1000);
        let stats = s.pipeline.stats();
        assert_eq!(
            (stats.negotiated_width, stats.negotiated_height),
            (1920, 1080)
        );
        assert!((stats.decode_time_ms - 4.0).abs() < f64::EPSILON);
        assert_eq!(s.pipeline.decode_stats().frames_decoded, 3);
        assert!(matches!(
            s.events.try_recv(),
            Ok(PipelineEvent::StatsChanged(_))
        ));
    }

    #[test]
    fn jitter_wait_is_averaged_over_the_stats_window() {
        let mut h = harness(true);
        h.pipeline.tick(0);
        // Single-packet IDR: complete on arrival (wait 0).
        let (p, t) = idr(0, 0);
        h.pipeline.push_video_rtp(&p, t);
        // Two-packet P-frame: first fragment at 16 ms, marker at 36 ms (wait 20).
        h.pipeline
            .push_video_rtp(&rtp_packet(1, 3000, false, &[0x5c, 0x81, 0xaa]), 16);
        h.pipeline
            .push_video_rtp(&rtp_packet(2, 3000, true, &[0x5c, 0x41, 0xbb]), 36);
        // Two-packet P-frame held 10 ms.
        h.pipeline
            .push_video_rtp(&rtp_packet(3, 6000, false, &[0x5c, 0x81, 0xcc]), 50);
        h.pipeline
            .push_video_rtp(&rtp_packet(4, 6000, true, &[0x5c, 0x41, 0xdd]), 60);
        assert_eq!(h.aus.lock().len(), 3);
        assert_eq!(
            h.pipeline.decode_stats().jitter,
            DecodeLatency::default(),
            "published at the window boundary"
        );
        h.pipeline.tick(1000);
        let jitter = h.pipeline.decode_stats().jitter;
        assert_eq!(jitter.samples, 3);
        assert!((jitter.avg_ms - 10.0).abs() < f64::EPSILON);
        assert!((jitter.max_ms - 20.0).abs() < f64::EPSILON);
        // The next window starts empty.
        h.pipeline.tick(2000);
        assert_eq!(h.pipeline.decode_stats().jitter, DecodeLatency::default());
    }

    #[test]
    fn reset_resets_the_sink_and_clears_stats() {
        let mut s = scripted();
        s.pipeline.tick(0);
        s.pipeline
            .push_video_rtp(&rtp_packet(0, 0, true, &[0x65; 500]), 0);
        s.pipeline.tick(1000);
        assert!(s.pipeline.stats().bitrate_kbps > 0);
        s.pipeline.reset();
        assert_eq!(*s.resets.lock(), 1);
        assert_eq!(s.pipeline.stats(), StreamingStatsSnapshot::default());
        assert_eq!(s.pipeline.video_stats(), VideoStats::default());
        assert_eq!(s.pipeline.decode_stats(), DecodeStats::default());
    }

    #[test]
    fn configure_sets_the_remb_ceiling() {
        let mut h = harness(true);
        let config = StreamingConfig {
            max_bitrate_kbps: 12_000,
            ..StreamingConfig::default()
        };
        h.pipeline.configure(&config);
        h.pipeline.tick(0);
        h.pipeline.tick(1000);
        assert_eq!(h.sender.rembs.lock().clone(), vec![12_000_000]);
        h.pipeline.tick(1500);
        assert_eq!(h.sender.rembs.lock().len(), 1, "once per second");
        h.pipeline.tick(2000);
        assert_eq!(h.sender.rembs.lock().len(), 2);
    }

    #[test]
    fn health_log_runs_once_per_five_seconds() {
        // No assertion on the log itself; this exercises the 5 s branch for coverage and
        // checks the max counters are reset by it.
        let mut h = harness(true);
        h.pipeline.tick(0);
        let (p, t) = idr(0, 0);
        h.pipeline.push_video_rtp(&p, t);
        let (p, t) = idr(1, 500);
        h.pipeline.push_video_rtp(&p, t);
        assert_eq!(h.pipeline.max_frame_gap_ms, 500);
        h.pipeline.tick(1000);
        assert_eq!(
            h.pipeline.max_frame_gap_ms, 0,
            "first window logs and resets"
        );
        let (p, t) = idr(2, 1100);
        h.pipeline.push_video_rtp(&p, t);
        h.pipeline.tick(2100);
        assert_eq!(
            h.pipeline.max_frame_gap_ms, 600,
            "kept until the next 5 s log"
        );
        h.pipeline.tick(6000);
        assert_eq!(h.pipeline.max_frame_gap_ms, 0);
    }

    #[test]
    fn no_rtcp_without_a_sender() {
        let mut h = harness(false);
        for i in 0..10u64 {
            let (p, t) = p_frame(i as u16, i * 16);
            h.pipeline.push_video_rtp(&p, t);
        }
        h.pipeline.tick(0);
        h.pipeline.tick(2000);
        assert_eq!(*h.sender.keyframe_requests.lock(), 0);
        assert!(h.sender.rembs.lock().is_empty());
        assert_eq!(recovery_events(&h.events), 0, "recovery follows the PLI");
    }

    #[test]
    fn audio_rtp_is_decoded_to_pcm() {
        let mut h = harness(false);
        // A minimal valid Opus packet: TOC for a 20 ms SILK-only mono frame plus a
        // one-byte payload. libopus decodes it (to near-silence) without error.
        let opus = [0x08u8, 0x00];
        for seq in 0..3u16 {
            h.pipeline
                .push_audio_rtp(&rtp_packet(seq, u32::from(seq) * 960, false, &opus), 0);
        }
        let pcm = h.pcm.lock().clone();
        assert_eq!(pcm.len(), 3, "one callback per push");
        for (samples, rate, channels) in pcm {
            assert_eq!((rate, channels), (48_000, 2));
            assert_eq!(samples, 960 * 2, "20 ms at 48 kHz stereo");
        }
        // Padding-only and malformed packets are ignored.
        h.pipeline
            .push_audio_rtp(&rtp_packet(3, 3 * 960, false, &[]), 0);
        h.pipeline.push_audio_rtp(&[0x80, 0x60, 0x00], 0);
        assert_eq!(h.pcm.lock().len(), 3);
    }

    #[test]
    fn audio_jitter_releases_in_sequence_order() {
        let mut h = harness(false);
        let opus = [0x08u8, 0x00];
        h.pipeline
            .push_audio_rtp(&rtp_packet(10, 0, false, &opus), 0);
        // seq 12 arrives before 11: held, not decoded yet.
        h.pipeline
            .push_audio_rtp(&rtp_packet(12, 0, false, &opus), 0);
        assert_eq!(h.pcm.lock().len(), 1);
        h.pipeline
            .push_audio_rtp(&rtp_packet(11, 0, false, &opus), 0);
        let pcm = h.pcm.lock().clone();
        assert_eq!(pcm.len(), 2);
        assert_eq!(pcm[1].0, 2 * 960 * 2, "11 and 12 decoded in one drain");
        assert_eq!(h.pipeline.audio_packets_lost(), 0);
    }

    #[test]
    fn due_mirrors_the_cpp_minus_one_sentinel() {
        assert!(!due(None, 998, 1000));
        assert!(due(None, 999, 1000));
        assert!(!due(Some(500), 1499, 1000));
        assert!(due(Some(500), 1500, 1000));
    }
}
