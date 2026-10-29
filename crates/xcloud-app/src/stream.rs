//! Port of qt/src/app/StreamController.{h,cpp} (the host side: bridge wiring, native
//! frame and audio plumbing, `setOutput`), qt/src/app/VideoFrameProvider.{h,cpp} (the
//! frame hand-off to the view), qt/src/app/AudioOutput.cpp (its use by the controller),
//! qt/src/app/GamepadInputSource.cpp (the 8 ms `QTimer`) and the renderer hand-off of
//! qt/src/app/MetalVideoItem.mm, onto the [`crate::state`] contract.
//!
//! The controller logic itself (`startCloudStream`, `stopStreaming`, `refreshPreferences`,
//! the lifecycle text) is `xcloud_stream::orchestrator::StreamController`; this module is
//! what the Qt `StreamController` constructor wired around it and what QML pulled from it:
//!
//! * One [`FrameMailbox`] for the app lifetime. The reader is the [`VideoSource`] the
//!   stream view locks once per frame (the `VideoFrameProvider` / `MetalVideoItem` input);
//!   the writer sits behind a [`DecodeThread`] whose frame callback bumps
//!   `videoFrameSerial` and requests a repaint (the `nativePixelBuffer` queued connection).
//! * A [`NativeMediaPipeline`] installed as the transport's media sink, its audio sink
//!   feeding an [`AudioOutput`] (`nativeAudioFrame` -> `AudioOutput::push`).
//! * A 125 Hz input thread owning the [`GamepadInputSource`] (`updatePolling`'s timer),
//!   active only while the controller is `Connected`, that also plays server rumble on
//!   the physical controller scaled by `AppSettings::vibration_intensity`.
//! * A host thread that drains [`StreamCommand`]s and the orchestrator / pipeline events
//!   (the QML frame hop) and mirrors the controller into the [`StreamSnapshot`].
//!
//! Differences from the C++: `videoFrameSerial` restarts at 0 when a stream ends (the
//! snapshot documents "0 until the first frame" per stream); the C++ kept counting.
//! `gpuVideo` is derived from the frame type (`CvPixelBuffer`) rather than from which
//! bridge signal fired.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use parking_lot::{Mutex, RwLock};
use tokio::runtime::Handle as RuntimeHandle;
use xcloud_api::auth::MicrosoftAuthService;
use xcloud_api::client::XCloudApiClient;
use xcloud_input::source::{
    GamepadInputSource, POLL_INTERVAL_MS, Sampler, TriggerSide as InputTriggerSide,
};
use xcloud_media::{AudioOutput, Frame, FrameMailbox, FrameSink, VideoDecoder};
use xcloud_proto::input::models::ControllerSettings;
use xcloud_proto::input::packet::VibrationReport;
use xcloud_proto::preferences::stream_resolution_to_string;
use xcloud_proto::streaming_config::StreamingConfig;
use xcloud_proto::streaming_types::StreamingStatsSnapshot;
use xcloud_stream::decode_thread::{DecodeFeedback, DecodeThread, FrameCallback, SubmitOutcome};
use xcloud_stream::orchestrator::{
    OrchestratorEvent, RuntimeSignal, SharedInputQueue, StreamController, StreamSettings,
};
use xcloud_stream::pipeline::{AuSink, AudioSink, DecodeStats, NativeMediaPipeline, PipelineEvent};
use xcloud_webrtc::ConfigurableMediaSink;
use xcloud_webrtc::native::WebRtcTransport;
use xcloud_webrtc::{MediaSink, Transport, now_ms};

use crate::services::resolution_from_profile;
use crate::state::{
    AppSettings, Handle, Repaint, StreamCommand, StreamHandle, StreamSnapshot, TriggerSide,
    VideoSource,
};

const LOG: &str = "StreamController";
#[cfg(all(not(target_os = "macos"), not(feature = "ffmpeg")))]
const VIDEO_LOG: &str = "Video";
const CONTROLLER_LOG: &str = "Controller";

/// How often the host thread drains controller and pipeline events when no command
/// arrives (the QML frame hop; the CLI uses the same cadence).
pub const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Capacity of the pipeline event channel; the pipeline sends with `try_send`.
const PIPELINE_EVENT_CAPACITY: usize = 256;
/// Cadence of the decode-stats publish from the pump thread (the C++ `kBitrateWindowMs`).
const PUBLISH_INTERVAL_MS: u64 = 1000;

// ----------------------------------------------------------------------------- factories

/// Builds the video decoder on the decode thread, given the sink frames must go to.
pub type DecoderFactory = Box<dyn FnOnce(FrameSink) -> Box<dyn VideoDecoder> + Send>;

/// The platform decoder: VideoToolbox on macOS (`OPENXCLOUD_SOFTWARE_VIDEO` honoured), the
/// FFmpeg elsewhere when enabled (otherwise a counting null decoder).
pub fn default_decoder_factory() -> DecoderFactory {
    // `OPENXCLOUD_SOFTWARE_VIDEO=ffmpeg` selects the portable backend when it is compiled in.
    #[cfg(feature = "ffmpeg")]
    if ffmpeg_requested() {
        return ffmpeg_decoder_factory();
    }
    #[cfg(all(target_os = "macos", feature = "ffmpeg"))]
    {
        // VideoToolbox first; when it cannot decode (no session, repeated invalid
        // sessions, keyframes without output) FFmpeg software decoding takes over for the
        // rest of the stream (docs/archive/PORTING_PLAN.md 5.2, `xcloud_media::fallback`). Software,
        // not the FFmpeg VideoToolbox hwaccel: that is the decoder that just failed.
        Box::new(|sink| {
            Box::new(xcloud_media::FallbackDecoder::with_config(
                xcloud_media::FallbackConfig::default().with_names("VideoToolbox", "FFmpeg"),
                sink,
                |sink| Box::new(xcloud_media::vt::VtDecoder::with_defaults(sink)),
                |sink| {
                    let config = xcloud_media::ffmpeg::FfmpegConfig {
                        hwaccel: xcloud_media::ffmpeg::Hwaccel::None,
                        ..xcloud_media::ffmpeg::FfmpegConfig::default()
                    };
                    Box::new(xcloud_media::ffmpeg::FfmpegDecoder::new(config, sink))
                },
            ))
        })
    }
    #[cfg(all(target_os = "macos", not(feature = "ffmpeg")))]
    {
        Box::new(|sink| Box::new(xcloud_media::vt::VtDecoder::with_defaults(sink)))
    }
    #[cfg(all(not(target_os = "macos"), feature = "ffmpeg"))]
    {
        ffmpeg_decoder_factory()
    }
    #[cfg(all(not(target_os = "macos"), not(feature = "ffmpeg")))]
    {
        Box::new(|sink| {
            drop(sink);
            tracing::warn!(target: VIDEO_LOG, "no video decoder on this platform (build with --features ffmpeg); access units are discarded");
            Box::new(NullDecoder)
        })
    }
}

#[cfg(feature = "ffmpeg")]
fn ffmpeg_requested() -> bool {
    std::env::var(xcloud_media::ffmpeg::SOFTWARE_VIDEO_ENV)
        .is_ok_and(|value| value.trim().eq_ignore_ascii_case("ffmpeg"))
}

#[cfg(feature = "ffmpeg")]
fn ffmpeg_decoder_factory() -> DecoderFactory {
    Box::new(|sink| Box::new(xcloud_media::ffmpeg::FfmpegDecoder::with_defaults(sink)))
}

/// Accepts every access unit and produces no frames (platforms without a decoder yet).
#[cfg(all(not(target_os = "macos"), not(feature = "ffmpeg")))]
struct NullDecoder;

#[cfg(all(not(target_os = "macos"), not(feature = "ffmpeg")))]
impl VideoDecoder for NullDecoder {
    fn decode(&mut self, _au: &[u8], _t_received: Instant) -> xcloud_media::Result<()> {
        Ok(())
    }

    fn flush(&mut self) {}
}

/// Plays one server vibration report at the given intensity (`0.0..=1.0`); returns
/// whether a controller took it.
pub type RumbleSink = Box<dyn FnMut(&VibrationReport, f32) -> bool + Send>;

