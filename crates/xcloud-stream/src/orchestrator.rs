//! Port of qt/src/streaming/SignalingOrchestrator.{h,cpp} ([`SignalingOrchestrator`]) and
//! qt/src/app/StreamController.{h,cpp} ([`StreamController`]).
//!
//! The C++ orchestrator is a `QObject` living on the GUI thread: every bridge signal,
//! timer and coroutine continuation runs serialised on that thread. Here:
//!
//! * All mutable state sits in one `Mutex<State>`; every entry point (public method,
//!   transport event, timer, connect step) locks it, mutates, collects the
//!   [`ChannelAction`]s the sans-IO channels return and executes them against the
//!   [`Transport`] before releasing the lock. The transport never calls back into the
//!   orchestrator synchronously (it only emits events on a channel), so holding the
//!   lock across `transport.send` is safe and keeps the C++ ordering.
//! * `runConnect` (a Qt coroutine) is a tokio task; `QTimer::singleShot(2000)` retries,
//!   the 1 s stats poll, the input flush loop and the control keyframe timer are tokio
//!   tasks too. Timers run on tokio's timer wheel: measured on this machine (macOS,
//!   8 ms deadline loop, two busy tasks as noise) `sleep_until` has a mean interval
//!   error of 0.24 ms, p99 2.0 ms, which equals a dedicated `std::thread` loop
//!   (0.52 ms mean, p99 1.9 ms). Nothing is gained by a thread, so the input loop
//!   stays on tokio.
//! * The transport's crossbeam event receiver is bridged by a dedicated std thread
//!   (`xcloud-orchestrator-events`) that blocks on `recv` and calls
//!   [`SignalingOrchestrator::handle_transport_event`] directly. It exits when the
//!   orchestrator is dropped or the transport closes its sender.
//! * Qt signals become [`OrchestratorEvent`]s on a `tokio::sync::broadcast` channel
//!   ([`SignalingOrchestrator::subscribe`]). Getters (`lifecycle`, `stats`, ...) read
//!   the latest value synchronously, as the Qt properties did.
//! * The media pipeline is not owned here (`pipeline.rs`): the host feeds
//!   [`RuntimeSignal`]s (`videoRecoveryNeeded`, decoded frame size) through
//!   [`SignalingOrchestrator::signal`].
//!
//! Time inside the orchestrator is `tokio::time::Instant` based so that
//! `tokio::time::pause()` tests run the 125 Hz loop and the 1 s poll in virtual time.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use tokio::runtime::Handle;
use tokio::sync::{Notify, broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use xcloud_api::auth::MicrosoftAuthService;
use xcloud_api::client::XCloudApiClient;
use xcloud_api::session::{IntermediateState, SessionState, StreamSession};
use xcloud_proto::channels::chat::ChatChannel;
use xcloud_proto::channels::control::{ChannelAction, ControlChannel};
use xcloud_proto::channels::input::InputChannel;
use xcloud_proto::channels::message::MessageChannel;
use xcloud_proto::input::packet::VibrationReport;
use xcloud_proto::input::queue::InputQueue;
use xcloud_proto::models::{IceCandidatePayload, StreamDimensions};
use xcloud_proto::preferences::{
    StreamKind, StreamPreferences, StreamResolutionMode, stream_resolution_from_string,
};
use xcloud_proto::sdp::ice::IceProcessor;
use xcloud_proto::sdp::processor::{
    ProcessOptions, format_sdp_for_logging, process_local_sdp, process_remote_sdp,
};
use xcloud_proto::sdp::utils::ufrag_from_sdp;
use xcloud_proto::streaming_config::{StreamingConfig, quality_tier_profile, resolution_alias};
use xcloud_proto::streaming_types::{
    DataChannelKind, PeerConnectionState, SessionDescription, StreamError, StreamErrorCode,
    StreamLifecycleState, StreamingStatsSnapshot, stream_lifecycle_name,
};
use xcloud_webrtc::{Transport, TransportEvent};

use crate::{Error, Result};

const LOG: &str = "Streaming";
const CONTROLLER_LOG: &str = "StreamController";

/// `m_maxAttempts`: a session can come up with a dead media path (ICE connects, DTLS
/// never answers); a fresh session re-rolls that server-side fault.
pub const MAX_ATTEMPTS: u32 = 2;
/// `QTimer::singleShot(2000, startAttempt)`.
pub const RETRY_DELAY: Duration = Duration::from_millis(2000);
/// `m_statsTimer->setInterval(1000)`.
pub const STATS_INTERVAL: Duration = Duration::from_millis(1000);
/// `waitUntil*(pollIntervalMs = 1000, ...)`.
pub const SESSION_POLL_INTERVAL: Duration = Duration::from_millis(1000);
/// `waitUntilReadyOrProvisioned(1000, 300000, ...)` for cloud sessions.
pub const CLOUD_PROVISION_TIMEOUT: Duration = Duration::from_millis(300_000);
/// `waitUntilProvisioned(1000, 60000, ...)` / `waitUntilReady(1000, 60000)`.
pub const PROVISIONED_TIMEOUT: Duration = Duration::from_millis(60_000);
/// `m_session->startKeepalive(30)`.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
/// The `for (i < 300) delay(100)` wait for the peer connection to report connected.
pub const CONNECT_WAIT_TIMEOUT: Duration = Duration::from_millis(300 * 100);
/// The poll step of that wait.
pub const CONNECT_WAIT_STEP: Duration = Duration::from_millis(100);
/// Capacity of the [`OrchestratorEvent`] broadcast channel.
pub const EVENT_CAPACITY: usize = 256;

/// The queue the input crate enqueues controller frames into and the input channel
/// flushes at 125 Hz. Same shape as `xcloud_input::source::SharedInputQueue`.
pub type SharedInputQueue = Arc<Mutex<InputQueue>>;

/// `SignalingOrchestrator::DisconnectIntent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DisconnectIntent {
    #[default]
    Reconnectable,
    UserInitiated,
    ServerInitiated,
    ReconnectTransition,
}

/// Everything the C++ orchestrator emitted as a signal.
#[derive(Debug, Clone, PartialEq)]
pub enum OrchestratorEvent {
    /// `lifecycleChanged(state)`.
    Lifecycle(StreamLifecycleState),
    /// `waitingForResources(estimatedWaitSeconds)`.
    WaitingForResources(i32),
    /// `statsChanged()`, carrying the snapshot `stats()` now returns.
    StatsChanged(StreamingStatsSnapshot),
    /// `errorOccurred(error)`.
    Error(StreamError),
    /// `serverInitiatedDisconnect()` (the message channel's
    /// `/streaming/sessionLifetimeManagement/serverInitiatedDisconnect`).
    ServerInitiatedDisconnect,
    /// `videoTrackAvailable()`.
    VideoTrackAvailable,
    /// The `setVibrationHandler` callback.
    Vibration(VibrationReport),
}

/// Signals the host feeds from the media pipeline / renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSignal {
    /// `WebRtcBridge::videoRecoveryNeeded`: the decoder lost sync; ask for a keyframe
    /// over the control channel (once the control startup completed).
    VideoRecoveryNeeded,
    /// Dimensions of the latest decoded frame, for a decoder that runs outside the
    /// transport. Picked up by the next stats poll: it fills `negotiated_width/height`
    /// when the transport's `collect_stats` reports none, and is authoritative over
    /// the input channel's server metadata, which may describe a different server
    /// surface or an earlier stream size. It never changes `stats()` on its own.
    DecodedResolution { width: u32, height: u32 },
}

/// `optionsFromConfig(config, native)`.
fn options_from_config(config: &StreamingConfig, native: bool) -> ProcessOptions {
    let mut options = ProcessOptions {
        stereo_audio: config.stereo_audio_enabled,
        ..ProcessOptions::default()
    };
    if !native {
        options.max_video_bitrate_kbps = config.max_video_bitrate_kbps;
        options.max_audio_bitrate_kbps = config.max_audio_bitrate_kbps;
        options.preferred_video_codec = config.preferred_sdp_codec.clone();
        options.max_video_frame_rate = config.requested_video_max_frame_rate;
        options.h264_profile_level_id_override = config.h264_fallback_profile_level_id.clone();
    } else {
        // The native client declares its capability through the message channel
        // rather than `b=AS`, and its offer template must stay intact.
        options.inject_bitrate_lines = false;
        options.max_video_frame_rate = 0;
        options.scale_video_caps_to_1080 = config.scale_video_caps_to_1080;
        options.h264_profile_level_id = config.h264_profile_level_id.clone();
    }
    options
}

/// `QUuid::createUuid()` folded into the `MessageChannel::with_seed` seed.
fn random_seed() -> u64 {
    let (hi, lo) = uuid::Uuid::new_v4().as_u64_pair();
    hi ^ lo.rotate_left(17)
}

#[derive(Default)]
struct Tasks {
    connect: Option<JoinHandle<()>>,
    retry: Option<JoinHandle<()>>,
    stats: Option<JoinHandle<()>>,
    channel_timer: Option<JoinHandle<()>>,
    intermediate: Option<JoinHandle<()>>,
}

impl Tasks {
    fn abort_all(&mut self) {
        for handle in [
            self.connect.take(),
            self.retry.take(),
            self.stats.take(),
            self.channel_timer.take(),
            self.intermediate.take(),
        ]
        .into_iter()
        .flatten()
        {
            handle.abort();
        }
    }
}

fn replace_task(slot: &mut Option<JoinHandle<()>>, handle: Option<JoinHandle<()>>) {
    if let Some(old) = slot.take() {
        old.abort();
    }
    *slot = handle;
}

/// The members of the C++ class, guarded by one mutex.
struct State {
    config: StreamingConfig,
    preferences: StreamPreferences,

    session: Option<Arc<StreamSession>>,
    control: Option<ControlChannel>,
    input: Option<InputChannel>,
    message: Option<MessageChannel>,
    chat: Option<ChatChannel>,

    lifecycle: StreamLifecycleState,
    last_error: StreamError,
    disconnect_intent: DisconnectIntent,
    stats: StreamingStatsSnapshot,

    diagnostics_polling: bool,
    control_startup_completed: bool,
    control_open: bool,
    input_open: bool,
    message_ready: bool,
    cancelled: bool,
    /// `QHash<int, bool>`; a BTreeMap keeps the replay order deterministic.
    desired_gamepad_state: BTreeMap<i32, bool>,
    server_width: u32,
    server_height: u32,
    decoded_width: u32,
    decoded_height: u32,

    kind: StreamKind,
    target_id: String,
    user_token: String,
    attempt: u32,
    max_attempts: u32,

    tasks: Tasks,
}

impl State {
    fn new() -> Self {
        Self {
            config: StreamingConfig::defaults(),
            preferences: StreamPreferences::default(),
            session: None,
            control: None,
            input: None,
            message: None,
            chat: None,
            lifecycle: StreamLifecycleState::Idle,
            last_error: StreamError::default(),
            disconnect_intent: DisconnectIntent::Reconnectable,
            stats: StreamingStatsSnapshot::default(),
            diagnostics_polling: false,
            control_startup_completed: false,
            control_open: false,
            input_open: false,
            message_ready: false,
            cancelled: false,
            desired_gamepad_state: BTreeMap::new(),
            server_width: 0,
            server_height: 0,
            decoded_width: 0,
            decoded_height: 0,
            kind: StreamKind::Cloud,
            target_id: String::new(),
            user_token: String::new(),
            attempt: 0,
            max_attempts: MAX_ATTEMPTS,
            tasks: Tasks::default(),
        }
    }

