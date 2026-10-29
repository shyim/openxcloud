//! The contract between the egui UI and the controllers.
//!
//! The Qt app exposed `QObject`s with `Q_PROPERTY`s and signals to QML. Here every
//! controller publishes an immutable-by-the-UI *snapshot* behind a lock and accepts
//! *commands* over a channel. The UI thread reads snapshots once per frame and never
//! blocks on a controller; controllers run on tokio (or their own threads) and call
//! [`Repaint`] after changing a snapshot so eframe redraws.
//!
//! Field names follow the `Q_PROPERTY` names of `SessionController`, `LibraryController`
//! and `StreamController` (snake_case) so the QML ports map one to one.

use std::collections::HashMap;
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use xcloud_proto::library_filter::LibraryFilter;
use xcloud_proto::models::LibraryCard;
use xcloud_proto::streaming_types::{StreamLifecycleState, StreamingStatsSnapshot};

/// Asks the UI to redraw. Controllers call it after every snapshot change.
/// Backed by `egui::Context::request_repaint` in the app and by a no-op in tests.
pub type Repaint = Arc<dyn Fn() + Send + Sync>;

pub fn noop_repaint() -> Repaint {
    Arc::new(|| {})
}

/// A controller as the UI sees it: the latest snapshot plus a command inbox.
#[derive(Clone)]
pub struct Handle<S, C> {
    snapshot: Arc<RwLock<S>>,
    commands: Sender<C>,
}

impl<S: Clone, C> Handle<S, C> {
    /// Creates the UI side; the controller keeps the returned `Receiver` and the
    /// `Arc<RwLock<S>>` it writes to.
    pub fn new(initial: S) -> (Self, Arc<RwLock<S>>, Receiver<C>) {
        let snapshot = Arc::new(RwLock::new(initial));
        let (tx, rx) = crossbeam_channel::unbounded();
        (
            Self {
                snapshot: snapshot.clone(),
                commands: tx,
            },
            snapshot,
            rx,
        )
    }

    /// A handle with no controller behind it (smoke mode, UI tests, previews).
    pub fn detached(initial: S) -> Self {
        Self::new(initial).0
    }

    pub fn read(&self) -> S {
        self.snapshot.read().clone()
    }

    pub fn with<R>(&self, f: impl FnOnce(&S) -> R) -> R {
        f(&self.snapshot.read())
    }

    /// Sends a command; a detached handle silently drops it.
    pub fn send(&self, command: C) {
        let _ = self.commands.send(command);
    }
}

// ----------------------------------------------------------------------------- session

/// `SessionController::AuthState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthState {
    #[default]
    Unknown,
    Unauthenticated,
    Authenticating,
    Authenticated,
    Error,
}

impl AuthState {
    /// The string the QML switched on (`session.state`).
    pub fn name(self) -> &'static str {
        match self {
            AuthState::Unknown => "unknown",
            AuthState::Unauthenticated => "unauthenticated",
            AuthState::Authenticating => "authenticating",
            AuthState::Authenticated => "authenticated",
            AuthState::Error => "error",
        }
    }
}

/// `SessionController` properties.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionSnapshot {
    pub state: AuthState,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    /// `DeviceCodeInfo::signInUrl()`: the URL the UI opens.
    pub sign_in_url: String,
    pub last_error: String,
    pub is_busy: bool,
    pub status_text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionCommand {
    RestoreSession,
    BeginSignIn,
    CancelSignIn,
    SignOut,
}

pub type SessionHandle = Handle<SessionSnapshot, SessionCommand>;

// ----------------------------------------------------------------------------- library

/// One row of `LibraryController::shelves`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Shelf {
    pub id: String,
    pub title: String,
    pub items: Vec<LibraryCard>,
}

/// One entry of `LibraryController::featureGroups`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FeatureGroup {
    pub id: String,
    pub title: String,
    pub count: usize,
    /// The group's feature names, sorted case-insensitively (`features` in the
    /// `QVariantMap`); `FilterMenu` lists them under the group title.
    pub features: Vec<String>,
}