/// The physical controller as the input thread uses it: a sampler for
/// [`GamepadInputSource`] and the rumble path (`NativeGamepad::set_vibration`).
pub struct GamepadDevice {
    pub sampler: Sampler,
    pub rumble: RumbleSink,
}

/// Builds the [`GamepadDevice`] on the input thread (gilrs wants to be created on the
/// thread that polls it).
pub type GamepadFactory = Box<dyn FnOnce() -> GamepadDevice + Send>;

/// The gilrs-backed controller with rumble; the "no controller" stub on iOS.
pub fn native_gamepad_factory() -> GamepadFactory {
    Box::new(|| {
        #[cfg(not(target_os = "ios"))]
        {
            let pad = Arc::new(Mutex::new(xcloud_input::native::NativeGamepad::new()));
            let sampler_pad = Arc::clone(&pad);
            GamepadDevice {
                sampler: Box::new(move || sampler_pad.lock().sample()),
                rumble: Box::new(move |report, intensity| {
                    match pad.lock().set_vibration(report, intensity) {
                        Ok(played) => played,
                        Err(error) => {
                            tracing::warn!(target: CONTROLLER_LOG, "rumble failed: {error}");
                            false
                        }
                    }
                }),
            }
        }
        #[cfg(target_os = "ios")]
        {
            keyboard_only_device()
        }
    })
}

/// No physical controller: the keyboard virtual controller only, rumble ignored.
pub fn keyboard_only_device() -> GamepadDevice {
    GamepadDevice {
        sampler: Box::new(xcloud_input::native::sample_stub),
        rumble: Box::new(|_, _| false),
    }
}

/// The WebRTC bridge the host wires its media sink into. The webrtc transport takes a
/// [`ConfigurableMediaSink`] so the pipeline sees the `StreamingConfig` of every offer
/// (`pipeline->configure(config)` in the C++ bridge); any other transport gets the plain
/// sink configured with the defaults.
pub enum HostTransport {
    WebRtc(Arc<WebRtcTransport>),
    Generic(Arc<dyn Transport>),
}

impl HostTransport {
    fn as_dyn(&self) -> Arc<dyn Transport> {
        match self {
            HostTransport::WebRtc(transport) => Arc::clone(transport) as Arc<dyn Transport>,
            HostTransport::Generic(transport) => Arc::clone(transport),
        }
    }

    fn install_sink(&self, mut sink: AppPipeline) {
        match self {
            HostTransport::WebRtc(transport) => {
                transport.set_configurable_media_sink(Box::new(sink));
            }
            HostTransport::Generic(transport) => {
                sink.configure(&StreamingConfig::defaults());
                transport.set_media_sink(Box::new(sink));
            }
        }
    }
}

/// Everything the Qt `StreamController` constructor received or created.
pub struct StreamHostConfig {
    pub transport: HostTransport,
    pub auth: Option<Arc<MicrosoftAuthService>>,
    pub api: Option<Arc<XCloudApiClient>>,
    /// The shared persisted settings (`Settings*`): read at start-up and on
    /// `ApplySettings`, written through by `SetHudEnabled` like the C++ `setHudEnabled`
    /// wrote `hud/enabled`. Same `Arc` as `Controllers::settings`.
    pub settings: Arc<RwLock<AppSettings>>,
    /// `egui::Context::request_repaint`.
    pub repaint: Repaint,
    pub decoder: DecoderFactory,
    /// `AudioOutput::new()` for the default device; a mock backend in tests.
    pub audio: AudioOutput,
    pub gamepad: GamepadFactory,
}

// ----------------------------------------------------------------------------- settings

/// `Settings` -> `StreamSettings`, as `StreamController::refreshPreferences` read them.
/// Accepts both the Qt profile spelling (`p1080`) and the UI one (`1080p`).
pub fn stream_settings(settings: &AppSettings) -> StreamSettings {
    StreamSettings {
        resolution_profile: stream_resolution_to_string(resolution_from_profile(
            &settings.resolution_profile,
        ))
        .to_owned(),
        locale: settings.locale.clone(),
        prefer_ipv6: settings.prefer_ipv6,
        preferred_region: settings.preferred_region.clone(),
        preferred_frame_rate: u32::try_from(settings.preferred_frame_rate)
            .unwrap_or(60)
            .max(1),
        hud_enabled: settings.hud_enabled,
    }
}

/// `Settings` -> the `GamepadInputSource` setters of `refreshPreferences` plus the
/// vibration scale the input thread applies to rumble.
pub fn controller_settings(settings: &AppSettings) -> ControllerSettings {
    ControllerSettings {
        deadzone: settings.deadzone as f32,
        invert_y: settings.invert_y,
        swap_ab: settings.swap_ab,
        trigger_sensitivity: settings.trigger_sensitivity as f32,
        vibration_intensity: settings.vibration_intensity.clamp(0.0, 1.0) as f32,
        ..ControllerSettings::default()
    }
}

fn input_trigger_side(side: TriggerSide) -> InputTriggerSide {
    match side {
        TriggerSide::Left => InputTriggerSide::Left,
        TriggerSide::Right => InputTriggerSide::Right,
    }
}

// ----------------------------------------------------------------------------- frames

/// What the decode thread's output callback records about frames, read by the host.
#[derive(Default)]
struct FrameMeta {
    /// `videoFrameSerial`.
    serial: AtomicU64,
    /// `gpuVideo`: the latest frame is IOSurface-backed.
    gpu: AtomicBool,
}

impl FrameMeta {
    /// Runs on the decoder's output thread before the frame enters the mailbox.
    fn note(&self, frame: &Frame) {
        let gpu = is_gpu_frame(frame);
        self.gpu.store(gpu, Ordering::Relaxed);
        if self.serial.load(Ordering::Relaxed) == 0 {
            tracing::info!(
                target: LOG,
                "First {} frame: {}x{}",
                if gpu { "VideoToolbox" } else { "decoded video" },
                frame.width,
                frame.height
            );
        }
    }

    fn reset(&self) {
        self.serial.store(0, Ordering::Relaxed);
        self.gpu.store(false, Ordering::Relaxed);
    }
}

fn is_gpu_frame(frame: &Frame) -> bool {
    #[cfg(target_os = "macos")]
    {
        matches!(frame.buf, xcloud_media::NativeBuffer::CvPixelBuffer(_))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = frame;
        false
    }
}

/// Wraps the decode thread's sink so the host learns the frame type before the mailbox
/// publish (and the frame callback) run.
fn observe_frames(mut sink: FrameSink, frames: Arc<FrameMeta>) -> FrameSink {
    Box::new(move |frame| {
        frames.note(&frame);
        sink(frame);
    })
}

/// `DecodeThread::spawn` wants a concrete `VideoDecoder`; this is the boxed one.
struct BoxedDecoder(Box<dyn VideoDecoder>);

impl VideoDecoder for BoxedDecoder {
    fn decode(&mut self, au: &[u8], t_received: Instant) -> xcloud_media::Result<()> {
        self.0.decode(au, t_received)
    }

    fn flush(&mut self) {
        self.0.flush();
    }
}

/// `AuSink` over a shared `DecodeThread`, so the host keeps a handle (`reset` on stop)
/// while the pipeline owns the sink.
struct DecoderHandle(Arc<DecodeThread>);

impl AuSink for DecoderHandle {
    fn submit(&mut self, au: &[u8], _now_ms: u64) -> SubmitOutcome {
        self.0.submit(au, Instant::now())
    }

    fn take_feedback(&mut self) -> DecodeFeedback {
        self.0.take_feedback()
    }

    fn reset(&mut self) {
        self.0.reset();
    }
}

// ----------------------------------------------------------------------------- pipeline

/// Decode-side counters published once per second from the pump thread for the HUD
/// (`decode_ms_avg` / `decode_ms_max`); the transport's `collect_stats` does not carry
/// them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PipelineReport {
    /// Bumped on every publish.
    pub generation: u64,
    pub decode: DecodeStats,
    pub video: xcloud_proto::video_jitter::Stats,
    pub waiting_keyframe: bool,
}