    /// Earliest deadline of the input flush loop and the control keyframe timer.
    fn next_timer_due_ms(&self) -> Option<u64> {
        let input = self.input.as_ref().and_then(InputChannel::next_tick_due_ms);
        let control = self
            .control
            .as_ref()
            .and_then(ControlChannel::next_keyframe_request_due_ms);
        match (input, control) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

struct Shared {
    api: Option<Arc<XCloudApiClient>>,
    auth: Option<Arc<MicrosoftAuthService>>,
    transport: Arc<dyn Transport>,
    runtime: Handle,
    input_queue: SharedInputQueue,
    events: broadcast::Sender<OrchestratorEvent>,
    state: Mutex<State>,
    /// Latest `ConnectionStateChanged`, for the connect task's wait.
    peer_state: watch::Sender<PeerConnectionState>,
    /// Woken whenever a channel deadline may have changed.
    timers_changed: Notify,
    clock_start: tokio::time::Instant,
}

impl Shared {
    /// Milliseconds since construction on the (pausable) tokio clock. Replaces the
    /// `uptime()` QElapsedTimer the channels were fed.
    fn now_ms(&self) -> u64 {
        tokio::time::Instant::now()
            .saturating_duration_since(self.clock_start)
            .as_millis() as u64
    }

    fn emit(&self, event: OrchestratorEvent) {
        // No subscriber is not an error: the Qt signal had no slots either.
        let _ = self.events.send(event);
    }

    fn set_lifecycle(&self, state: &mut State, lifecycle: StreamLifecycleState) {
        if state.lifecycle == lifecycle {
            return;
        }
        state.lifecycle = lifecycle;
        self.emit(OrchestratorEvent::Lifecycle(lifecycle));
    }

    /// `fail(code, message)`: retries a dead media path with a fresh session (the
    /// native client's policy); authentication and configuration failures are not
    /// retryable.
    fn fail(
        self: &Arc<Self>,
        state: &mut State,
        code: StreamErrorCode,
        message: impl Into<String>,
    ) {
        let message = message.into();
        let retryable = matches!(
            code,
            StreamErrorCode::WebRtc | StreamErrorCode::Signaling | StreamErrorCode::Network
        );
        if retryable && !state.cancelled && state.attempt < state.max_attempts {
            tracing::warn!(
                target: LOG,
                "attempt {} failed ({}); retrying",
                state.attempt,
                message
            );
            state.last_error = StreamError::new(code, message);
            self.set_lifecycle(state, StreamLifecycleState::Provisioning);
            let shared = Arc::clone(self);
            let retry = self.runtime.spawn(async move {
                tokio::time::sleep(RETRY_DELAY).await;
                shared.start_attempt();
            });
            replace_task(&mut state.tasks.retry, Some(retry));
            return;
        }

        state.last_error = StreamError::new(code, message);
        tracing::error!(
            target: LOG,
            "Stream failed [{}]: {}",
            state.last_error.code_name(),
            state.last_error.message
        );
        self.set_lifecycle(state, StreamLifecycleState::Failed);
        self.emit(OrchestratorEvent::Error(state.last_error.clone()));
    }

    /// `installChannels`.
    fn install_channels(self: &Arc<Self>, state: &mut State) {
        self.teardown_channels(state);
        state.control_startup_completed = false;

        let profile = quality_tier_profile(state.preferences.resolution);
        state
            .config
            .apply_quality_tier(state.preferences.resolution);

        let mut control = ControlChannel::new(
            i32::try_from(state.config.keyframe_request_interval_seconds).unwrap_or(i32::MAX),
        );
        control.configure_video_preference(
            state.config.video_width_hint,
            state.config.video_height_hint,
            state.config.preferred_frame_rate,
            Some(state.config.preferred_color_range.as_str()),
        );
        state.control = Some(control);
        state.input = Some(InputChannel::new());

        let mut message = MessageChannel::new(StreamDimensions {
            width: profile.width,
            height: profile.height,
        })
        .with_seed(random_seed());
        message.configure_device_capabilities(
            profile.width,
            profile.height,
            profile.max_bitrate_kbps,
            state.config.preferred_frame_rate,
        );
        state.message = Some(message);
        state.chat = Some(ChatChannel::new());

        let shared = Arc::clone(self);
        let timer = self
            .runtime
            .spawn(async move { shared.channel_timer_loop().await });
        replace_task(&mut state.tasks.channel_timer, Some(timer));
    }

    /// `teardownChannels`.
    fn teardown_channels(&self, state: &mut State) {
        if let Some(control) = state.control.as_mut() {
            control.destroy();
        }
        if let Some(input) = state.input.as_mut() {
            input.destroy();
        }
        if let Some(message) = state.message.as_mut() {
            message.destroy();
        }
        if let Some(chat) = state.chat.as_mut() {
            chat.destroy();
        }
        state.control = None;
        state.input = None;
        state.message = None;
        state.chat = None;
        state.control_startup_completed = false;
        state.control_open = false;
        state.input_open = false;
        state.message_ready = false;
        replace_task(&mut state.tasks.channel_timer, None);
    }

    /// `startInputWhenReady`: SCTP association readiness alone does not mean the
    /// xCloud application handshake has completed. Match green-nx: authorize, attach,
    /// then input.
    fn start_input_when_ready(&self, state: &mut State) {
        if !state.message_ready || !state.control_open {
            return;
        }
        let now = self.now_ms();
        let mut actions = Vec::new();
        let (Some(control), Some(input)) = (state.control.as_mut(), state.input.as_mut()) else {
            return;
        };
        if !state.control_startup_completed {
            tracing::info!(
                target: LOG,
                "Message handshake acknowledged; authorizing controller input"
            );
            actions.extend(control.on_open(now));
            state.control_startup_completed = true;
            for (index, connected) in &state.desired_gamepad_state {
                actions.extend(control.send_gamepad_changed(*index, *connected));
            }
            actions
                .extend(control.send_user_requested_resolution(resolution_alias(
                    state.preferences.resolution,
                )));
        }
        if state.input_open {
            let mut queue = self.input_queue.lock();
            actions.extend(input.on_open(&mut *queue, now));
        }
        self.execute(state, actions);
        self.timers_changed.notify_one();
    }

    /// `onBridgeChannelOpened`.
    fn on_channel_opened(&self, state: &mut State, kind: DataChannelKind) {
        match kind {
            DataChannelKind::Control => {
                state.control_open = true;
                self.start_input_when_ready(state);
            }
            DataChannelKind::Input => {
                state.input_open = true;
                self.start_input_when_ready(state);
            }
            DataChannelKind::Message => {
                if let Some(message) = state.message.as_mut() {
                    let actions = message.on_channel_open();
                    self.execute(state, actions);
                }
            }
            DataChannelKind::Chat => {
                if let Some(chat) = state.chat.as_ref() {
                    let actions = chat.on_channel_open();
                    self.execute(state, actions);
                }
            }
        }
    }

    /// Performs the channels' actions: sends go to the transport (a binary send on
    /// the input channel is immediately completed with `on_send_result`, the
    /// `QFutureWatcher` of the C++), events are mapped onto the orchestrator's own
    /// handlers and signals. FIFO, so follow-up actions keep the C++ order.
    fn execute(&self, state: &mut State, actions: Vec<ChannelAction>) {
        let mut queue: VecDeque<ChannelAction> = actions.into();
        while let Some(action) = queue.pop_front() {
            match action {
                ChannelAction::SendText { kind, text } => {
                    if let Err(error) = self.transport.send_text(kind, &text) {
                        tracing::warn!(target: LOG, "send_text on {kind:?} failed: {error}");
                    }
                }
                ChannelAction::SendBinary { kind, bytes } => {
                    let ok = match self.transport.send(kind, &bytes) {
                        Ok(()) => true,
                        Err(error) => {
                            tracing::debug!(target: LOG, "send on {kind:?} failed: {error}");
                            false
                        }
                    };
                    if kind == DataChannelKind::Input
                        && let Some(input) = state.input.as_mut()
                    {
                        queue.extend(input.on_send_result(ok));
                    }
                }
                ChannelAction::HandshakeCompleted => {
                    state.message_ready = true;
                    self.start_input_when_ready(state);
                }
                ChannelAction::ServerMetadata { width, height } => {
                    state.server_width = width;
                    state.server_height = height;
                    if let Some(message) = state.message.as_mut() {
                        queue.extend(
                            message.send_dimensions_changed(StreamDimensions { width, height }),
                        );
                    }
                    self.emit(OrchestratorEvent::StatsChanged(state.stats.clone()));
                }
                ChannelAction::Vibration(report) => {
                    self.emit(OrchestratorEvent::Vibration(report));
                }
                ChannelAction::ServerInitiatedDisconnect => {
                    tracing::info!(target: LOG, "Server initiated disconnect");
                    self.emit(OrchestratorEvent::ServerInitiatedDisconnect);
                }
                ChannelAction::ProtocolMessage { target, .. } => {
                    tracing::debug!(target: LOG, "Protocol message: {target}");
                }
                ChannelAction::MessageReceived { text } => {
                    tracing::trace!(target: LOG, "Message channel rx: {text}");
                }
                ChannelAction::FlushTelemetry { hz, jitter_ms } => {
                    tracing::debug!(
                        target: "InputChannel",
                        "input flush {hz:.1} Hz, jitter {jitter_ms:.2} ms"
                    );
                }
            }
        }
    }

    /// The bridge-signal slots of the C++ constructor.
    fn handle_transport_event(self: &Arc<Self>, event: TransportEvent) {
        match event {
            TransportEvent::ConnectionStateChanged(peer) => {
                tracing::info!(target: LOG, "Peer connection state: {peer:?}");
                self.peer_state.send_replace(peer);
            }
            TransportEvent::ChannelOpened(kind) => {
                let mut state = self.state.lock();
                self.on_channel_opened(&mut state, kind);
            }
            TransportEvent::DataReceived { kind, data } => {
                if kind != DataChannelKind::Input {
                    return;
                }
                let mut state = self.state.lock();
                if let Some(input) = state.input.as_mut() {
                    let actions = input.on_binary(&data);
                    self.execute(&mut state, actions);
                }
            }
            TransportEvent::TextReceived { kind, text } => {
                if kind != DataChannelKind::Message {
                    return;
                }
                let mut state = self.state.lock();
                if let Some(message) = state.message.as_mut() {
                    let actions = message.on_text_message(&text);
                    self.execute(&mut state, actions);
                }
            }
            TransportEvent::Failed(message) => {
                let mut state = self.state.lock();
                self.fail(&mut state, StreamErrorCode::WebRtc, message);
            }
            TransportEvent::CandidateGathered(_)
            | TransportEvent::VideoTrackReceived
            | TransportEvent::AudioTrackReceived => {}
        }
    }

    /// `WebRtcBridge::videoRecoveryNeeded` slot.
    fn video_recovery_needed(&self) {
        let mut state = self.state.lock();
        if !state.control_startup_completed {
            return;
        }
        let Some(control) = state.control.as_ref() else {
            return;
        };
        tracing::info!(
            target: LOG,
            "Video recovery: requesting keyframe over control channel"
        );
        let actions = control.request_keyframe(true);
        self.execute(&mut state, actions);
    }

    /// Records the latest decoded frame size. In the C++ the pipeline wrote it into
    /// the snapshot `collectStats` returns; here the decoder runs outside the
    /// transport, so the host feeds it and the stats timer slot ([`Self::poll_stats`])
    /// folds it in. Like the C++, nothing else touches `stats` or emits
    /// `statsChanged` between polls.
    fn decoded_resolution(&self, width: u32, height: u32) {
        let mut state = self.state.lock();
        state.decoded_width = width;
        state.decoded_height = height;
    }

    /// The `QTimer` replacement for `InputChannel::m_timer` (125 Hz flush) and
    /// `ControlChannel::m_keyframeTimer`: sleeps until the earliest channel deadline,
    /// ticks, repeats. Re-evaluated whenever `timers_changed` fires.
    async fn channel_timer_loop(self: Arc<Self>) {
        loop {
            let due = self.state.lock().next_timer_due_ms();
            match due {
                None => self.timers_changed.notified().await,
                Some(due) => {
                    let delay = due.saturating_sub(self.now_ms());
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(delay)) => self.tick_channels(),
                        _ = self.timers_changed.notified() => {}
                    }
                }
            }
        }
    }