/// `LibraryController` properties.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LibrarySnapshot {
    /// Changes whenever the controller publishes a new library snapshot.
    pub revision: u64,
    /// `items` / `gameModel`: the filtered, sorted cards for the grid.
    pub items: Vec<LibraryCard>,
    /// `allItems`.
    pub all_items: Vec<LibraryCard>,
    /// `shelfModel` content: the cards of the shelf chosen with `SetBrowseShelf`.
    pub browse_shelf: String,
    /// `shelves`: id/title plus a ≤20-card preview (`items.mid(0, 20)`) per shelf.
    pub shelves: Vec<Shelf>,
    /// `m_shelfItems`: the full card list of every shelf, by shelf id; what the
    /// Q_INVOKABLE `shelfItems(shelfId)` returned.
    pub shelf_items: HashMap<String, Vec<LibraryCard>>,
    pub feature_groups: Vec<FeatureGroup>,
    pub query: String,
    pub filter: LibraryFilter,
    pub is_loading: bool,
    pub status_text: String,
    pub last_error: String,
    /// The server rejected the session; offer sign-in instead of retrying stale credentials.
    pub authentication_required: bool,
}

impl LibrarySnapshot {
    pub fn is_empty(&self) -> bool {
        self.all_items.is_empty()
    }
    pub fn game_count(&self) -> usize {
        self.items.len()
    }
    /// `shelfItems(shelfId)`: the full list, not the preview in [`Shelf::items`].
    pub fn shelf_items(&self, shelf_id: &str) -> &[LibraryCard] {
        self.shelf_items
            .get(shelf_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
    pub fn item_by_id(&self, title_id: &str) -> Option<&LibraryCard> {
        self.all_items.iter().find(|c| c.title_id == title_id)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LibraryCommand {
    /// `refresh(force)`.
    Refresh {
        force: bool,
    },
    SetQuery(String),
    SetFilter(LibraryFilter),
    SetBrowseShelf(String),
    /// Store language/market for catalog text (`de-DE`); reloads a loaded library.
    SetLocale(String),
}

pub type LibraryHandle = Handle<LibrarySnapshot, LibraryCommand>;

// ----------------------------------------------------------------------------- stream

/// `StreamController` + `GamepadInputSource` properties the stream view and HUD read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamSnapshot {
    pub lifecycle: StreamLifecycleState,
    pub status_text: String,
    pub title_id: String,
    pub title_name: String,
    pub hero_image: String,
    pub error: String,
    /// `active`: the stream view is shown (from launch until the user closes it).
    pub active: bool,
    pub hud_enabled: bool,
    pub stats: StreamingStatsSnapshot,
    pub pipeline_stats_ready: bool,
    pub decode_stats: xcloud_stream::pipeline::DecodeStats,
    pub video_stats: xcloud_proto::video_jitter::Stats,
    /// `nativeVideo`: the transport owns the media pipeline (native path).
    pub native_video: bool,
    /// `gpuVideo`: frames are IOSurface-backed and drawn through xcloud-gpu.
    pub gpu_video: bool,
    /// `videoFrameSerial`: increments per decoded frame; 0 until the first frame.
    pub video_frame_serial: u64,
    /// `outputHeight` / `upscaled` as reported by the renderer.
    pub output_height: u32,
    pub upscaled: bool,
    /// The upscaler actually running, as the HUD prints it after the output height:
    /// `"FSR"`, `"FX"` (MetalFX) or empty (`VideoRenderer::upscaler_label`).
    pub upscaler_label: String,
    /// Per-stage latency for the HUD (ms). Zero when unknown.
    pub decode_ms_avg: f64,
    pub decode_ms_max: f64,
    pub gpu_ms: f64,
    pub frame_age_ms: f64,
    /// Jitter wait (first RTP packet of the access unit → access unit emitted), average
    /// and maximum over the pipeline's 1 s stats window.
    pub jitter_ms_avg: f64,
    pub jitter_ms_max: f64,
    /// Present time: decoder output → the GPU completed the submission that drew the
    /// frame (`VideoRenderer::last_present_latency_ms`). wgpu runs that completion callback
    /// only when the device is polled, i.e. on the next UI frame's submit, so the value is
    /// an upper bound quantised to the UI frame period (docs/perf.md, "frame age at GPU
    /// done").
    pub present_ms: f64,
    /// `GamepadInputSource::connected` / `controllerName`.
    pub controller_connected: bool,
    pub controller_name: String,
    pub input_focused: bool,
}

impl StreamSnapshot {
    pub fn connected(&self) -> bool {
        self.lifecycle == StreamLifecycleState::Connected
    }
    pub fn resolution_height(&self) -> i32 {
        self.stats.negotiated_height
    }
    pub fn resolution_width(&self) -> i32 {
        self.stats.negotiated_width
    }
    pub fn bitrate_mbps(&self) -> f64 {
        f64::from(self.stats.bitrate_kbps) / 1000.0
    }
}

/// Which trigger a keyboard/pointer command addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerSide {
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamCommand {
    StartCloudStream {
        title_id: String,
        title_name: String,
        hero_image: String,
    },
    StopStreaming,
    SetHudEnabled(bool),
    /// Re-apply live settings before the next launch (`applySettings`).
    ApplySettings(AppSettings),
    /// `GamepadInputSource` keyboard entry points. `name` uses the C++ button names
    /// (a, b, x, y, menu, view, dpad-up/down/left/right, left-shoulder, right-shoulder,
    /// left-thumb, right-thumb, left-trigger, right-trigger).
    SetKey {
        name: String,
        pressed: bool,
    },
    SetLeftStick {
        x: f64,
        y: f64,
    },
    SetRightStick {
        x: f64,
        y: f64,
    },
    SetTrigger {
        side: TriggerSide,
        value: f64,
    },
    ReleaseAll,
    SetInputFocused(bool),
    /// Reported by the stream view after the renderer ran (`setOutput`). `present_ms` is
    /// the renderer's last present latency (see [`StreamSnapshot::present_ms`]);
    /// `upscaler_label` is the renderer's HUD suffix (see [`StreamSnapshot::upscaler_label`]).
    SetOutput {
        height: u32,
        upscaled: bool,
        upscaler_label: &'static str,
        gpu_ms: f64,
        frame_age_ms: f64,
        present_ms: f64,
    },
}

pub type StreamHandle = Handle<StreamSnapshot, StreamCommand>;

/// Where the stream view gets decoded frames: the reader half of the media mailbox.
/// Only the UI thread locks it, once per frame.
pub type VideoSource = Arc<parking_lot::Mutex<xcloud_media::MailboxReader>>;

// ----------------------------------------------------------------------------- settings

/// `core/Settings.h` keys. Persisted as JSON by `settings.rs`; the UI edits a copy and
/// sends it back with `StreamCommand::ApplySettings` / `SettingsCommand`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub hud_enabled: bool,
    /// "auto" | "720p" | "1080p" | "1440p" | "1080pHQ" (see `StreamResolutionMode`).
    pub resolution_profile: String,
    pub preferred_frame_rate: i32,
    pub locale: String,
    pub preferred_region: String,
    pub prefer_ipv6: bool,
    pub deadzone: f64,
    pub invert_y: bool,
    pub swap_ab: bool,
    pub trigger_sensitivity: f64,
    pub vibration_intensity: f64,
    /// New in the Rust port: "immediate" (default for the stream view) | "fifo".
    pub present_mode: String,
    /// New in the Rust port: FSR sharpness 0..2 and an upscaler on/off switch.
    pub upscale_enabled: bool,
    /// Local GPU output target height (1080, 1440 or 2160).
    pub upscale_height: u32,
    pub sharpness: f32,
    /// New in the Rust port: which upscaler runs when `upscale_enabled`: "fsr" (default)
    /// or "metalfx" (macOS, `--features metalfx`; falls back to FSR when unavailable).
    /// `OPENXCLOUD_UPSCALER` overrides it (`xcloud_gpu::UPSCALER_ENV`).
    pub upscaler: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            hud_enabled: false,
            // `Settings::resolutionProfile()` defaults to "p1440" (core/Settings.cpp:21).
            resolution_profile: "1440p".into(),
            preferred_frame_rate: 60,
            locale: "en-US".into(),
            preferred_region: String::new(),
            prefer_ipv6: false,
            deadzone: 0.15,
            invert_y: false,
            swap_ab: false,
            // `Settings::triggerSensitivity()` defaults to 0.5 (core/Settings.cpp:89).
            trigger_sensitivity: 0.5,
            vibration_intensity: 1.0,
            present_mode: "immediate".into(),
            upscale_enabled: true,
            upscale_height: 2160,
            sharpness: 0.25,
            upscaler: "fsr".into(),
        }
    }
}

// ----------------------------------------------------------------------------- app

/// Everything the UI needs, built by `services.rs` / `app.rs`.
#[derive(Clone)]
pub struct Controllers {
    pub session: SessionHandle,
    pub library: LibraryHandle,
    pub stream: StreamHandle,
    pub video: VideoSource,
    pub settings: Arc<RwLock<AppSettings>>,
    /// `AppController::streamingAvailable`.
    pub streaming_available: bool,
    pub app_version: String,
}