type SharedPipelineReport = Arc<Mutex<PipelineReport>>;

/// `NativeMediaPipeline` as the transport's sink, publishing a [`PipelineReport`].
struct AppPipeline {
    pipeline: NativeMediaPipeline,
    report: SharedPipelineReport,
    last_publish_ms: Option<u64>,
    generation: u64,
}

impl AppPipeline {
    fn new(pipeline: NativeMediaPipeline, report: SharedPipelineReport) -> Self {
        Self {
            pipeline,
            report,
            last_publish_ms: None,
            generation: 0,
        }
    }

    fn publish(&mut self, now_ms: u64) {
        self.generation += 1;
        self.last_publish_ms = Some(now_ms);
        *self.report.lock() = PipelineReport {
            generation: self.generation,
            decode: self.pipeline.decode_stats(),
            video: self.pipeline.video_stats(),
            waiting_keyframe: self.pipeline.waiting_keyframe(),
        };
    }
}

impl MediaSink for AppPipeline {
    fn on_video_rtp(&mut self, packet: &[u8], now_ms: u64) {
        self.pipeline.push_video_rtp(packet, now_ms);
    }

    fn on_audio_rtp(&mut self, packet: &[u8], now_ms: u64) {
        self.pipeline.push_audio_rtp(packet, now_ms);
    }

    fn tick(&mut self, now_ms: u64) {
        self.pipeline.tick(now_ms);
        let due = self
            .last_publish_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= PUBLISH_INTERVAL_MS);
        if due {
            self.publish(now_ms);
        }
    }

    fn reset(&mut self) {
        self.pipeline.reset();
        self.last_publish_ms = None;
        *self.report.lock() = PipelineReport::default();
    }

    fn stats(&self) -> StreamingStatsSnapshot {
        self.pipeline.stats()
    }
}

impl ConfigurableMediaSink for AppPipeline {
    fn configure(&mut self, config: &StreamingConfig) {
        self.pipeline.configure(config);
    }
}

// ----------------------------------------------------------------------------- input thread

/// What the host forwards to the input thread (`GamepadInputSource`'s invokables plus
/// the settings and the rumble reports).
enum InputMsg {
    Key {
        name: String,
        pressed: bool,
    },
    LeftStick {
        x: f64,
        y: f64,
    },
    RightStick {
        x: f64,
        y: f64,
    },
    Trigger {
        side: InputTriggerSide,
        value: f64,
    },
    ReleaseAll,
    Focus(bool),
    Settings(ControllerSettings),
    Vibration(VibrationReport),
    #[cfg(test)]
    Probe(Sender<InputProbe>),
}

/// Test window into the input thread's `GamepadInputSource`.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
struct InputProbe {
    active: bool,
    focused: bool,
    pressed: Vec<&'static str>,
    connected: bool,
    controller_name: String,
    settings: ControllerSettings,
    vibration_intensity: f32,
}

/// `GamepadInputSource` + its 8 ms `QTimer`, on a dedicated thread. `active` follows the
/// controller's `input_active()` (polling only while `Connected`).
struct InputThread {
    tx: Option<Sender<InputMsg>>,
    active: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl InputThread {
    fn spawn(
        queue: SharedInputQueue,
        gamepad: GamepadFactory,
        settings: ControllerSettings,
        snapshot: Arc<RwLock<StreamSnapshot>>,
        repaint: Repaint,
    ) -> anyhow::Result<Self> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let active = Arc::new(AtomicBool::new(false));
        let thread_active = Arc::clone(&active);
        let handle = std::thread::Builder::new()
            .name("xcloud-input".to_owned())
            .spawn(move || {
                let GamepadDevice { sampler, rumble } = gamepad();
                let mut source = GamepadInputSource::new(sampler);
                source.set_input_queue(Some(queue));
                source.set_settings(settings);
                let worker = InputWorker {
                    source,
                    rumble,
                    vibration_intensity: settings.vibration_intensity,
                    snapshot,
                    repaint,
                    active: thread_active,
                };
                input_loop(worker, &rx);
            })
            .context("cannot spawn the input thread")?;
        Ok(Self {
            tx: Some(tx),
            active,
            handle: Some(handle),
        })
    }

    fn send(&self, message: InputMsg) {
        if let Some(tx) = &self.tx {
            // The thread is gone only after `stop`; nothing to forward to then.
            let _ = tx.send(message);
        }
    }

    fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }

    /// Closes the channel (the thread releases every input and exits) and joins.
    fn stop(&mut self) {
        self.tx = None;
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            tracing::error!(target: CONTROLLER_LOG, "input thread panicked");
        }
    }
}

impl Drop for InputThread {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The input thread's state.
struct InputWorker {
    source: GamepadInputSource,
    rumble: RumbleSink,
    vibration_intensity: f32,
    snapshot: Arc<RwLock<StreamSnapshot>>,
    repaint: Repaint,
    active: Arc<AtomicBool>,
}

impl InputWorker {
    fn handle(&mut self, message: InputMsg) {
        match message {
            InputMsg::Key { name, pressed } => self.source.set_key(&name, pressed),
            InputMsg::LeftStick { x, y } => self.source.set_left_stick(x, y),
            InputMsg::RightStick { x, y } => self.source.set_right_stick(x, y),
            InputMsg::Trigger { side, value } => self.source.set_trigger(side, value),
            InputMsg::ReleaseAll => self.source.release_all(),
            InputMsg::Focus(focused) => self.source.set_input_focused(focused),
            InputMsg::Settings(settings) => {
                self.source.set_settings(settings);
                self.vibration_intensity = settings.vibration_intensity;
            }
            InputMsg::Vibration(report) => {
                if !(self.rumble)(&report, self.vibration_intensity) {
                    tracing::trace!(target: CONTROLLER_LOG, ?report, "vibration report ignored (no rumble-capable controller)");
                }
            }
            #[cfg(test)]
            InputMsg::Probe(reply) => {
                let _ = reply.send(InputProbe {
                    active: self.source.active(),
                    focused: self.source.input_focused(),
                    pressed: self.source.pressed_keys().collect(),
                    connected: self.source.connected(),
                    controller_name: self.source.controller_name().to_owned(),
                    settings: *self.source.settings(),
                    vibration_intensity: self.vibration_intensity,
                });
            }
        }
    }