    fn tick_channels(&self) {
        let mut state = self.state.lock();
        let now = self.now_ms();
        let mut actions = Vec::new();
        if let Some(input) = state.input.as_mut() {
            let mut queue = self.input_queue.lock();
            actions.extend(input.tick(&mut *queue, now));
        }
        if let Some(control) = state.control.as_mut() {
            actions.extend(control.tick(now));
        }
        self.execute(&mut state, actions);
    }

    /// `startStatsPolling`.
    fn start_stats_polling(self: &Arc<Self>, state: &mut State) {
        if state.tasks.stats.is_some() {
            return;
        }
        let shared = Arc::clone(self);
        let handle = self.runtime.spawn(async move {
            let mut interval = tokio::time::interval_at(
                tokio::time::Instant::now() + STATS_INTERVAL,
                STATS_INTERVAL,
            );
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                shared.poll_stats();
            }
        });
        state.tasks.stats = Some(handle);
    }

    fn stop_stats_polling(&self, state: &mut State) {
        replace_task(&mut state.tasks.stats, None);
    }

    /// The stats timer slot.
    fn poll_stats(&self) {
        let mut stats = self.transport.collect_stats();
        let mut state = self.state.lock();
        // Decoded frame dimensions are authoritative. Input-channel metadata may
        // describe a different server surface or an earlier stream size.
        //
        // The transport's snapshot carries them when its media sink decodes inline
        // (the C++ `NativeMediaPipeline` shape), so whatever it reports wins whenever
        // it is > 0: a later size change there must always show. The host-fed
        // [`RuntimeSignal::DecodedResolution`] stands in for that when the transport
        // reports nothing, and the server metadata is the last resort.
        if (stats.negotiated_width <= 0 || stats.negotiated_height <= 0)
            && state.decoded_width > 0
            && state.decoded_height > 0
        {
            stats.negotiated_width = i32::try_from(state.decoded_width).unwrap_or(i32::MAX);
            stats.negotiated_height = i32::try_from(state.decoded_height).unwrap_or(i32::MAX);
        }
        if (stats.negotiated_width <= 0 || stats.negotiated_height <= 0)
            && state.server_width > 0
            && state.server_height > 0
        {
            stats.negotiated_width = i32::try_from(state.server_width).unwrap_or(i32::MAX);
            stats.negotiated_height = i32::try_from(state.server_height).unwrap_or(i32::MAX);
        }
        if state.input.is_some() && stats.renderer_mode.is_empty() {
            stats.renderer_mode = "webrtc".to_owned();
        }
        state.stats = stats.clone();
        self.emit(OrchestratorEvent::StatsChanged(stats));
    }

    /// `resetForConnect`.
    fn reset_for_connect(self: &Arc<Self>, state: &mut State) {
        state.cancelled = false;
        state.last_error = StreamError::default();
        state.disconnect_intent = DisconnectIntent::Reconnectable;
        state.stats = StreamingStatsSnapshot::default();
        state.server_width = 0;
        state.server_height = 0;
        state.decoded_width = 0;
        state.decoded_height = 0;
        self.install_channels(state);
    }

    /// `startAttempt` plus the synchronous prologue of `runConnect` (everything up
    /// to its first `co_await` ran inline in the C++ too).
    fn start_attempt(self: &Arc<Self>) {
        let mut state = self.state.lock();
        if state.cancelled {
            return;
        }
        state.attempt += 1;
        tracing::info!(
            target: LOG,
            "starting stream attempt {} of {}",
            state.attempt,
            state.max_attempts
        );

        self.reset_for_connect(&mut state);
        self.set_lifecycle(&mut state, StreamLifecycleState::StartingSession);

        let Some(api) = self.api.clone() else {
            self.fail(
                &mut state,
                StreamErrorCode::Unsupported,
                "Streaming is not configured",
            );
            return;
        };

        let kind = state.kind;
        let target_id = state.target_id.clone();
        let user_token = state.user_token.clone();
        let shared = Arc::clone(self);
        let connect = self
            .runtime
            .spawn(async move { shared.run_connect(api, kind, target_id, user_token).await });
        replace_task(&mut state.tasks.connect, Some(connect));
    }

    fn cancelled(&self) -> bool {
        self.state.lock().cancelled
    }

    fn fail_now(self: &Arc<Self>, code: StreamErrorCode, message: impl Into<String>) {
        let mut state = self.state.lock();
        self.fail(&mut state, code, message);
    }

    fn set_lifecycle_now(&self, lifecycle: StreamLifecycleState) {
        let mut state = self.state.lock();
        self.set_lifecycle(&mut state, lifecycle);
    }

    /// The asynchronous remainder of `runConnect`.
    async fn run_connect(
        self: Arc<Self>,
        api: Arc<XCloudApiClient>,
        kind: StreamKind,
        target_id: String,
        msa_user_token: String,
    ) {
        // The service allows one session per account; clear any left behind.
        if let Err(error) = api.cleanup_stale_sessions(kind).await {
            tracing::debug!(target: LOG, "cleanupStaleSessions: {error}");
        }

        let preferences = self.state.lock().preferences.clone();
        let start = api.start_stream(kind, &target_id, &preferences).await;
        if self.cancelled() {
            return;
        }
        let start = match start {
            Ok(response) => response,
            Err(error) => {
                self.fail_now(StreamErrorCode::Signaling, error.to_string());
                return;
            }
        };

        let session = Arc::new(StreamSession::new(Arc::clone(&api), &start));
        let (intermediate_tx, mut intermediate_rx) = mpsc::unbounded_channel::<IntermediateState>();
        session.set_intermediate_state_sender(intermediate_tx);
        let intermediate_task = {
            let shared = Arc::clone(&self);
            self.runtime.spawn(async move {
                while let Some(intermediate) = intermediate_rx.recv().await {
                    let mut state = shared.state.lock();
                    shared.set_lifecycle(&mut state, StreamLifecycleState::WaitingForResources);
                    shared.emit(OrchestratorEvent::WaitingForResources(
                        intermediate.estimated_wait_seconds,
                    ));
                }
            })
        };
        {
            let mut state = self.state.lock();
            if let Some(previous) = state.session.take() {
                previous.stop_keepalive();
            }
            state.session = Some(Arc::clone(&session));
            replace_task(&mut state.tasks.intermediate, Some(intermediate_task));
            self.set_lifecycle(&mut state, StreamLifecycleState::Provisioning);
        }

        if kind == StreamKind::Cloud {
            let initial = session
                .wait_until_ready_or_provisioned(
                    SESSION_POLL_INTERVAL,
                    CLOUD_PROVISION_TIMEOUT,
                    &target_id,
                )
                .await;
            if self.cancelled() {
                return;
            }
            match initial {
                Err(error) => {
                    self.fail_now(StreamErrorCode::Signaling, error.to_string());
                    return;
                }
                Ok(SessionState::ReadyToConnect) => {
                    self.set_lifecycle_now(StreamLifecycleState::ReadyToConnect);
                    if msa_user_token.is_empty() {
                        self.fail_now(
                            StreamErrorCode::Authentication,
                            "Missing MSA token for xCloud /connect auth step",
                        );
                        return;
                    }
                    self.set_lifecycle_now(StreamLifecycleState::Provisioning);
                    if let Err(error) = session.send_msal_auth(&msa_user_token).await {
                        self.fail_now(StreamErrorCode::Authentication, error.to_string());
                        return;
                    }
                    if let Err(error) = session
                        .wait_until_provisioned(
                            SESSION_POLL_INTERVAL,
                            PROVISIONED_TIMEOUT,
                            &target_id,
                        )
                        .await
                    {
                        self.fail_now(StreamErrorCode::Signaling, error.to_string());
                        return;
                    }
                }
                Ok(_) => self.set_lifecycle_now(StreamLifecycleState::Provisioning),
            }
        } else {
            let ready = session
                .wait_until_ready(SESSION_POLL_INTERVAL, PROVISIONED_TIMEOUT)
                .await;
            if self.cancelled() {
                return;
            }
            if let Err(error) = ready {
                self.fail_now(StreamErrorCode::Signaling, error.to_string());
                return;
            }
            self.set_lifecycle_now(StreamLifecycleState::ReadyToConnect);
        }

        self.set_lifecycle_now(StreamLifecycleState::ConnectingWebRTC);

        let native = self.transport.rtcp_sender().is_some();
        let config = self.state.lock().config.clone();

        let offer = match self.transport.create_offer(&config) {
            Ok(offer) => offer,
            Err(error) => {
                self.fail_now(StreamErrorCode::WebRtc, error.to_string());
                return;
            }
        };

        let processed_sdp = process_local_sdp(&offer.sdp, &options_from_config(&config, native));
        if config.log_local_sdp_offer {
            tracing::info!(target: LOG, "{}", format_sdp_for_logging(&processed_sdp, true));
        }

        if let Err(error) = self.transport.set_local_description(&SessionDescription {
            kind: "offer".to_owned(),
            sdp: processed_sdp.clone(),
        }) {
            self.fail_now(StreamErrorCode::WebRtc, error.to_string());
            return;
        }

        let answer_sdp = match session.exchange_sdp(&processed_sdp).await {
            Ok(answer) => answer,
            Err(error) => {
                self.fail_now(StreamErrorCode::Signaling, error.to_string());
                return;
            }
        };
        let remote_sdp = process_remote_sdp(&answer_sdp);
        let remote_description = SessionDescription {
            kind: "answer".to_owned(),
            sdp: remote_sdp,
        };

        let ufrag = ufrag_from_sdp(&processed_sdp);
        let mut local_candidates: Vec<IceCandidatePayload> = self.transport.local_ice_candidates();
        for candidate in &mut local_candidates {
            if candidate.username_fragment.is_empty() && !ufrag.is_empty() {
                candidate.username_fragment = ufrag.clone();
            }
        }
        tracing::info!(
            target: LOG,
            "Local ICE candidates gathered: {}",
            local_candidates.len()
        );

        let remote_candidates = session
            .exchange_ice(&local_candidates, preferences.prefer_ipv6)
            .await;

        // Some backends require candidates before the remote description.
        // The webrtc-rs backend needs the remote description first.
        let candidates_first = self
            .transport
            .requires_candidates_before_remote_description();
        if !candidates_first
            && let Err(error) = self.transport.set_remote_description(&remote_description)
        {
            self.fail_now(StreamErrorCode::WebRtc, error.to_string());
            return;
        }

        match remote_candidates {
            Ok(candidates) if !candidates.is_empty() => {
                let expanded = IceProcessor.expand_candidates(&candidates);
                tracing::info!(
                    target: LOG,
                    "Remote ICE candidates: {} (expanded {})",
                    candidates.len(),
                    expanded.len()
                );
                for candidate in &expanded {
                    if let Err(error) = self.transport.add_remote_ice_candidate(candidate) {
                        tracing::warn!(
                            target: LOG,
                            "Failed adding remote ICE candidate: {error}"
                        );
                    }
                }
            }
            Err(error) => {
                self.fail_now(
                    StreamErrorCode::Signaling,
                    format!("ICE exchange failed: {error}"),
                );
                return;
            }
            Ok(_) => {
                self.fail_now(StreamErrorCode::Signaling, "Server sent no ICE candidates");
                return;
            }
        }

        if candidates_first
            && let Err(error) = self.transport.set_remote_description(&remote_description)
        {
            self.fail_now(StreamErrorCode::WebRtc, error.to_string());
            return;
        }

        session.start_keepalive(KEEPALIVE_INTERVAL);

        // Wait for the peer connection to report connected.
        let deadline = tokio::time::Instant::now() + CONNECT_WAIT_TIMEOUT;
        let mut peer_state = self.peer_state.subscribe();
        loop {
            if self.cancelled() {
                return;
            }
            if self.transport.connection_state() == PeerConnectionState::Connected
                || *peer_state.borrow_and_update() == PeerConnectionState::Connected
            {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(CONNECT_WAIT_STEP) => {}
                changed = peer_state.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
            }
        }
        if self.transport.connection_state() != PeerConnectionState::Connected {
            tracing::warn!(
                target: LOG,
                "Peer connection did not report connected; continuing anyway"
            );
        }

        let mut state = self.state.lock();
        state.disconnect_intent = DisconnectIntent::Reconnectable;
        self.set_lifecycle(&mut state, StreamLifecycleState::Connected);
        self.emit(OrchestratorEvent::VideoTrackAvailable);
        if state.diagnostics_polling {
            self.start_stats_polling(&mut state);
        }
    }

    /// `disconnect(intent)`. Teardown order as in the C++: stats polling, channels
    /// (input loop stops with them), session keepalive + delete, then the transport.
    fn disconnect(self: &Arc<Self>, intent: DisconnectIntent) {
        let session = {
            let mut state = self.state.lock();
            state.disconnect_intent = intent;
            state.cancelled = true;
            self.set_lifecycle(&mut state, StreamLifecycleState::Disconnecting);

            replace_task(&mut state.tasks.connect, None);
            replace_task(&mut state.tasks.retry, None);
            self.stop_stats_polling(&mut state);
            self.teardown_channels(&mut state);
            replace_task(&mut state.tasks.intermediate, None);
            state.session.take()
        };
        if let Some(session) = session {
            session.stop_keepalive();
            // `m_session->stop()` + `deleteLater()`: the DELETE is best effort.
            self.runtime.spawn(async move {
                if let Err(error) = session.stop().await {
                    tracing::debug!(target: LOG, "session stop failed: {error}");
                }
            });
        }
        self.transport.close();
        self.set_lifecycle_now(StreamLifecycleState::Disconnected);
    }
}

/// Runs the full streaming startup sequence (provision, signal, connect) and owns
/// the four data channels. Mirrors the Swift StreamingRuntime.
pub struct SignalingOrchestrator {
    shared: Arc<Shared>,
    /// Dropping this stops the event bridge thread.
    _shutdown: Sender<()>,
}

impl std::fmt::Debug for SignalingOrchestrator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.shared.state.lock();
        f.debug_struct("SignalingOrchestrator")
            .field("lifecycle", &state.lifecycle)
            .field("attempt", &state.attempt)
            .field("last_error", &state.last_error)
            .finish_non_exhaustive()
    }
}

impl SignalingOrchestrator {
    /// `SignalingOrchestrator(api, auth, bridge)`. `api`/`auth` may be `None` like the
    /// C++ `nullptr` (tests drive the channels without a network session). Must be
    /// called inside a tokio runtime: the connect task, retries and timers are spawned
    /// on the current runtime's handle.
    pub fn new(
        api: Option<Arc<XCloudApiClient>>,
        auth: Option<Arc<MicrosoftAuthService>>,
        transport: Arc<dyn Transport>,
    ) -> Result<Self> {
        let runtime = Handle::try_current().map_err(|_| {
            Error::Other("SignalingOrchestrator::new requires a tokio runtime".to_owned())
        })?;
        let events = transport.events();
        let (event_tx, _) = broadcast::channel(EVENT_CAPACITY);
        let shared = Arc::new(Shared {
            api,
            auth,
            transport,
            runtime,
            input_queue: Arc::new(Mutex::new(InputQueue::new())),
            events: event_tx,
            state: Mutex::new(State::new()),
            peer_state: watch::Sender::new(PeerConnectionState::New),
            timers_changed: Notify::new(),
            clock_start: tokio::time::Instant::now(),
        });
        let (shutdown_tx, shutdown_rx) = unbounded::<()>();
        spawn_event_bridge(Arc::downgrade(&shared), events, shutdown_rx)?;
        Ok(Self {
            shared,
            _shutdown: shutdown_tx,
        })
    }

    pub fn set_config(&self, config: StreamingConfig) {
        self.shared.state.lock().config = config;
    }

    pub fn config(&self) -> StreamingConfig {
        self.shared.state.lock().config.clone()
    }

    pub fn set_preferences(&self, preferences: StreamPreferences) {
        self.shared.state.lock().preferences = preferences;
    }

    pub fn preferences(&self) -> StreamPreferences {
        self.shared.state.lock().preferences.clone()
    }

    /// `connectStream`: begins the startup sequence. Progress is reported through
    /// [`OrchestratorEvent::Lifecycle`]. Ignored while a connection is in progress.
    pub fn connect_stream(&self, kind: StreamKind, target_id: &str, msa_user_token: &str) {
        {
            let mut state = self.shared.state.lock();
            if matches!(
                state.lifecycle,
                StreamLifecycleState::StartingSession
                    | StreamLifecycleState::Provisioning
                    | StreamLifecycleState::ConnectingWebRTC
            ) {
                tracing::warn!(
                    target: LOG,
                    "connectStream ignored: a connection is already in progress"
                );
                return;
            }
            state.kind = kind;
            state.target_id = target_id.to_owned();
            state.user_token = msa_user_token.to_owned();
            state.attempt = 0;
        }
        self.shared.start_attempt();
    }

    /// `disconnect(intent)`: tears down the active session. Idempotent.
    pub fn disconnect(&self, intent: DisconnectIntent) {
        self.shared.disconnect(intent);
    }

    /// `cancelPendingConnect`: cancels the current connection attempt (used when
    /// leaving the stream view).
    pub fn cancel_pending_connect(&self) {
        self.shared.state.lock().cancelled = true;
    }

    pub fn lifecycle(&self) -> StreamLifecycleState {
        self.shared.state.lock().lifecycle
    }

    pub fn stats(&self) -> StreamingStatsSnapshot {
        self.shared.state.lock().stats.clone()
    }

    pub fn last_error(&self) -> StreamError {
        self.shared.state.lock().last_error.clone()
    }

    pub fn disconnect_intent(&self) -> DisconnectIntent {
        self.shared.state.lock().disconnect_intent
    }

    /// `m_attempt`: the number of the attempt in progress (or last run).
    pub fn attempt(&self) -> u32 {
        self.shared.state.lock().attempt
    }

    /// `inputQueue()`: shared with the input crate, which enqueues controller frames.
    pub fn input_queue(&self) -> SharedInputQueue {
        Arc::clone(&self.shared.input_queue)
    }

    /// `bridge()`.
    pub fn transport(&self) -> Arc<dyn Transport> {
        Arc::clone(&self.shared.transport)
    }

    /// `session()`.
    pub fn session(&self) -> Option<Arc<StreamSession>> {
        self.shared.state.lock().session.clone()
    }

    pub fn auth(&self) -> Option<Arc<MicrosoftAuthService>> {
        self.shared.auth.clone()
    }

    pub fn api(&self) -> Option<Arc<XCloudApiClient>> {
        self.shared.api.clone()
    }

    /// True while the input channel's flush loop is running.
    pub fn input_running(&self) -> bool {
        self.shared
            .state
            .lock()
            .input
            .as_ref()
            .is_some_and(InputChannel::is_running)
    }

    /// The signal stream. Each receiver sees every event emitted after it subscribed.
    pub fn subscribe(&self) -> broadcast::Receiver<OrchestratorEvent> {
        self.shared.events.subscribe()
    }

    /// `setGamepadConnectionState`.
    pub fn set_gamepad_connection_state(&self, index: i32, connected: bool) {
        let mut state = self.shared.state.lock();
        state.desired_gamepad_state.insert(index, connected);
        if !state.control_startup_completed {
            return;
        }
        if let Some(control) = state.control.as_ref() {
            let actions = control.send_gamepad_changed(index, connected);
            self.shared.execute(&mut state, actions);
        }
    }

    /// `setDiagnosticsPollingEnabled`.
    pub fn set_diagnostics_polling_enabled(&self, enabled: bool) {
        let mut state = self.shared.state.lock();
        state.diagnostics_polling = enabled;
        if enabled {
            self.shared.start_stats_polling(&mut state);
        } else {
            self.shared.stop_stats_polling(&mut state);
        }
    }

    /// Feeds a signal from the media pipeline / renderer.
    pub fn signal(&self, signal: RuntimeSignal) {
        match signal {
            RuntimeSignal::VideoRecoveryNeeded => self.shared.video_recovery_needed(),
            RuntimeSignal::DecodedResolution { width, height } => {
                self.shared.decoded_resolution(width, height)
            }
        }
    }

    /// Delivers one transport event synchronously, exactly as the Qt direct
    /// connections ran the orchestrator's slots. The event bridge thread calls this
    /// for everything the transport emits; tests may call it directly.
    pub fn handle_transport_event(&self, event: TransportEvent) {
        self.shared.handle_transport_event(event);
    }
}

impl Drop for SignalingOrchestrator {
    fn drop(&mut self) {
        // Background tasks hold `Arc<Shared>`; stop them so the state is released.
        let mut state = self.shared.state.lock();
        state.tasks.abort_all();
        if let Some(session) = state.session.take() {
            session.stop_keepalive();
        }
    }
}

/// Bridges the transport's crossbeam receiver onto the orchestrator's handlers.
fn spawn_event_bridge(
    shared: std::sync::Weak<Shared>,
    events: Receiver<TransportEvent>,
    shutdown: Receiver<()>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("xcloud-orchestrator-events".to_owned())
        .spawn(move || {
            loop {
                crossbeam_channel::select! {
                    recv(events) -> event => {
                        let Ok(event) = event else { break };
                        let Some(shared) = shared.upgrade() else { break };
                        shared.handle_transport_event(event);
                    }
                    recv(shutdown) -> _ => break,
                }
            }
        })
        .map(|_| ())
        .map_err(|error| Error::Other(format!("spawning the transport event bridge: {error}")))
}

// ---------------------------------------------------------------------------
// StreamController

/// The subset of `core/Settings` the stream controller reads. It lives here rather than in
/// the app crate because `xcloud-app` depends on this crate (not the other way
/// round): the host converts its own `Settings` into this value, as `StreamController::
/// refreshPreferences` read them (see the app's `stream_settings()`).
#[derive(Debug, Clone, PartialEq)]
pub struct StreamSettings {
    /// `Settings::resolutionProfile()` ("auto", "p720", "p1080", "p1440", "p1080HQ").
    pub resolution_profile: String,
    pub locale: String,
    pub prefer_ipv6: bool,
    pub preferred_region: String,
    pub preferred_frame_rate: u32,
    pub hud_enabled: bool,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            resolution_profile: "p1080".to_owned(),
            locale: "en-US".to_owned(),
            prefer_ipv6: false,
            preferred_region: String::new(),
            preferred_frame_rate: 60,
            hud_enabled: false,
        }
    }
}

struct ControllerState {
    lifecycle_state: StreamLifecycleState,
    status_text: String,
    title_id: String,
    title_name: String,
    hero_image: String,
    error: String,
    active: bool,
    /// `GamepadInputSource::active`: controller polling runs only while connected.
    input_active: bool,
    wait_seconds: i32,
    stats: StreamingStatsSnapshot,
    stream_ended: bool,
    lpt_task: Option<JoinHandle<()>>,
}