    /// One timer tick: samples, enqueues, and mirrors `connectedChanged` into the snapshot.
    fn tick(&mut self) {
        self.source.set_active(self.active.load(Ordering::Relaxed));
        let report = self.source.tick(now_ms());
        if report.connection_changed {
            {
                let mut snapshot = self.snapshot.write();
                snapshot.controller_connected = self.source.connected();
                snapshot.controller_name = self.source.controller_name().to_owned();
            }
            (self.repaint)();
        }
    }
}

fn input_loop(mut worker: InputWorker, rx: &Receiver<InputMsg>) {
    let period = Duration::from_millis(POLL_INTERVAL_MS);
    let mut next = Instant::now() + period;
    loop {
        match rx.recv_deadline(next) {
            Ok(message) => worker.handle(message),
            Err(RecvTimeoutError::Timeout) => {
                worker.tick();
                next += period;
                let now = Instant::now();
                if next < now {
                    next = now + period;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // `setActive(false)`: release everything (a neutral frame while the queue is attached).
    worker.source.set_active(false);
}

// ----------------------------------------------------------------------------- host core

/// The host state, owned by the host thread (or driven inline by tests).
struct HostCore {
    runtime: RuntimeHandle,
    controller: StreamController,
    transport: HostTransport,
    /// `m_settings`: the shared persisted settings the HUD toggle writes through to.
    settings: Arc<RwLock<AppSettings>>,
    snapshot: Arc<RwLock<StreamSnapshot>>,
    commands: Receiver<StreamCommand>,
    repaint: Repaint,
    pipeline_events: Receiver<PipelineEvent>,
    pipeline_report: SharedPipelineReport,
    decoder: Arc<DecodeThread>,
    audio: Arc<Mutex<AudioOutput>>,
    frames: Arc<FrameMeta>,
    input: InputThread,
}

impl HostCore {
    /// The Qt `StreamController` constructor. Must run inside a tokio runtime context.
    fn build(
        config: StreamHostConfig,
        snapshot: Arc<RwLock<StreamSnapshot>>,
        commands: Receiver<StreamCommand>,
    ) -> anyhow::Result<(Self, VideoSource)> {
        let StreamHostConfig {
            transport,
            auth,
            api,
            settings: shared_settings,
            repaint,
            decoder,
            audio,
            gamepad,
        } = config;
        let settings = shared_settings.read().clone();
        let runtime = RuntimeHandle::try_current()
            .context("StreamHost must be built inside a tokio runtime")?;
        let transport_dyn = transport.as_dyn();
        let controller = StreamController::new(
            auth,
            api,
            Arc::clone(&transport_dyn),
            stream_settings(&settings),
        )
        .context("creating the stream controller")?;

        // Frames: the mailbox lives as long as the app; the reader is the view's source.
        let (writer, reader) = FrameMailbox::open();
        let frames = Arc::new(FrameMeta::default());
        let on_frame: FrameCallback = {
            let frames = Arc::clone(&frames);
            let snapshot = Arc::clone(&snapshot);
            let repaint = Arc::clone(&repaint);
            Box::new(move || {
                let serial = frames.serial.fetch_add(1, Ordering::Relaxed) + 1;
                {
                    let mut snapshot = snapshot.write();
                    snapshot.video_frame_serial = serial;
                    snapshot.gpu_video = frames.gpu.load(Ordering::Relaxed);
                }
                repaint();
            })
        };
        let decode_thread = Arc::new(DecodeThread::spawn(writer, Some(on_frame), {
            let frames = Arc::clone(&frames);
            move |sink| BoxedDecoder(decoder(observe_frames(sink, frames)))
        }));

        // Audio: `nativeAudioFrame` -> `AudioOutput::push`, from the pump thread.
        let audio = Arc::new(Mutex::new(audio));
        let audio_sink: AudioSink = {
            let audio = Arc::clone(&audio);
            Box::new(move |pcm: &[i16], sample_rate: u32, channels: u32| {
                let channels = u16::try_from(channels).unwrap_or(2);
                audio.lock().push(pcm, sample_rate, channels);
            })
        };

        // The media pipeline, as the transport's sink.
        let (events_tx, pipeline_events) = crossbeam_channel::bounded(PIPELINE_EVENT_CAPACITY);
        let pipeline = NativeMediaPipeline::new(
            transport_dyn.rtcp_sender(),
            DecoderHandle(Arc::clone(&decode_thread)),
            audio_sink,
            events_tx,
        );
        let pipeline_report: SharedPipelineReport = Arc::default();
        transport.install_sink(AppPipeline::new(pipeline, Arc::clone(&pipeline_report)));

        // The gamepad source on its 125 Hz thread, enqueuing into the orchestrator's queue.
        let input = InputThread::spawn(
            controller.input_queue(),
            gamepad,
            controller_settings(&settings),
            Arc::clone(&snapshot),
            Arc::clone(&repaint),
        )?;

        {
            let mut snapshot = snapshot.write();
            snapshot.hud_enabled = controller.hud_enabled();
            snapshot.native_video = controller.native_video();
            // `GamepadInputSource` starts focused; the view toggles it.
            snapshot.input_focused = true;
        }

        let core = Self {
            runtime,
            controller,
            transport,
            settings: shared_settings,
            snapshot,
            commands,
            repaint,
            pipeline_events,
            pipeline_report,
            decoder: decode_thread,
            audio,
            frames,
            input,
        };
        Ok((core, Arc::new(Mutex::new(reader))))
    }

    /// The host thread body: commands as they come, a poll every [`POLL_INTERVAL`].
    fn run(mut self, stop: &Receiver<()>) -> Self {
        let runtime = self.runtime.clone();
        let _guard = runtime.enter();
        let commands = self.commands.clone();
        loop {
            crossbeam_channel::select! {
                recv(commands) -> command => match command {
                    Ok(command) => self.handle_command(command),
                    // Every `StreamHandle` is gone: the UI has shut down.
                    Err(_) => break,
                },
                recv(stop) -> _ => break,
                default(POLL_INTERVAL) => {}
            }
            self.poll();
        }
        self
    }

    /// One `StreamCommand` (a QML invokable).
    fn handle_command(&mut self, command: StreamCommand) {
        match command {
            StreamCommand::StartCloudStream {
                title_id,
                title_name,
                hero_image,
            } => {
                self.controller
                    .start_cloud_stream(&title_id, &title_name, &hero_image);
            }
            StreamCommand::StopStreaming => self.controller.stop_streaming(),
            StreamCommand::SetHudEnabled(enabled) => self.set_hud_enabled(enabled),
            StreamCommand::ApplySettings(settings) => self.apply_settings(&settings),
            StreamCommand::SetKey { name, pressed } => {
                self.input.send(InputMsg::Key { name, pressed });
                return;
            }
            StreamCommand::SetLeftStick { x, y } => {
                self.input.send(InputMsg::LeftStick { x, y });
                return;
            }
            StreamCommand::SetRightStick { x, y } => {
                self.input.send(InputMsg::RightStick { x, y });
                return;
            }
            StreamCommand::SetTrigger { side, value } => {
                self.input.send(InputMsg::Trigger {
                    side: input_trigger_side(side),
                    value,
                });
                return;
            }
            StreamCommand::ReleaseAll => {
                self.input.send(InputMsg::ReleaseAll);
                return;
            }
            StreamCommand::SetInputFocused(focused) => {
                self.input.send(InputMsg::Focus(focused));
                let changed = {
                    let mut snapshot = self.snapshot.write();
                    let changed = snapshot.input_focused != focused;
                    snapshot.input_focused = focused;
                    changed
                };
                if changed {
                    (self.repaint)();
                }
                return;
            }
            StreamCommand::SetOutput {
                height,
                upscaled,
                upscaler_label,
                gpu_ms,
                frame_age_ms,
                present_ms,
            } => {
                self.set_output(
                    height,
                    upscaled,
                    upscaler_label,
                    gpu_ms,
                    frame_age_ms,
                    present_ms,
                );
                return;
            }
        }
        self.poll();
    }

    /// `setHudEnabled`: writes through to the shared persisted settings (`hud/enabled`)
    /// and the controller, so the toggle survives restarts and later `ApplySettings`.
    fn set_hud_enabled(&mut self, enabled: bool) {
        self.settings.write().hud_enabled = enabled;
        self.controller.set_hud_enabled(enabled);
    }

    /// `applySettings` / `refreshPreferences`: the orchestrator preferences/config and the
    /// input source settings (including the rumble scale). Like the C++ it never touches
    /// the HUD flag: that is owned by `setHudEnabled`, not by the settings panel.
    fn apply_settings(&mut self, settings: &AppSettings) {
        let stream = StreamSettings {
            hud_enabled: self.controller.hud_enabled(),
            ..stream_settings(settings)
        };
        self.controller.set_settings(stream);
        self.controller.apply_settings();
        self.input
            .send(InputMsg::Settings(controller_settings(settings)));
    }

    /// `setOutput(height, scaled)` plus the renderer's upscaler label and per-frame timings.
    fn set_output(
        &self,
        height: u32,
        upscaled: bool,
        upscaler_label: &str,
        gpu_ms: f64,
        frame_age_ms: f64,
        present_ms: f64,
    ) {
        let changed = {
            let mut snapshot = self.snapshot.write();
            let changed = snapshot.output_height != height
                || snapshot.upscaled != upscaled
                || snapshot.upscaler_label != upscaler_label
                || snapshot.gpu_ms != gpu_ms
                || snapshot.frame_age_ms != frame_age_ms
                || snapshot.present_ms != present_ms;
            snapshot.output_height = height;
            snapshot.upscaled = upscaled;
            if snapshot.upscaler_label != upscaler_label {
                snapshot.upscaler_label = upscaler_label.to_owned();
            }
            snapshot.gpu_ms = gpu_ms;
            snapshot.frame_age_ms = frame_age_ms;
            snapshot.present_ms = present_ms;
            changed
        };
        if changed {
            (self.repaint)();
        }
    }

    /// Drains orchestrator and pipeline events and mirrors the controller into the
    /// snapshot. Called after every command and every [`POLL_INTERVAL`].
    fn poll(&mut self) {
        for event in self.controller.poll_events() {
            self.react(&event);
        }
        self.input.set_active(self.controller.input_active());
        if self.controller.take_stream_ended() {
            self.on_stream_ended();
        }
        for event in self.pipeline_events.try_iter() {
            Self::forward_pipeline_event(&self.controller, event);
        }
        self.sync_snapshot();
    }

    /// Delivers one orchestrator event the way the event bridge would (tests).
    #[cfg(test)]
    fn inject_event(&mut self, event: OrchestratorEvent) {
        self.controller.handle_event(event.clone());
        self.react(&event);
        self.poll();
    }

    /// The host's share of `wireOrchestrator`: what the controller does not handle itself.
    fn react(&self, event: &OrchestratorEvent) {
        match event {
            OrchestratorEvent::Vibration(report) => {
                self.input.send(InputMsg::Vibration(*report));
            }
            OrchestratorEvent::VideoTrackAvailable => {
                tracing::info!(target: LOG, "video track available");
            }
            OrchestratorEvent::Lifecycle(_)
            | OrchestratorEvent::WaitingForResources(_)
            | OrchestratorEvent::StatsChanged(_)
            | OrchestratorEvent::Error(_)
            | OrchestratorEvent::ServerInitiatedDisconnect => {}
        }
    }

    /// The pipeline's signals that went to the bridge in the C++ (`videoRecoveryNeeded`,
    /// the decoded size written into the stats) go to the orchestrator as
    /// [`RuntimeSignal`]s.
    fn forward_pipeline_event(controller: &StreamController, event: PipelineEvent) {
        match event {
            PipelineEvent::KeyframeRecoveryNeeded => {
                controller
                    .orchestrator()
                    .signal(RuntimeSignal::VideoRecoveryNeeded);
            }
            PipelineEvent::Resolution { width, height } => {
                controller
                    .orchestrator()
                    .signal(RuntimeSignal::DecodedResolution { width, height });
            }
            // The orchestrator's stats poll merges the transport's `collect_stats`, which
            // carries the pipeline's counters on the native path; the decode timings come
            // through the `PipelineReport`.
            PipelineEvent::StatsChanged(_) => {}
        }
    }

    /// The host half of `stopStreaming`, run after the controller emitted `streamEnded`
    /// (user stop or server-initiated disconnect): `m_audioOutput->stop()`,
    /// `m_frameBuffer->clear()`, `m_nativeFrame = {}`, `setOutput(0, false)`.
    fn on_stream_ended(&mut self) {
        self.audio.lock().stop();
        // Flushes the decoder and clears the mailbox so the view does not keep the last
        // picture of the ended stream.
        self.decoder.reset();
        self.frames.reset();
        let mut snapshot = self.snapshot.write();
        snapshot.video_frame_serial = 0;
        snapshot.gpu_video = false;
        snapshot.output_height = 0;
        snapshot.upscaled = false;
        snapshot.upscaler_label.clear();
        snapshot.gpu_ms = 0.0;
        snapshot.frame_age_ms = 0.0;
        snapshot.present_ms = 0.0;
    }

    /// Mirrors the controller's properties into the snapshot; repaints on a change.
    fn sync_snapshot(&self) {
        let controller = &self.controller;
        let report = self.pipeline_report.lock().clone();
        let changed = {
            let mut snapshot = self.snapshot.write();
            let before = snapshot.clone();
            snapshot.lifecycle = controller.lifecycle_state();
            snapshot.status_text = controller.status_text();
            snapshot.title_id = controller.title_id();
            snapshot.title_name = controller.title_name();
            snapshot.hero_image = controller.hero_image();
            snapshot.error = controller.error();
            snapshot.active = controller.active();
            snapshot.hud_enabled = controller.hud_enabled();
            snapshot.stats = controller.stats();
            snapshot.native_video = controller.native_video();
            snapshot.pipeline_stats_ready = report.generation > 0;
            snapshot.decode_stats = report.decode;
            snapshot.video_stats = report.video;
            snapshot.decode_ms_avg = report.decode.decode.avg_ms;
            snapshot.decode_ms_max = report.decode.decode.max_ms;
            snapshot.jitter_ms_avg = report.decode.jitter.avg_ms;
            snapshot.jitter_ms_max = report.decode.jitter.max_ms;
            *snapshot != before
        };
        if changed {
            (self.repaint)();
        }
    }

    /// App exit: stop the stream, the input thread, the decoder and the audio device,
    /// then release the transport and stop its worker.
    fn teardown(mut self) {
        self.controller.stop_streaming();
        if self.controller.take_stream_ended() {
            self.on_stream_ended();
        }
        self.sync_snapshot();
        self.input.stop();
        let Self {
            controller,
            decoder,
            audio,
            transport,
            ..
        } = self;
        drop(controller);
        drop(decoder);
        audio.lock().stop();
        drop(audio);
        drop(transport);
    }
}

// ----------------------------------------------------------------------------- host

/// The stream host as `services.rs` / `app.rs` own it: the UI side of the contract plus
/// the thread that runs the controller.
pub struct StreamHost {
    handle: StreamHandle,
    video: VideoSource,
    stop: Sender<()>,
    thread: Option<JoinHandle<HostCore>>,
}

impl std::fmt::Debug for StreamHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamHost")
            .field("running", &self.thread.is_some())
            .finish_non_exhaustive()
    }
}

impl StreamHost {
    /// Builds the object graph (inside `runtime`) and starts the host thread.
    pub fn spawn(config: StreamHostConfig, runtime: &RuntimeHandle) -> anyhow::Result<Self> {
        let (handle, snapshot, commands) = Handle::new(StreamSnapshot::default());
        let (core, video) = {
            let _guard = runtime.enter();
            HostCore::build(config, snapshot, commands)?
        };
        let (stop, stop_rx) = crossbeam_channel::bounded(1);
        let thread = std::thread::Builder::new()
            .name("xcloud-stream-host".to_owned())
            .spawn(move || core.run(&stop_rx))
            .context("cannot spawn the stream host thread")?;
        Ok(Self {
            handle,
            video,
            stop,
            thread: Some(thread),
        })
    }

    /// The UI side: snapshot reads and the command inbox.
    pub fn handle(&self) -> StreamHandle {
        self.handle.clone()
    }

    /// Where the stream view takes decoded frames from.
    pub fn video(&self) -> VideoSource {
        Arc::clone(&self.video)
    }

    /// App exit: stops the stream and joins the host thread, then tears the stack down
    /// in order (controller, decoder, audio, transport).
    pub fn shutdown(mut self) {
        let _ = self.stop.try_send(());
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(core) => core.teardown(),
                Err(_) => tracing::error!(target: LOG, "stream host thread panicked"),
            }
        }
    }
}

impl Drop for StreamHost {
    fn drop(&mut self) {
        // Without `shutdown` the thread still exits; the stack is dropped with the core.
        let _ = self.stop.try_send(());
    }
}

#[cfg(test)]
mod tests {
    //! Port of qt/tests/tst_streamcontroller.cpp at the host level. `inputWaitsForHandshake`
    //! and the orchestrator-only half of `decodedResolutionOverridesServerMetadata` live in
    //! `xcloud_stream::orchestrator`; here the same scenario runs through the host's
    //! `RuntimeSignal` forwarding and the snapshot. `pollingFollowsStreamLifecycle` is
    //! `lifecycle_events_reach_the_snapshot` + `input_commands_reach_the_gamepad_source`.
    //! `framesSurviveEngineOwnershipAndTeardown` / `bufferCanBeClearedAfterTransfer` become
    //! `stop_streaming_clears_the_mailbox_and_deactivates` (the mailbox replaces the
    //! `VideoFrameBuffer`).