/// UI-facing stream state: lifecycle text, performance overlay values and the
/// commands the stream view needs. Mirrors the Swift StreamController.
///
/// The QML-only members (frame provider, hero image provider, audio output) are not
/// ported; frames flow through the media pipeline and the GPU crate directly. The
/// host drains the orchestrator's events with [`StreamController::poll_events`] once
/// per frame (the queued-connection hop to the GUI thread).
pub struct StreamController {
    auth: Option<Arc<MicrosoftAuthService>>,
    api: Option<Arc<XCloudApiClient>>,
    orchestrator: Arc<SignalingOrchestrator>,
    settings: Mutex<StreamSettings>,
    state: Arc<Mutex<ControllerState>>,
    events: Mutex<broadcast::Receiver<OrchestratorEvent>>,
    runtime: Handle,
}

impl std::fmt::Debug for StreamController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock();
        f.debug_struct("StreamController")
            .field("lifecycle", &state.lifecycle_state)
            .field("active", &state.active)
            .field("title_id", &state.title_id)
            .finish_non_exhaustive()
    }
}

impl StreamController {
    /// `StreamController(auth, api, bridge, settings)`.
    pub fn new(
        auth: Option<Arc<MicrosoftAuthService>>,
        api: Option<Arc<XCloudApiClient>>,
        transport: Arc<dyn Transport>,
        settings: StreamSettings,
    ) -> Result<Self> {
        let orchestrator = Arc::new(SignalingOrchestrator::new(
            api.clone(),
            auth.clone(),
            transport,
        )?);
        let events = orchestrator.subscribe();
        let controller = Self {
            auth,
            api,
            runtime: Handle::try_current().map_err(|_| {
                Error::Other("StreamController::new requires a tokio runtime".to_owned())
            })?,
            orchestrator,
            settings: Mutex::new(settings),
            state: Arc::new(Mutex::new(ControllerState {
                lifecycle_state: StreamLifecycleState::Idle,
                status_text: String::new(),
                title_id: String::new(),
                title_name: String::new(),
                hero_image: String::new(),
                error: String::new(),
                active: false,
                input_active: false,
                wait_seconds: 0,
                stats: StreamingStatsSnapshot::default(),
                stream_ended: false,
                lpt_task: None,
            })),
            events: Mutex::new(events),
        };
        controller.refresh_preferences();
        Ok(controller)
    }

    pub fn orchestrator(&self) -> &Arc<SignalingOrchestrator> {
        &self.orchestrator
    }

    /// `lifecycle` property: the lifecycle name ("idle", "connected", ...).
    pub fn lifecycle(&self) -> &'static str {
        stream_lifecycle_name(self.state.lock().lifecycle_state)
    }

    pub fn lifecycle_state(&self) -> StreamLifecycleState {
        self.state.lock().lifecycle_state
    }

    pub fn status_text(&self) -> String {
        self.state.lock().status_text.clone()
    }

    pub fn title_id(&self) -> String {
        self.state.lock().title_id.clone()
    }

    pub fn title_name(&self) -> String {
        self.state.lock().title_name.clone()
    }

    pub fn hero_image(&self) -> String {
        self.state.lock().hero_image.clone()
    }

    pub fn error(&self) -> String {
        self.state.lock().error.clone()
    }

    pub fn active(&self) -> bool {
        self.state.lock().active
    }

    pub fn connected(&self) -> bool {
        self.state.lock().lifecycle_state == StreamLifecycleState::Connected
    }

    /// `input()->active()`: whether controller polling should run.
    pub fn input_active(&self) -> bool {
        self.state.lock().input_active
    }

    /// `input()->inputQueue()`: the queue the input source should enqueue into.
    pub fn input_queue(&self) -> SharedInputQueue {
        self.orchestrator.input_queue()
    }

    pub fn hud_enabled(&self) -> bool {
        self.settings.lock().hud_enabled
    }

    pub fn set_hud_enabled(&self, enabled: bool) {
        self.settings.lock().hud_enabled = enabled;
    }

    pub fn settings(&self) -> StreamSettings {
        self.settings.lock().clone()
    }

    /// Replaces the settings; call [`StreamController::apply_settings`] to push them.
    pub fn set_settings(&self, settings: StreamSettings) {
        *self.settings.lock() = settings;
    }

    pub fn stats(&self) -> StreamingStatsSnapshot {
        self.state.lock().stats.clone()
    }

    pub fn resolution_height(&self) -> i32 {
        self.state.lock().stats.negotiated_height
    }

    pub fn resolution_width(&self) -> i32 {
        self.state.lock().stats.negotiated_width
    }

    pub fn frames_per_second(&self) -> f64 {
        self.state.lock().stats.frames_per_second
    }

    pub fn bitrate_mbps(&self) -> f64 {
        f64::from(self.state.lock().stats.bitrate_kbps) / 1000.0
    }

    pub fn round_trip_ms(&self) -> f64 {
        self.state.lock().stats.round_trip_time_ms
    }

    pub fn packets_lost(&self) -> i32 {
        self.state.lock().stats.packets_lost
    }

    pub fn renderer_mode(&self) -> String {
        self.state.lock().stats.renderer_mode.clone()
    }

    /// `nativeVideo`: true when the transport owns the media pipeline (native transport path).
    pub fn native_video(&self) -> bool {
        self.orchestrator.transport().rtcp_sender().is_some()
    }

    /// The `streamEnded` signal, latched until read.
    pub fn take_stream_ended(&self) -> bool {
        std::mem::take(&mut self.state.lock().stream_ended)
    }

    /// `refreshPreferences`.
    fn refresh_preferences(&self) {
        let settings = self.settings.lock().clone();
        let preferences = StreamPreferences {
            resolution: stream_resolution_from_string(
                &settings.resolution_profile,
                StreamResolutionMode::P1080,
            ),
            locale: settings.locale.clone(),
            prefer_ipv6: settings.prefer_ipv6,
            preferred_region_id: settings.preferred_region.clone(),
            ..StreamPreferences::default()
        };
        self.orchestrator.set_preferences(preferences.clone());

        if let Some(api) = &self.api {
            api.set_preferences(&preferences);
            api.update_credentials(&api.base_host(), &api.gs_token());
        }

        let mut config = StreamingConfig::defaults();
        config.preferred_frame_rate = settings.preferred_frame_rate;
        config.requested_video_max_frame_rate = settings.preferred_frame_rate;
        let profile = quality_tier_profile(preferences.resolution);
        config.video_width_hint = profile.width;
        config.video_height_hint = profile.height;
        config.max_bitrate_kbps = profile.max_bitrate_kbps;
        config.max_video_bitrate_kbps = profile.max_bitrate_kbps;
        self.orchestrator.set_config(config);
    }

    /// `applySettings`: re-applies live settings before the next launch.
    pub fn apply_settings(&self) {
        self.refresh_preferences();
    }

    fn set_lifecycle(state: &mut ControllerState, lifecycle: StreamLifecycleState) {
        state.input_active = lifecycle == StreamLifecycleState::Connected;
        state.lifecycle_state = lifecycle;
        Self::update_status_text(state);
    }

    fn update_status_text(state: &mut ControllerState) {
        let name = if state.title_name.is_empty() {
            "the game"
        } else {
            state.title_name.as_str()
        };
        let text = match state.lifecycle_state {
            StreamLifecycleState::Idle | StreamLifecycleState::StartingSession => {
                format!("Starting {name}")
            }
            StreamLifecycleState::Provisioning => "Provisioning a cloud console".to_owned(),
            StreamLifecycleState::WaitingForResources => {
                if state.wait_seconds > 0 {
                    format!("In queue, about {} min", state.wait_seconds / 60)
                } else {
                    "Waiting for a free console".to_owned()
                }
            }
            StreamLifecycleState::ReadyToConnect | StreamLifecycleState::ConnectingWebRTC => {
                "Connecting".to_owned()
            }
            StreamLifecycleState::Connected => "Waiting for video".to_owned(),
            StreamLifecycleState::Disconnecting | StreamLifecycleState::Disconnected => {
                "Disconnected".to_owned()
            }
            StreamLifecycleState::Failed => format!("Stream failed: {}", state.error),
        };
        state.status_text = text;
    }

    fn set_error(state: &mut ControllerState, error: String) {
        state.error = error;
    }

    /// Drains and handles every pending orchestrator event, returning them so the
    /// host can react too (vibration, video track). Call once per UI frame.
    pub fn poll_events(&self) -> Vec<OrchestratorEvent> {
        let mut drained = Vec::new();
        loop {
            let next = self.events.lock().try_recv();
            match next {
                Ok(event) => {
                    self.handle_event(event.clone());
                    drained.push(event);
                }
                Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                    tracing::warn!(target: CONTROLLER_LOG, "dropped {skipped} orchestrator events");
                }
                Err(_) => break,
            }
        }
        drained
    }

    /// `wireOrchestrator`: the slots connected to the orchestrator's signals.
    pub fn handle_event(&self, event: OrchestratorEvent) {
        match event {
            OrchestratorEvent::Lifecycle(lifecycle) => {
                let mut state = self.state.lock();
                Self::set_lifecycle(&mut state, lifecycle);
                if matches!(
                    lifecycle,
                    StreamLifecycleState::Disconnected | StreamLifecycleState::Failed
                ) {
                    state.input_active = false;
                }
            }
            OrchestratorEvent::StatsChanged(stats) => {
                self.state.lock().stats = stats;
            }
            OrchestratorEvent::WaitingForResources(seconds) => {
                let mut state = self.state.lock();
                state.wait_seconds = seconds;
                Self::update_status_text(&mut state);
            }
            OrchestratorEvent::Error(error) => {
                let mut state = self.state.lock();
                Self::set_error(&mut state, error.description());
                Self::update_status_text(&mut state);
            }
            OrchestratorEvent::ServerInitiatedDisconnect => self.stop_streaming(),
            OrchestratorEvent::VideoTrackAvailable | OrchestratorEvent::Vibration(_) => {}
        }
    }

    /// `startCloudStream`: launches a cloud title. Progress is reported through the
    /// lifecycle property.
    pub fn start_cloud_stream(&self, title_id: &str, title_name: &str, hero_image: &str) {
        {
            let mut state = self.state.lock();
            if state.active {
                tracing::warn!(
                    target: CONTROLLER_LOG,
                    "startCloudStream ignored: a stream is already active"
                );
                return;
            }
            state.title_id = title_id.to_owned();
            state.title_name = title_name.to_owned();
            state.hero_image = hero_image.to_owned();
            state.error.clear();
            state.wait_seconds = 0;
            state.stats = StreamingStatsSnapshot::default();
            state.active = true;
        }

        self.refresh_preferences();
        self.orchestrator.set_diagnostics_polling_enabled(true);
        // One logical Xbox controller supports both physical and keyboard input.
        self.orchestrator.set_gamepad_connection_state(0, true);

        let mut state = self.state.lock();
        state.input_active = false;
        Self::set_lifecycle(&mut state, StreamLifecycleState::StartingSession);

        let Some(auth) = self.auth.clone() else {
            Self::set_error(&mut state, "Authentication service unavailable".to_owned());
            Self::set_lifecycle(&mut state, StreamLifecycleState::Failed);
            return;
        };

        let shared_state = Arc::clone(&self.state);
        let orchestrator = Arc::clone(&self.orchestrator);
        let title_id = title_id.to_owned();
        let task = self.runtime.spawn(async move {
            let result = auth.fetch_lpt_for_cloud_connect().await;
            {
                let mut state = shared_state.lock();
                if !state.active {
                    return;
                }
                match &result {
                    Ok(_) => {}
                    Err(error) => {
                        // Without a cloud transfer token the /connect step cannot run.
                        Self::set_error(&mut state, error.to_string());
                        Self::set_lifecycle(&mut state, StreamLifecycleState::Failed);
                        return;
                    }
                }
            }
            if let Ok(token) = result {
                orchestrator.connect_stream(StreamKind::Cloud, &title_id, &token);
            }
        });
        replace_task(&mut state.lpt_task, Some(task));
    }

    /// `stopStreaming`: stops the active stream and returns to the library.
    pub fn stop_streaming(&self) {
        {
            let mut state = self.state.lock();
            if !state.active {
                return;
            }
            state.input_active = false;
            replace_task(&mut state.lpt_task, None);
        }
        self.orchestrator.cancel_pending_connect();
        self.orchestrator.set_diagnostics_polling_enabled(false);
        self.orchestrator
            .disconnect(DisconnectIntent::UserInitiated);
        let mut state = self.state.lock();
        state.active = false;
        Self::set_lifecycle(&mut state, StreamLifecycleState::Disconnected);
        state.stream_ended = true;
    }
}

impl Drop for StreamController {
    fn drop(&mut self) {
        replace_task(&mut self.state.lock().lpt_task, None);
    }
}

#[cfg(test)]
mod tests {
    //! Port of qt/tests/tst_streamcontroller.cpp. Ported: inputWaitsForHandshake (all
    //! three data rows), decodedResolutionOverridesServerMetadata and
    //! pollingFollowsStreamLifecycle. Not applicable (QML engine / QImage provider
    //! specific): framesSurviveEngineOwnershipAndTeardown, bufferCanBeClearedAfterTransfer.
    //!
    //! Where the C++ passed `nullptr` for the API client the tests pass `None`; the
    //! full `runConnect` path is exercised against `xcloud_api::transport::FakeTransport`.
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::Value;
    use xcloud_api::transport::{FakeTransport as FakeHttp, Reply};
    use xcloud_proto::preferences::StreamResolutionMode;
    use xcloud_webrtc::fake::FakeTransport;

    use super::*;

    fn orchestrator(fake: &Arc<FakeTransport>) -> SignalingOrchestrator {
        SignalingOrchestrator::new(None, None, Arc::clone(fake) as Arc<dyn Transport>)
            .expect("orchestrator")
    }

    fn json(text: &str) -> Value {
        serde_json::from_str(text).unwrap_or(Value::Null)
    }