    use std::sync::atomic::AtomicUsize;

    use xcloud_input::source::NativeGamepadSample;
    use xcloud_media::{AudioBackend, ColorInfo, NativeBuffer, OutputStream, PcmFormat, PcmSource};
    use xcloud_proto::input::gamepad::RawGamepadState;
    use xcloud_proto::input::models::GamepadButtons;
    use xcloud_proto::input::packet::HEADER_SIZE;
    use xcloud_proto::preferences::StreamKind;
    use xcloud_proto::streaming_types::{DataChannelKind, StreamLifecycleState};
    use xcloud_webrtc::TransportEvent;
    use xcloud_webrtc::fake::FakeTransport;

    use super::*;

    /// Records every stream the "device" opened.
    #[derive(Clone, Default)]
    struct MockBackend {
        opened: Arc<Mutex<Vec<PcmFormat>>>,
    }

    struct MockStream(PcmFormat);

    impl OutputStream for MockStream {
        fn format(&self) -> PcmFormat {
            self.0
        }
    }

    impl AudioBackend for MockBackend {
        fn open(
            &mut self,
            wanted: PcmFormat,
            source: PcmSource,
        ) -> xcloud_media::Result<Box<dyn OutputStream>> {
            drop(source);
            self.opened.lock().push(wanted);
            Ok(Box::new(MockStream(wanted)))
        }
    }