    fn string_field(value: &Value, key: &str) -> String {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    fn int_field(value: &Value, key: &str) -> i64 {
        value.get(key).and_then(Value::as_i64).unwrap_or_default()
    }

    /// `inputWaitsForHandshake` for one data row.
    fn input_waits_for_handshake(ack_first: bool, use_1440: bool) {
        let fake = Arc::new(FakeTransport::new());
        let orchestrator = orchestrator(&fake);
        let preferences = StreamPreferences {
            resolution: if use_1440 {
                StreamResolutionMode::P1440
            } else {
                StreamResolutionMode::P1080
            },
            ..StreamPreferences::default()
        };
        orchestrator.set_preferences(preferences);
        orchestrator.set_gamepad_connection_state(0, true);
        // Channel setup happens before the absent API terminates provisioning.
        // Drive transport events directly without a network session.
        orchestrator.connect_stream(StreamKind::Cloud, "test", "");
        assert_eq!(orchestrator.lifecycle(), StreamLifecycleState::Failed);
        assert_eq!(orchestrator.last_error().code, StreamErrorCode::Unsupported);

        let open = |kind| orchestrator.handle_transport_event(TransportEvent::ChannelOpened(kind));
        let ack = || {
            orchestrator.handle_transport_event(TransportEvent::TextReceived {
                kind: DataChannelKind::Message,
                text: r#"{"type":"HandshakeAck"}"#.to_owned(),
            })
        };

        open(DataChannelKind::Message);
        if ack_first {
            ack();
        } else {
            open(DataChannelKind::Control);
            open(DataChannelKind::Input);
        }
        for entry in fake.sent() {
            assert!(
                entry.kind != DataChannelKind::Control && entry.kind != DataChannelKind::Input,
                "nothing may be sent on control/input before handshake + channels: {entry:?}"
            );
        }
        if ack_first {
            open(DataChannelKind::Input);
            open(DataChannelKind::Control);
        } else {
            ack();
        }

        let mut control_messages = Vec::new();
        let (mut metadata, mut capabilities, mut dimensions) = (0, 0, 0);
        let width = if use_1440 { 2560 } else { 1920 };
        let height = if use_1440 { 1440 } else { 1080 };
        for entry in fake.sent() {
            if entry.kind == DataChannelKind::Input {
                metadata += 1;
                assert!(entry.is_binary());
            }
            if entry.kind == DataChannelKind::Message {
                assert!(entry.is_text());
                let envelope = json(&entry.text());
                let content = json(&string_field(&envelope, "content"));
                let target = string_field(&envelope, "target");
                if target == "/streaming/characteristics/clientdevicecapabilities" {
                    capabilities += 1;
                    assert_eq!(int_field(&content, "maxWidth"), width);
                    assert_eq!(int_field(&content, "maxHeight"), height);
                    assert_eq!(
                        int_field(&content, "maxBitrateKbps"),
                        if use_1440 { 30000 } else { 20000 }
                    );
                    assert_eq!(int_field(&content, "supportsFps"), 60);
                    assert_eq!(content["supportsHevc"], Value::Bool(false));
                    assert_eq!(content["supportsHdr"], Value::Bool(false));
                    assert_eq!(int_field(&content["video"], "width"), width);
                    assert_eq!(int_field(&content["video"], "maxHeight"), height);
                }
                if target == "/streaming/characteristics/dimensionschanged" {
                    dimensions += 1;
                    assert_eq!(int_field(&content, "horizontal"), width);
                    assert_eq!(int_field(&content, "vertical"), height);
                }
            }
            if entry.kind != DataChannelKind::Control {
                assert!(!entry.text().contains("userRequestedResolutionUpdate"));
                continue;
            }
            assert!(entry.is_text());
            let object = json(&entry.text());
            let message = string_field(&object, "message");
            if message == "userRequestedResolutionUpdate" {
                assert_eq!(
                    string_field(&object, "resolutionAlias"),
                    if use_1440 { "1440" } else { "1080" }
                );
            }
            if message == "gamepadChanged" {
                assert_eq!(object["wasAdded"], Value::Bool(true));
            }
            control_messages.push(message);
        }
        assert_eq!(
            control_messages.first().map(String::as_str),
            Some("authorizationRequest")
        );
        assert_eq!(
            control_messages
                .iter()
                .filter(|m| *m == "gamepadChanged")
                .count(),
            1
        );
        assert_eq!(
            control_messages
                .iter()
                .filter(|m| *m == "userRequestedResolutionUpdate")
                .count(),
            1
        );
        let index_of = |needle: &str| control_messages.iter().position(|m| m == needle);
        assert!(index_of("userRequestedResolutionUpdate") > index_of("gamepadChanged"));
        assert_eq!(metadata, 1);
        assert_eq!(capabilities, 1);
        assert_eq!(dimensions, 1);
        assert!(orchestrator.input_running());

        let count = fake.sent_count();
        ack();
        open(DataChannelKind::Control);
        open(DataChannelKind::Input);
        assert_eq!(fake.sent_count(), count);
    }

    #[tokio::test(start_paused = true)]
    async fn input_waits_for_handshake_channels_before_ack() {
        input_waits_for_handshake(false, false);
    }

    #[tokio::test(start_paused = true)]
    async fn input_waits_for_handshake_ack_before_channels() {
        input_waits_for_handshake(true, false);
    }

    #[tokio::test(start_paused = true)]
    async fn input_waits_for_handshake_1440p() {
        input_waits_for_handshake(false, true);
    }

    /// `QTRY_COMPARE` over virtual time: lets the 1 s stats timer fire.
    async fn try_until(mut condition: impl FnMut() -> bool) -> bool {
        for _ in 0..10 {
            if condition() {
                return true;
            }
            tokio::time::sleep(STATS_INTERVAL).await;
            tokio::task::yield_now().await;
        }
        condition()
    }

    #[tokio::test(start_paused = true)]
    async fn decoded_resolution_overrides_server_metadata() {
        let fake = Arc::new(FakeTransport::new());
        let orchestrator = orchestrator(&fake);
        orchestrator.connect_stream(StreamKind::Cloud, "test", "");
        // ServerMetadata: type=16, height=1080, width=1920, little endian.
        orchestrator.handle_transport_event(TransportEvent::DataReceived {
            kind: DataChannelKind::Input,
            data: vec![0x10, 0x00, 0x38, 0x04, 0x00, 0x00, 0x80, 0x07, 0x00, 0x00],
        });
        orchestrator.set_diagnostics_polling_enabled(true);
        assert!(try_until(|| orchestrator.stats().negotiated_height == 1080).await);
        assert_eq!(orchestrator.stats().negotiated_width, 1920);
        assert_eq!(orchestrator.stats().renderer_mode, "webrtc");

        fake.set_stats(StreamingStatsSnapshot {
            negotiated_width: 2560,
            negotiated_height: 1440,
            ..StreamingStatsSnapshot::default()
        });
        assert!(try_until(|| orchestrator.stats().negotiated_height == 1440).await);
        assert_eq!(orchestrator.stats().negotiated_width, 2560);

        // A real resolution change in the decoded stream must still be shown.
        fake.set_stats(StreamingStatsSnapshot {
            negotiated_width: 1280,
            negotiated_height: 720,
            ..StreamingStatsSnapshot::default()
        });
        assert!(try_until(|| orchestrator.stats().negotiated_height == 720).await);
        orchestrator.set_diagnostics_polling_enabled(false);

        // A host-fed decoded size is only folded in by the stats timer slot: while
        // polling is off it neither changes `stats()` nor emits `statsChanged`
        // (the C++ has no such path; the pipeline wrote into the bridge snapshot).
        let mut events = orchestrator.subscribe();
        orchestrator.signal(RuntimeSignal::DecodedResolution {
            width: 3840,
            height: 2160,
        });
        assert_eq!(orchestrator.stats().negotiated_width, 1280);
        assert_eq!(orchestrator.stats().negotiated_height, 720);
        assert!(events.try_recv().is_err());

        // With polling back on, the transport's collectStats dimensions stay
        // authoritative over the host-fed size whenever they are > 0 ...
        orchestrator.set_diagnostics_polling_enabled(true);
        tokio::time::sleep(STATS_INTERVAL * 2).await;
        tokio::task::yield_now().await;
        assert_eq!(orchestrator.stats().negotiated_width, 1280);
        assert_eq!(orchestrator.stats().negotiated_height, 720);
        // ... and a later transport-reported change is never masked by it.
        fake.set_stats(StreamingStatsSnapshot {
            negotiated_width: 1920,
            negotiated_height: 1080,
            ..StreamingStatsSnapshot::default()
        });
        assert!(try_until(|| orchestrator.stats().negotiated_width == 1920).await);
        assert_eq!(orchestrator.stats().negotiated_height, 1080);

        // When the transport reports no dimensions (decoder outside the transport),
        // the host-fed decoded size wins over the server metadata ...
        fake.set_stats(StreamingStatsSnapshot::default());
        assert!(try_until(|| orchestrator.stats().negotiated_height == 2160).await);
        assert_eq!(orchestrator.stats().negotiated_width, 3840);
        // ... and tracks later decoded size changes too.
        orchestrator.signal(RuntimeSignal::DecodedResolution {
            width: 2560,
            height: 1440,
        });
        assert!(try_until(|| orchestrator.stats().negotiated_height == 1440).await);
        assert_eq!(orchestrator.stats().negotiated_width, 2560);

        // Without either, the input-channel server metadata (1920x1080) is the fallback.
        orchestrator.signal(RuntimeSignal::DecodedResolution {
            width: 0,
            height: 0,
        });
        assert!(try_until(|| orchestrator.stats().negotiated_height == 1080).await);
        assert_eq!(orchestrator.stats().negotiated_width, 1920);
        orchestrator.set_diagnostics_polling_enabled(false);
    }

    #[tokio::test(start_paused = true)]
    async fn polling_follows_stream_lifecycle() {
        let fake = Arc::new(FakeTransport::unavailable());
        let controller = StreamController::new(
            None,
            None,
            Arc::clone(&fake) as Arc<dyn Transport>,
            StreamSettings::default(),
        )
        .expect("controller");
        assert!(!controller.input_active());
        controller.handle_event(OrchestratorEvent::Lifecycle(
            StreamLifecycleState::Connected,
        ));
        assert!(controller.input_active());
        assert_eq!(controller.lifecycle(), "connected");
        assert!(controller.connected());
        assert_eq!(controller.status_text(), "Waiting for video");
        controller.handle_event(OrchestratorEvent::Lifecycle(
            StreamLifecycleState::Disconnected,
        ));
        assert!(!controller.input_active());
        assert_eq!(controller.lifecycle(), "disconnected");

        // Events emitted by the orchestrator itself arrive through poll_events.
        controller
            .orchestrator()
            .connect_stream(StreamKind::Cloud, "test", "");
        let drained = controller.poll_events();
        assert!(drained.contains(&OrchestratorEvent::Lifecycle(StreamLifecycleState::Failed)));
        assert_eq!(controller.lifecycle(), "failed");
        assert_eq!(
            controller.status_text(),
            "Stream failed: unsupported: Streaming is not configured"
        );
        assert!(!controller.input_active());
    }

    #[tokio::test(start_paused = true)]
    async fn start_cloud_stream_without_auth_fails_and_stop_tears_down() {
        let fake = Arc::new(FakeTransport::new());
        let controller = StreamController::new(
            None,
            None,
            Arc::clone(&fake) as Arc<dyn Transport>,
            StreamSettings {
                resolution_profile: "p1440".to_owned(),
                preferred_frame_rate: 30,
                ..StreamSettings::default()
            },
        )
        .expect("controller");
        assert_eq!(
            controller.orchestrator().preferences().resolution,
            StreamResolutionMode::P1440
        );
        assert_eq!(controller.orchestrator().config().preferred_frame_rate, 30);
        assert_eq!(controller.orchestrator().config().max_bitrate_kbps, 30000);

        controller.start_cloud_stream("9PABC", "Halo", "hero.png");
        assert!(controller.active());
        assert_eq!(controller.lifecycle(), "failed");
        assert_eq!(controller.error(), "Authentication service unavailable");
        assert_eq!(
            controller.status_text(),
            "Stream failed: Authentication service unavailable"
        );
        assert_eq!(controller.title_name(), "Halo");
        // A second start while active is ignored.
        controller.start_cloud_stream("other", "", "");
        assert_eq!(controller.title_id(), "9PABC");

        controller.stop_streaming();
        assert!(!controller.active());
        assert_eq!(controller.lifecycle(), "disconnected");
        assert!(controller.take_stream_ended());
        assert!(!controller.take_stream_ended());
        assert_eq!(fake.close_count(), 1);
        assert_eq!(
            controller.orchestrator().lifecycle(),
            StreamLifecycleState::Disconnected
        );
        assert_eq!(
            controller.orchestrator().disconnect_intent(),
            DisconnectIntent::UserInitiated
        );
    }

    #[tokio::test(start_paused = true)]
    async fn video_recovery_requests_keyframe_only_after_control_startup() {
        let fake = Arc::new(FakeTransport::new());
        let orchestrator = orchestrator(&fake);
        orchestrator.connect_stream(StreamKind::Cloud, "test", "");
        orchestrator.signal(RuntimeSignal::VideoRecoveryNeeded);
        assert!(fake.sent_on(DataChannelKind::Control).is_empty());

        orchestrator
            .handle_transport_event(TransportEvent::ChannelOpened(DataChannelKind::Message));
        orchestrator
            .handle_transport_event(TransportEvent::ChannelOpened(DataChannelKind::Control));
        orchestrator.handle_transport_event(TransportEvent::TextReceived {
            kind: DataChannelKind::Message,
            text: r#"{"type":"HandshakeAck"}"#.to_owned(),
        });
        let before = fake.sent_on(DataChannelKind::Control).len();
        orchestrator.signal(RuntimeSignal::VideoRecoveryNeeded);
        let control = fake.sent_on(DataChannelKind::Control);
        assert_eq!(control.len(), before + 1);
        assert_eq!(
            control.last().map(Sent::text),
            Some(r#"{"ifrRequested":true,"message":"videoKeyframeRequested"}"#.to_owned())
        );

        // Late gamepad changes go straight out once the control startup completed.
        orchestrator.set_gamepad_connection_state(1, false);
        let last = fake
            .sent_on(DataChannelKind::Control)
            .pop()
            .map(|s| s.text());
        assert_eq!(
            last,
            Some(r#"{"gamepadIndex":1,"message":"gamepadChanged","wasAdded":false}"#.to_owned())
        );

        // Vibration reports and server-initiated disconnects become events.
        let mut events = orchestrator.subscribe();
        orchestrator.handle_transport_event(TransportEvent::TextReceived {
            kind: DataChannelKind::Message,
            text: r#"{"type":"TransactionStart","id":"tx-1","target":"/streaming/sessionLifetimeManagement/serverInitiatedDisconnect","content":"{}"}"#.to_owned(),
        });
        assert_eq!(
            events.try_recv(),
            Ok(OrchestratorEvent::ServerInitiatedDisconnect)
        );
        // The transaction was acknowledged on the message channel.
        let last = fake
            .sent_on(DataChannelKind::Message)
            .pop()
            .map(|s| s.text());
        assert_eq!(
            last,
            Some(r#"{"content":"","cv":"","id":"tx-1","type":"TransactionComplete"}"#.to_owned())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn input_loop_flushes_queued_frames_at_125hz() {
        let fake = Arc::new(FakeTransport::new());
        let orchestrator = orchestrator(&fake);
        orchestrator.connect_stream(StreamKind::Cloud, "test", "");
        orchestrator
            .handle_transport_event(TransportEvent::ChannelOpened(DataChannelKind::Message));
        orchestrator
            .handle_transport_event(TransportEvent::ChannelOpened(DataChannelKind::Control));
        orchestrator.handle_transport_event(TransportEvent::ChannelOpened(DataChannelKind::Input));
        orchestrator.handle_transport_event(TransportEvent::TextReceived {
            kind: DataChannelKind::Message,
            text: r#"{"type":"HandshakeAck"}"#.to_owned(),
        });
        assert_eq!(
            fake.sent_on(DataChannelKind::Input).len(),
            1,
            "ClientMetadata"
        );

        // Two frames queued inside one 8 ms window coalesce into one packet.
        {
            let queue = orchestrator.input_queue();
            let mut queue = queue.lock();
            queue.enqueue_gamepad_frame(xcloud_proto::input::models::GamepadInputFrame::default());
            queue.enqueue_gamepad_frame(xcloud_proto::input::models::GamepadInputFrame::default());
        }
        tokio::time::sleep(Duration::from_millis(InputChannel::LOOP_INTERVAL_MS + 1)).await;
        tokio::task::yield_now().await;
        assert_eq!(fake.sent_on(DataChannelKind::Input).len(), 2);

        // Nothing queued: the loop ticks but sends nothing.
        tokio::time::sleep(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        assert_eq!(fake.sent_on(DataChannelKind::Input).len(), 2);

        // The control keyframe timer fires every 5 s after the open sequence.
        let control_before = fake.sent_on(DataChannelKind::Control).len();
        tokio::time::sleep(Duration::from_millis(5_100)).await;
        tokio::task::yield_now().await;
        let control = fake.sent_on(DataChannelKind::Control);
        assert_eq!(control.len(), control_before + 1);
        assert!(
            control
                .last()
                .map(Sent::text)
                .unwrap_or_default()
                .contains("videoKeyframeRequested")
        );

        // Disconnect stops the loop: no more sends afterwards.
        orchestrator.disconnect(DisconnectIntent::UserInitiated);
        assert!(!orchestrator.input_running());
        let count = fake.sent_count();
        orchestrator
            .input_queue()
            .lock()
            .enqueue_gamepad_frame(xcloud_proto::input::models::GamepadInputFrame::default());
        tokio::time::sleep(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        assert_eq!(fake.sent_count(), count);
        assert_eq!(orchestrator.lifecycle(), StreamLifecycleState::Disconnected);
    }

    use xcloud_webrtc::fake::Sent;

    const PLAY_RESPONSE: &str =
        r#"{"sessionPath":"/v5/sessions/cloud/ABCD","sessionId":"ABCD","state":"Provisioning"}"#;
    const ICE_RESPONSE: &str = r#"{"exchangeResponse":[{"candidate":"candidate:1 1 UDP 2130706431 10.0.0.1 9 typ host","sdpMid":"0","sdpMLineIndex":0}]}"#;

    fn api_with(http: &Arc<FakeHttp>) -> Arc<XCloudApiClient> {
        let api =
            XCloudApiClient::new(Arc::clone(http) as Arc<dyn xcloud_api::transport::HttpTransport>);
        api.update_credentials("https://example.test", "gs-token");
        Arc::new(api)
    }

    /// Queues the replies of one successful cloud session (Provisioned without the
    /// ReadyToConnect/connect step).
    fn enqueue_happy_session(http: &FakeHttp) {
        http.enqueue_json("/play", PLAY_RESPONSE, 200);
        http.enqueue_json("/state", r#"{"state":"WaitingForResources"}"#, 200);
        http.enqueue_json(
            "/waittime",
            r#"{"estimatedTotalWaitTimeInSeconds":120}"#,
            200,
        );
        http.enqueue_json("/state", r#"{"state":"Provisioned"}"#, 200);
        http.enqueue_for(
            "POST",
            "/sdp",
            Reply::new(200, br#"{"sdp":"ANSWER"}"#.to_vec()),
        );
        http.enqueue_for("POST", "/ice", Reply::new(200, Vec::new()));
        http.enqueue_for(
            "GET",
            "/ice",
            Reply::new(200, ICE_RESPONSE.as_bytes().to_vec()),
        );
    }

    async fn wait_for_lifecycle(
        events: &mut broadcast::Receiver<OrchestratorEvent>,
        wanted: StreamLifecycleState,
    ) -> Vec<OrchestratorEvent> {
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        loop {
            let next = tokio::time::timeout_at(deadline, events.recv()).await;
            match next {
                Ok(Ok(event)) => {
                    let done = event == OrchestratorEvent::Lifecycle(wanted);
                    seen.push(event);
                    if done {
                        // Signals emitted right after the lifecycle change
                        // (errorOccurred, videoTrackAvailable) are already queued.
                        while let Ok(event) = events.try_recv() {
                            seen.push(event);
                        }
                        return seen;
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                _ => panic!("lifecycle {wanted:?} not reached; saw {seen:?}"),
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn run_connect_reaches_connected_over_fake_http() {
        let http = Arc::new(FakeHttp::new());
        enqueue_happy_session(&http);
        let api = api_with(&http);
        let fake = Arc::new(FakeTransport::new());
        fake.set_requires_candidates_before_remote_description(true);
        fake.set_local_candidates(vec![IceCandidatePayload::new(
            "candidate:7 1 UDP 2130706431 192.168.1.2 50000 typ host",
        )]);
        let orchestrator = SignalingOrchestrator::new(
            Some(Arc::clone(&api)),
            None,
            Arc::clone(&fake) as Arc<dyn Transport>,
        )
        .expect("orchestrator");
        orchestrator.set_diagnostics_polling_enabled(true);
        let mut events = orchestrator.subscribe();

        // The peer reports connected once the remote description is in.
        fake.set_connection_state_silently(PeerConnectionState::Connected);
        orchestrator.connect_stream(StreamKind::Cloud, "9PABC", "msa-token");
        assert_eq!(
            orchestrator.lifecycle(),
            StreamLifecycleState::StartingSession
        );

        let seen = wait_for_lifecycle(&mut events, StreamLifecycleState::Connected).await;
        let lifecycles: Vec<_> = seen
            .iter()
            .filter_map(|event| match event {
                OrchestratorEvent::Lifecycle(state) => Some(*state),
                _ => None,
            })
            .collect();
        assert_eq!(
            lifecycles,
            vec![
                StreamLifecycleState::StartingSession,
                StreamLifecycleState::Provisioning,
                StreamLifecycleState::WaitingForResources,
                StreamLifecycleState::Provisioning,
                StreamLifecycleState::ConnectingWebRTC,
                StreamLifecycleState::Connected,
            ]
        );
        assert!(seen.contains(&OrchestratorEvent::WaitingForResources(120)));
        assert!(seen.contains(&OrchestratorEvent::VideoTrackAvailable));
        assert_eq!(
            orchestrator.disconnect_intent(),
            DisconnectIntent::Reconnectable
        );
        assert_eq!(orchestrator.attempt(), 1);

        // Offer processed with the SdpProcessor (stereo) and set locally.
        let local = fake.local_description().expect("local description");
        assert_eq!(local.kind, "offer");
        assert!(local.sdp.contains("stereo=1"));
        let remote = fake.remote_description().expect("remote description");
        assert_eq!(remote.kind, "answer");
        assert_eq!(remote.sdp, "ANSWER");
        // Candidates-first backend: every candidate before the remote description.
        assert_eq!(fake.remote_candidates().len(), 1);
        assert_eq!(fake.candidates_added_before_remote_description(), 1);

        // The local candidate was tagged with the offer's ufrag and posted.
        let ice_post = http
            .requests()
            .into_iter()
            .find(|request| request.method == "POST" && request.url.ends_with("/ice"))
            .expect("ICE post");
        assert!(ice_post.body_text().contains("fakeufrag"));

        let session = orchestrator.session().expect("session");
        assert_eq!(session.session_path(), "/v5/sessions/cloud/ABCD");
        assert!(session.keepalive_running());

        // Stats polling was requested before connecting and starts on Connected.
        fake.set_stats(StreamingStatsSnapshot {
            negotiated_width: 1920,
            negotiated_height: 1080,
            frames_per_second: 59.9,
            ..StreamingStatsSnapshot::default()
        });
        assert!(try_until(|| orchestrator.stats().frames_per_second > 59.0).await);

        // Teardown: channels, keepalive, session delete (best effort), transport.
        http.enqueue_for(
            "DELETE",
            "/v5/sessions/cloud/ABCD",
            Reply::new(204, Vec::new()),
        );
        orchestrator.disconnect(DisconnectIntent::UserInitiated);
        assert_eq!(orchestrator.lifecycle(), StreamLifecycleState::Disconnected);
        assert!(orchestrator.session().is_none());
        assert!(!session.keepalive_running());
        assert_eq!(fake.close_count(), 1);
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert!(
            http.requests()
                .iter()
                .any(|request| request.method == "DELETE")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ready_to_connect_sends_msal_auth_and_waits_for_provisioned() {
        let http = Arc::new(FakeHttp::new());
        http.enqueue_json("/play", PLAY_RESPONSE, 200);
        http.enqueue_json("/state", r#"{"state":"ReadyToConnect"}"#, 200);
        http.enqueue_for("POST", "/connect", Reply::new(202, Vec::new()));
        http.enqueue_json("/state", r#"{"state":"Provisioned"}"#, 200);
        http.enqueue_for(
            "POST",
            "/sdp",
            Reply::new(200, br#"{"sdp":"ANSWER"}"#.to_vec()),
        );
        http.enqueue_for("POST", "/ice", Reply::new(200, Vec::new()));
        http.enqueue_for(
            "GET",
            "/ice",
            Reply::new(200, ICE_RESPONSE.as_bytes().to_vec()),
        );
        let api = api_with(&http);
        let fake = Arc::new(FakeTransport::new());
        let orchestrator =
            SignalingOrchestrator::new(Some(api), None, Arc::clone(&fake) as Arc<dyn Transport>)
                .expect("orchestrator");
        let mut events = orchestrator.subscribe();
        orchestrator.connect_stream(StreamKind::Cloud, "9PABC", "msa-token");
        // Chromium order: remote description first, then candidates; the peer only
        // reports connected a while later.
        let later = Arc::clone(&fake);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(750)).await;
            later.set_connection_state(PeerConnectionState::Connected);
        });
        let seen = wait_for_lifecycle(&mut events, StreamLifecycleState::Connected).await;
        assert!(seen.contains(&OrchestratorEvent::Lifecycle(
            StreamLifecycleState::ReadyToConnect
        )));
        let connect = http
            .requests()
            .into_iter()
            .find(|request| request.url.ends_with("/connect"))
            .expect("connect request");
        assert!(connect.body_text().contains("msa-token"));
        assert_eq!(fake.candidates_added_before_remote_description(), 0);
        assert_eq!(fake.remote_candidates().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn missing_msa_token_is_an_authentication_failure_without_retry() {
        let http = Arc::new(FakeHttp::new());
        http.enqueue_json("/play", PLAY_RESPONSE, 200);
        http.enqueue_json("/state", r#"{"state":"ReadyToConnect"}"#, 200);
        let api = api_with(&http);
        let fake = Arc::new(FakeTransport::new());
        let orchestrator =
            SignalingOrchestrator::new(Some(api), None, Arc::clone(&fake) as Arc<dyn Transport>)
                .expect("orchestrator");
        let mut events = orchestrator.subscribe();
        orchestrator.connect_stream(StreamKind::Cloud, "9PABC", "");
        let seen = wait_for_lifecycle(&mut events, StreamLifecycleState::Failed).await;
        let error = orchestrator.last_error();
        assert_eq!(error.code, StreamErrorCode::Authentication);
        assert_eq!(
            error.message,
            "Missing MSA token for xCloud /connect auth step"
        );
        assert!(seen.contains(&OrchestratorEvent::Error(error)));
        assert_eq!(orchestrator.attempt(), 1);
        assert_eq!(fake.offers_created(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn webrtc_failure_rerolls_the_session_once() {
        let http = Arc::new(FakeHttp::new());
        for _ in 0..2 {
            http.enqueue_json("/play", PLAY_RESPONSE, 200);
            http.enqueue_json("/state", r#"{"state":"Provisioned"}"#, 200);
        }
        let api = api_with(&http);
        let fake = Arc::new(FakeTransport::new());
        fake.set_offer_error(Some("peer_connection_create_offer failed"));
        let orchestrator =
            SignalingOrchestrator::new(Some(api), None, Arc::clone(&fake) as Arc<dyn Transport>)
                .expect("orchestrator");
        let mut events = orchestrator.subscribe();
        orchestrator.connect_stream(StreamKind::Cloud, "9PABC", "msa-token");
        let seen = wait_for_lifecycle(&mut events, StreamLifecycleState::Failed).await;

        // Two /play requests: the second attempt re-rolled the session.
        let plays = http
            .requests()
            .iter()
            .filter(|request| request.url.ends_with("/play"))
            .count();
        assert_eq!(plays, 2);
        assert_eq!(orchestrator.attempt(), 2);
        assert_eq!(fake.offers_created(), 2);
        assert_eq!(orchestrator.last_error().code, StreamErrorCode::WebRtc);
        // The first failure only reports Provisioning (retrying), not an error event.
        let errors = seen
            .iter()
            .filter(|event| matches!(event, OrchestratorEvent::Error(_)))
            .count();
        assert_eq!(errors, 1);
        let connecting = seen
            .iter()
            .filter(|event| {
                **event == OrchestratorEvent::Lifecycle(StreamLifecycleState::ConnectingWebRTC)
            })
            .count();
        assert_eq!(connecting, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_pending_connect_stops_the_attempt_quietly() {
        let http = Arc::new(FakeHttp::new());
        http.enqueue_json("/play", PLAY_RESPONSE, 200);
        http.enqueue_json("/state", r#"{"state":"Provisioning"}"#, 200);
        http.enqueue_json("/state", r#"{"state":"Provisioned"}"#, 200);
        let api = api_with(&http);
        let fake = Arc::new(FakeTransport::new());
        let orchestrator =
            SignalingOrchestrator::new(Some(api), None, Arc::clone(&fake) as Arc<dyn Transport>)
                .expect("orchestrator");
        orchestrator.connect_stream(StreamKind::Cloud, "9PABC", "msa-token");
        // A second connect while one is in progress is ignored.
        orchestrator.connect_stream(StreamKind::Cloud, "other", "msa-token");
        orchestrator.cancel_pending_connect();
        tokio::time::sleep(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        // Cancelled before the session became Provisioned: no offer, no failure.
        assert_eq!(fake.offers_created(), 0);
        assert_ne!(orchestrator.lifecycle(), StreamLifecycleState::Failed);
        assert_ne!(orchestrator.lifecycle(), StreamLifecycleState::Connected);
        assert_eq!(orchestrator.last_error(), StreamError::default());
    }

    #[test]
    fn transport_events_are_dispatched_by_the_bridge_thread() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let _guard = runtime.enter();
        let fake = Arc::new(FakeTransport::new());
        let orchestrator = orchestrator(&fake);
        orchestrator.connect_stream(StreamKind::Cloud, "test", "");
        fake.push_event(TransportEvent::ChannelOpened(DataChannelKind::Message));
        fake.set_connection_state(PeerConnectionState::Connected);
        fake.push_event(TransportEvent::Failed("dtls timeout".to_owned()));

        // QTRY_VERIFY: the bridge thread delivers asynchronously.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && orchestrator.last_error().message != "dtls timeout"
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        let message = fake.sent_on(DataChannelKind::Message);
        assert_eq!(message.len(), 1);
        assert!(message[0].text().contains("\"type\":\"Handshake\""));
        assert_eq!(orchestrator.last_error().message, "dtls timeout");
        // Attempt 1 of 2 failed with a retryable code: the orchestrator is retrying.
        assert_eq!(orchestrator.lifecycle(), StreamLifecycleState::Provisioning);
        assert_eq!(orchestrator.attempt(), 1);
        drop(orchestrator);
    }

    #[test]
    fn options_from_config_mirror_the_cpp() {
        let mut config = StreamingConfig::defaults();
        config.preferred_sdp_codec = "VP9".to_owned();
        config.scale_video_caps_to_1080 = true;
        config.h264_profile_level_id = "42e020".to_owned();

        let web = options_from_config(&config, false);
        assert_eq!(web.max_video_bitrate_kbps, 120_000);
        assert_eq!(web.max_audio_bitrate_kbps, 48_000);
        assert_eq!(web.preferred_video_codec, "VP9");
        assert_eq!(web.max_video_frame_rate, 60);
        assert_eq!(web.h264_profile_level_id_override, "640029");
        assert!(web.inject_bitrate_lines);
        assert!(!web.scale_video_caps_to_1080);
        assert!(web.h264_profile_level_id.is_empty());
        assert!(web.stereo_audio);

        let native = options_from_config(&config, true);
        assert_eq!(native.max_video_bitrate_kbps, 0);
        assert_eq!(native.max_video_frame_rate, 0);
        assert!(!native.inject_bitrate_lines);
        assert!(native.scale_video_caps_to_1080);
        assert_eq!(native.h264_profile_level_id, "42e020");
        assert!(native.preferred_video_codec.is_empty());
    }

    #[test]
    fn new_outside_a_runtime_is_an_error() {
        let fake = Arc::new(FakeTransport::new());
        assert!(SignalingOrchestrator::new(None, None, fake as Arc<dyn Transport>).is_err());
    }
}