    /// Turns a two-byte access unit `[w/16, h/16]` into an NV12 frame of that size.
    struct FakeDecoder {
        sink: FrameSink,
        flushes: Arc<AtomicUsize>,
    }

    impl VideoDecoder for FakeDecoder {
        fn decode(&mut self, au: &[u8], t_received: Instant) -> xcloud_media::Result<()> {
            let width = u32::from(au[0]) * 16;
            let height = u32::from(au[1]) * 16;
            let now = Instant::now();
            (self.sink)(Frame {
                buf: NativeBuffer::Nv12Cpu {
                    y: vec![0; (width * height) as usize],
                    uv: vec![0; (width * height / 2) as usize],
                    y_stride: width as usize,
                    uv_stride: width as usize,
                },
                width,
                height,
                color: ColorInfo::default(),
                t_received,
                t_submitted: now,
                t_decoded: now,
            });
            Ok(())
        }

        fn flush(&mut self) {
            self.flushes.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The scripted physical controller plus every rumble it received.
    #[derive(Default)]
    struct Script {
        sample: NativeGamepadSample,
        samples: usize,
        rumbles: Vec<(VibrationReport, f32)>,
    }

    fn scripted_device(script: &Arc<Mutex<Script>>) -> GamepadFactory {
        let sampler_script = Arc::clone(script);
        let rumble_script = Arc::clone(script);
        Box::new(move || GamepadDevice {
            sampler: Box::new(move || {
                let mut script = sampler_script.lock();
                script.samples += 1;
                script.sample.clone()
            }),
            rumble: Box::new(move |report, intensity| {
                rumble_script.lock().rumbles.push((*report, intensity));
                true
            }),
        })
    }

    struct Harness {
        core: HostCore,
        handle: StreamHandle,
        video: VideoSource,
        /// The shared `AppSettings` the host writes the HUD flag through to.
        settings: Arc<RwLock<AppSettings>>,
        fake: Arc<FakeTransport>,
        script: Arc<Mutex<Script>>,
        flushes: Arc<AtomicUsize>,
        repaints: Arc<AtomicUsize>,
        audio_opened: Arc<Mutex<Vec<PcmFormat>>>,
        // Dropped last: the controller's tasks live on it.
        _runtime: tokio::runtime::Runtime,
    }

    fn harness() -> Harness {
        harness_with(AppSettings::default())
    }

    fn harness_with(settings: AppSettings) -> Harness {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let fake = Arc::new(FakeTransport::new());
        let script = Arc::new(Mutex::new(Script::default()));
        let flushes = Arc::new(AtomicUsize::new(0));
        let repaints = Arc::new(AtomicUsize::new(0));
        let backend = MockBackend::default();
        let audio_opened = Arc::clone(&backend.opened);
        let decoder_flushes = Arc::clone(&flushes);
        let repaint_counter = Arc::clone(&repaints);
        let settings = Arc::new(RwLock::new(settings));
        let config = StreamHostConfig {
            transport: HostTransport::Generic(Arc::clone(&fake) as Arc<dyn Transport>),
            auth: None,
            api: None,
            settings: Arc::clone(&settings),
            repaint: Arc::new(move || {
                repaint_counter.fetch_add(1, Ordering::Relaxed);
            }),
            decoder: Box::new(move |sink| {
                Box::new(FakeDecoder {
                    sink,
                    flushes: decoder_flushes,
                })
            }),
            audio: AudioOutput::with_backend(Box::new(backend)),
            gamepad: scripted_device(&script),
        };
        let (handle, snapshot, commands) = Handle::new(StreamSnapshot::default());
        let (core, video) = {
            let _guard = runtime.enter();
            HostCore::build(config, snapshot, commands).expect("host")
        };
        Harness {
            core,
            handle,
            video,
            settings,
            fake,
            script,
            flushes,
            repaints,
            audio_opened,
            _runtime: runtime,
        }
    }

    impl Harness {
        fn probe(&self) -> InputProbe {
            let (tx, rx) = crossbeam_channel::bounded(1);
            self.core.input.send(InputMsg::Probe(tx));
            rx.recv_timeout(Duration::from_secs(5))
                .expect("the input thread answers")
        }

        fn start(&mut self) {
            self.core.handle_command(StreamCommand::StartCloudStream {
                title_id: "title-1".into(),
                title_name: "Halo".into(),
                hero_image: "hero.png".into(),
            });
        }
    }

    /// Polls `condition` for up to five seconds of real time.
    fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
        for _ in 0..500 {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        condition()
    }

    #[test]
    fn settings_map_to_stream_and_controller_settings() {
        let settings = AppSettings {
            resolution_profile: "1440p".into(),
            preferred_frame_rate: 0,
            preferred_region: "WestEurope".into(),
            prefer_ipv6: true,
            hud_enabled: true,
            deadzone: 0.3,
            invert_y: true,
            swap_ab: true,
            trigger_sensitivity: 0.75,
            vibration_intensity: 1.5,
            ..AppSettings::default()
        };
        let stream = stream_settings(&settings);
        assert_eq!(stream.resolution_profile, "p1440");
        assert_eq!(stream.preferred_frame_rate, 1, "clamped to at least 1");
        assert_eq!(stream.preferred_region, "WestEurope");
        assert!(stream.prefer_ipv6);
        assert!(stream.hud_enabled);
        assert_eq!(
            stream_settings(&AppSettings {
                resolution_profile: "p1080HQ".into(),
                ..AppSettings::default()
            })
            .resolution_profile,
            "p1080HQ"
        );
        // `Settings::resolutionProfile()` defaults to "p1440" (core/Settings.cpp).
        assert_eq!(
            stream_settings(&AppSettings::default()).resolution_profile,
            "p1440"
        );
        // `Settings::triggerSensitivity()` defaults to 0.5 (core/Settings.cpp).
        assert_eq!(
            controller_settings(&AppSettings::default()).trigger_sensitivity,
            0.5
        );
        assert_eq!(controller_settings(&AppSettings::default()).deadzone, 0.15);

        let controller = controller_settings(&settings);
        assert_eq!(controller.deadzone, 0.3);
        assert!(controller.invert_y);
        assert!(controller.swap_ab);
        assert_eq!(controller.trigger_sensitivity, 0.75);
        assert_eq!(controller.vibration_intensity, 1.0, "clamped to 0..=1");
    }

    /// `pollingFollowsStreamLifecycle` and the lifecycle/status/error properties: every
    /// controller change the orchestrator signals lands in the snapshot with a repaint.
    #[test]
    fn lifecycle_events_reach_the_snapshot() {
        let mut h = harness();
        let initial = h.handle.read();
        assert_eq!(initial.lifecycle, StreamLifecycleState::Idle);
        assert!(!initial.active);
        assert!(
            !initial.native_video,
            "the fake transport has no RTCP sender"
        );
        assert!(initial.input_focused);
        assert!(!h.probe().active);

        // Without an auth service `startCloudStream` fails synchronously; the controller
        // stays active (only `stopStreaming` clears it), as in the C++.
        h.start();
        let s = h.handle.read();
        assert!(s.active);
        assert_eq!(s.title_id, "title-1");
        assert_eq!(s.title_name, "Halo");
        assert_eq!(s.hero_image, "hero.png");
        assert_eq!(s.lifecycle, StreamLifecycleState::Failed);
        assert_eq!(s.error, "Authentication service unavailable");
        assert_eq!(
            s.status_text,
            "Stream failed: Authentication service unavailable"
        );
        assert!(h.repaints.load(Ordering::Relaxed) > 0);

        h.core.inject_event(OrchestratorEvent::Lifecycle(
            StreamLifecycleState::Connected,
        ));
        let s = h.handle.read();
        assert!(s.connected());
        assert_eq!(s.status_text, "Waiting for video");
        assert!(wait_for(|| h.probe().active), "polling starts on Connected");

        h.core.inject_event(OrchestratorEvent::Lifecycle(
            StreamLifecycleState::WaitingForResources,
        ));
        h.core
            .inject_event(OrchestratorEvent::WaitingForResources(120));
        assert_eq!(h.handle.read().status_text, "In queue, about 2 min");
        assert!(
            wait_for(|| !h.probe().active),
            "polling stops when not connected"
        );

        let repaints = h.repaints.load(Ordering::Relaxed);
        h.core
            .inject_event(OrchestratorEvent::StatsChanged(StreamingStatsSnapshot {
                negotiated_width: 2560,
                negotiated_height: 1440,
                bitrate_kbps: 12_500,
                ..StreamingStatsSnapshot::default()
            }));
        let s = h.handle.read();
        assert_eq!(s.resolution_width(), 2560);
        assert_eq!(s.resolution_height(), 1440);
        assert_eq!(s.bitrate_mbps(), 12.5);
        assert!(h.repaints.load(Ordering::Relaxed) > repaints);

        // A poll without changes does not repaint.
        let repaints = h.repaints.load(Ordering::Relaxed);
        h.core.poll();
        assert_eq!(h.repaints.load(Ordering::Relaxed), repaints);

        // The server's disconnect runs `stopStreaming` (controller) and the host teardown.
        h.core
            .inject_event(OrchestratorEvent::ServerInitiatedDisconnect);
        let s = h.handle.read();
        assert!(!s.active);
        assert_eq!(s.lifecycle, StreamLifecycleState::Disconnected);
        assert_eq!(s.status_text, "Disconnected");
        assert_eq!(h.fake.close_count(), 1, "the bridge was closed");
    }

    /// `framesSurviveEngineOwnershipAndTeardown` / `bufferCanBeClearedAfterTransfer`:
    /// decoded frames reach the view through the mailbox with a serial bump and a
    /// repaint; `stopStreaming` clears the mailbox, stops audio and leaves `active` false
    /// after `streamEnded`.
    #[test]
    fn stop_streaming_clears_the_mailbox_and_deactivates() {
        let mut h = harness();
        h.start();
        assert!(h.handle.read().active);

        let repaints = h.repaints.load(Ordering::Relaxed);
        assert_eq!(
            h.core.decoder.submit(&[4, 2], Instant::now()),
            SubmitOutcome::Accepted
        );
        assert!(wait_for(|| h.handle.read().video_frame_serial == 1));
        assert!(
            h.repaints.load(Ordering::Relaxed) > repaints,
            "every frame repaints"
        );
        assert!(!h.handle.read().gpu_video, "CPU frames are not gpuVideo");
        let frame = h
            .video
            .lock()
            .take_latest()
            .expect("the frame reached the view");
        assert_eq!((frame.width, frame.height), (64, 32));
        assert!(!h.video.lock().has_new());

        assert_eq!(
            h.core.decoder.submit(&[4, 2], Instant::now()),
            SubmitOutcome::Accepted
        );
        assert!(wait_for(|| h.handle.read().video_frame_serial == 2));

        // Audio pushed from the pump thread opened the device.
        h.core.audio.lock().push(&[0; 1920], 48_000, 2);
        assert!(wait_for(|| !h.audio_opened.lock().is_empty()));
        assert_eq!(h.audio_opened.lock()[0], PcmFormat::new(48_000, 2));

        h.core.set_output(1440, true, "FX", 0.7, 12.0, 19.5);
        let s = h.handle.read();
        assert_eq!((s.output_height, s.upscaled), (1440, true));
        assert_eq!(s.upscaler_label, "FX");
        assert_eq!((s.gpu_ms, s.frame_age_ms, s.present_ms), (0.7, 12.0, 19.5));

        h.core.handle_command(StreamCommand::StopStreaming);
        let s = h.handle.read();
        assert!(!s.active, "active is cleared after streamEnded");
        assert_eq!(s.lifecycle, StreamLifecycleState::Disconnected);
        assert_eq!(s.status_text, "Disconnected");
        assert_eq!(s.video_frame_serial, 0);
        assert!(!s.gpu_video);
        assert_eq!((s.output_height, s.upscaled), (0, false));
        assert!(s.upscaler_label.is_empty(), "label cleared with the output");
        assert_eq!((s.gpu_ms, s.frame_age_ms, s.present_ms), (0.0, 0.0, 0.0));
        assert!(
            h.core.audio.lock().format().is_none(),
            "audio output stopped"
        );
        assert_eq!(h.fake.close_count(), 1);

        // The decode thread flushed and published the clear: the pending frame is gone.
        assert!(wait_for(|| h.flushes.load(Ordering::Relaxed) >= 1));
        assert!(wait_for(|| h.video.lock().has_new()));
        assert!(h.video.lock().take_latest().is_none());

        // `stopStreaming` while inactive is a no-op.
        h.core.handle_command(StreamCommand::StopStreaming);
        assert_eq!(h.fake.close_count(), 1);
        assert!(!h.core.controller.take_stream_ended());

        // The next frame counts from 1 again.
        assert_eq!(
            h.core.decoder.submit(&[1, 1], Instant::now()),
            SubmitOutcome::Accepted
        );
        assert!(wait_for(|| h.handle.read().video_frame_serial == 1));
    }

    /// `decodedResolutionOverridesServerMetadata` through the host: the decode thread's
    /// size goes to the orchestrator as `RuntimeSignal::DecodedResolution`, is folded in
    /// by the stats poll and reaches the snapshot via `StatsChanged`.
    #[test]
    fn decoded_resolution_overrides_server_metadata() {
        let mut h = harness();
        let orchestrator = Arc::clone(h.core.controller.orchestrator());
        orchestrator.connect_stream(StreamKind::Cloud, "test", "");
        // ServerMetadata: type=16, height=1080, width=1920, little endian.
        orchestrator.handle_transport_event(TransportEvent::DataReceived {
            kind: DataChannelKind::Input,
            data: vec![0x10, 0x00, 0x38, 0x04, 0x00, 0x00, 0x80, 0x07, 0x00, 0x00],
        });
        orchestrator.set_diagnostics_polling_enabled(true);
        assert!(wait_for(|| {
            h.core.poll();
            h.handle.read().resolution_height() == 1080
        }));
        assert_eq!(h.handle.read().resolution_width(), 1920);

        // The transport reports no size: the decoded size wins over the server metadata.
        HostCore::forward_pipeline_event(
            &h.core.controller,
            PipelineEvent::Resolution {
                width: 2560,
                height: 1440,
            },
        );
        assert!(wait_for(|| {
            h.core.poll();
            h.handle.read().resolution_height() == 1440
        }));
        assert_eq!(h.handle.read().resolution_width(), 2560);

        // A size the transport reports stays authoritative.
        h.fake.set_stats(StreamingStatsSnapshot {
            negotiated_width: 1280,
            negotiated_height: 720,
            ..StreamingStatsSnapshot::default()
        });
        assert!(wait_for(|| {
            h.core.poll();
            h.handle.read().resolution_height() == 720
        }));
        assert_eq!(h.handle.read().resolution_width(), 1280);

        // Keyframe recovery is forwarded too (no control channel here: nothing is sent).
        let sent = h.fake.sent_count();
        HostCore::forward_pipeline_event(&h.core.controller, PipelineEvent::KeyframeRecoveryNeeded);
        assert_eq!(h.fake.sent_count(), sent);
        orchestrator.set_diagnostics_polling_enabled(false);
    }

    /// Keyboard commands, focus, settings and rumble reach the `GamepadInputSource` on the
    /// input thread; the merged keyboard + physical state is what the queue flushes.
    #[test]
    fn input_commands_reach_the_gamepad_source() {
        let mut h = harness();
        let queue = h.core.controller.input_queue();

        // Keyboard state is held while inactive, but nothing is enqueued.
        h.core.handle_command(StreamCommand::SetKey {
            name: "a".into(),
            pressed: true,
        });
        assert!(wait_for(|| h.probe().pressed == vec!["a"]));
        assert!(!queue.lock().has_pending());
        assert_eq!(h.script.lock().samples, 0, "no polling while inactive");

        h.script.lock().sample = NativeGamepadSample {
            connected: true,
            name: "Scripted Pad".into(),
            state: RawGamepadState {
                buttons: GamepadButtons::B,
                ..RawGamepadState::default()
            },
        };
        h.core.inject_event(OrchestratorEvent::Lifecycle(
            StreamLifecycleState::Connected,
        ));
        assert!(wait_for(|| h.handle.read().controller_connected));
        assert_eq!(h.handle.read().controller_name, "Scripted Pad");
        assert!(wait_for(|| queue.lock().has_pending()));
        let packet = queue.lock().flush(now_ms()).expect("a gamepad packet");
        // header, count byte, gamepad index, then the LE16 button mask.
        let buttons = u16::from_le_bytes([packet[HEADER_SIZE + 2], packet[HEADER_SIZE + 3]]);
        assert_ne!(buttons & GamepadButtons::A.bits(), 0, "keyboard A");
        assert_ne!(buttons & GamepadButtons::B.bits(), 0, "physical B");

        // Sticks and triggers.
        h.core
            .handle_command(StreamCommand::SetLeftStick { x: 0.5, y: -0.5 });
        h.core.handle_command(StreamCommand::SetTrigger {
            side: TriggerSide::Right,
            value: 1.0,
        });
        assert!(wait_for(|| {
            let mut queue = queue.lock();
            let Some(packet) = queue.flush(now_ms()) else {
                return false;
            };
            let left_x = i16::from_le_bytes([packet[HEADER_SIZE + 4], packet[HEADER_SIZE + 5]]);
            let right_trigger =
                u16::from_le_bytes([packet[HEADER_SIZE + 14], packet[HEADER_SIZE + 15]]);
            left_x > 0 && right_trigger == u16::MAX
        }));

        // Settings reach the source and set the rumble scale; `refreshPreferences` does
        // not touch the HUD flag.
        h.core
            .handle_command(StreamCommand::ApplySettings(AppSettings {
                deadzone: 0.3,
                swap_ab: true,
                vibration_intensity: 0.5,
                hud_enabled: true,
                ..AppSettings::default()
            }));
        assert!(wait_for(|| {
            let probe = h.probe();
            probe.settings.swap_ab
                && probe.settings.deadzone == 0.3
                && probe.vibration_intensity == 0.5
        }));
        assert!(!h.handle.read().hud_enabled);
        assert!(!h.core.controller.settings().hud_enabled);

        // Rumble: orchestrator event -> input thread -> the device, scaled.
        let report = VibrationReport {
            gamepad_index: 0,
            left_motor_percent: 0.8,
            right_motor_percent: 0.2,
            duration_ms: 100,
            ..VibrationReport::default()
        };
        h.core.inject_event(OrchestratorEvent::Vibration(report));
        assert!(wait_for(|| !h.script.lock().rumbles.is_empty()));
        assert_eq!(h.script.lock().rumbles[0], (report, 0.5));

        // Losing focus releases every held input; `SetInputFocused` mirrors into the snapshot.
        h.core.handle_command(StreamCommand::SetInputFocused(false));
        assert!(!h.handle.read().input_focused);
        assert!(wait_for(|| {
            let probe = h.probe();
            !probe.focused && probe.pressed.is_empty()
        }));
        h.core.handle_command(StreamCommand::SetInputFocused(true));
        h.core.handle_command(StreamCommand::SetKey {
            name: "x".into(),
            pressed: true,
        });
        assert!(wait_for(|| h.probe().pressed == vec!["x"]));
        h.core.handle_command(StreamCommand::ReleaseAll);
        assert!(wait_for(|| h.probe().pressed.is_empty()));

        // Disconnected: polling stops.
        h.core.inject_event(OrchestratorEvent::Lifecycle(
            StreamLifecycleState::Disconnected,
        ));
        assert!(wait_for(|| !h.probe().active));
    }

    /// `setHudEnabled` writes through to the persisted `Settings` (`hud/enabled`) and
    /// `refreshPreferences` leaves the flag alone: a HUD toggled from the stream view
    /// survives the next `ApplySettings` from the settings panel and is what a restart
    /// reads back.
    #[test]
    fn hud_toggle_persists_and_survives_apply_settings() {
        let mut h = harness();
        assert!(!h.handle.read().hud_enabled);
        assert!(!h.settings.read().hud_enabled);

        h.core.handle_command(StreamCommand::SetHudEnabled(true));
        assert!(h.handle.read().hud_enabled);
        assert!(h.core.controller.settings().hud_enabled);
        assert!(h.settings.read().hud_enabled, "written through to Settings");

        // The settings panel sends its (stale) copy: the HUD flag is not reverted and the
        // other preferences still apply.
        h.core
            .handle_command(StreamCommand::ApplySettings(AppSettings {
                hud_enabled: false,
                resolution_profile: "720p".into(),
                ..AppSettings::default()
            }));
        assert!(h.handle.read().hud_enabled);
        let applied = h.core.controller.settings();
        assert!(applied.hud_enabled);
        assert_eq!(applied.resolution_profile, "p720");
        assert!(h.settings.read().hud_enabled);

        h.core.handle_command(StreamCommand::SetHudEnabled(false));
        assert!(!h.handle.read().hud_enabled);
        assert!(!h.settings.read().hud_enabled);

        // A host built from persisted settings starts with the persisted flag.
        let restarted = harness_with(AppSettings {
            hud_enabled: true,
            ..AppSettings::default()
        });
        assert!(restarted.handle.read().hud_enabled);
        assert!(restarted.core.controller.hud_enabled());
    }

    /// The threaded host: commands through the handle, snapshot updates, clean shutdown.
    #[test]
    fn spawned_host_runs_commands_and_shuts_down() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let fake = Arc::new(FakeTransport::new());
        let flushes = Arc::new(AtomicUsize::new(0));
        let decoder_flushes = Arc::clone(&flushes);
        let config = StreamHostConfig {
            transport: HostTransport::Generic(Arc::clone(&fake) as Arc<dyn Transport>),
            auth: None,
            api: None,
            settings: Arc::new(RwLock::new(AppSettings::default())),
            repaint: crate::state::noop_repaint(),
            decoder: Box::new(move |sink| {
                Box::new(FakeDecoder {
                    sink,
                    flushes: decoder_flushes,
                })
            }),
            audio: AudioOutput::with_backend(Box::new(MockBackend::default())),
            gamepad: Box::new(keyboard_only_device),
        };
        let host = StreamHost::spawn(config, runtime.handle()).expect("host");
        let handle = host.handle();
        let video = host.video();
        assert!(fake.has_media_sink());

        handle.send(StreamCommand::StartCloudStream {
            title_id: "t".into(),
            title_name: "Forza".into(),
            hero_image: String::new(),
        });
        assert!(wait_for(|| handle.read().active));
        assert_eq!(handle.read().title_name, "Forza");
        assert_eq!(handle.read().lifecycle, StreamLifecycleState::Failed);
        handle.send(StreamCommand::SetHudEnabled(true));
        assert!(wait_for(|| handle.read().hud_enabled));
        handle.send(StreamCommand::StopStreaming);
        assert!(wait_for(|| !handle.read().active));

        host.shutdown();
        assert!(video.lock().take_latest().is_none());
        assert_eq!(fake.close_count(), 1);
    }
}
