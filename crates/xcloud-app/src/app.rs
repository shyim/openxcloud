//! Port of qt/qml/Main.qml and the `AppController` glue of qt/src/app/AppController.{h,cpp}:
//! the `eframe::App`, screen routing, the window-level shortcut and the `--smoke`
//! self-check of qt/src/main.cpp.
//!
//! `Main.qml` was an `ApplicationWindow` with two `Loader`s: one switched on
//! `session.state` (`"authenticated"` → `LibraryView`, `"authenticating"` →
//! `DeviceCodeView`, `"unknown"` → a busy indicator, anything else → `SignInView`), the
//! other showed `StreamView` on top while `stream.active`. Both are [`App::show_in`] here;
//! the routing is the pure [`route_for`].
//!
//! What `AppController` and the QML did between the controllers also lives here, once per
//! frame, never blocking:
//!
//! * `playRequested(item)` → `stream.startCloudStream(titleId, name, heroImageUrl || artUrl)`
//!   ([`hero_image_for`]).
//! * The `authenticated` / `tokensChanged` signals ([`SessionEvent`]) are drained from the
//!   session controller's channel and handed to [`Services::handle_session_event`]
//!   (credentials + `library.refresh(true)`).
//! * `Shortcut { sequences: ["Ctrl+Shift+I", "Meta+Shift+I"] }` toggles the HUD. While the
//!   stream view is shown it consumes the shortcut itself; otherwise the shell does.
//! * The settings sheet (`ui::settings_sheet`) is opened from the top-bar gear or the
//!   account menu; a change is written to the shared
//!   `Controllers::settings`, sent to the stream host (`StreamCommand::ApplySettings`),
//!   applied to the API preferences and saved. Settings the controllers change themselves
//!   (`setHudEnabled` writes `hud/enabled`) are saved by the per-frame comparison in
//!   [`App::sync_settings`].
//!
//! The production graph is built by [`App::launch`] from the `eframe::CreationContext`
//! (the repaint callback needs the `egui::Context`); [`App::detached`] builds the same
//! shell on `Handle::detached` snapshots for the smoke run and the tests.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use anyhow::Context as _;
use crossbeam_channel::Receiver;
use egui::{Ui, pos2, vec2};
use parking_lot::RwLock;
use tokio::runtime::Handle as RuntimeHandle;
use xcloud_media::{AudioOutput, FrameMailbox};
use xcloud_proto::models::LibraryCard;
use xcloud_proto::streaming_types::StreamLifecycleState;

use crate::artwork::{ArtworkCache, ArtworkConfig, ReqwestFetcher};
use crate::library::LibraryController;
use crate::services::{APP_VERSION, Services};
use crate::session::{self, SessionEvent};
use crate::settings::SettingsStore;
use crate::state::{
    AppSettings, AuthState, Controllers, Handle, LibraryCommand, LibraryHandle, LibrarySnapshot,
    Repaint, SessionCommand, SessionHandle, SessionSnapshot, Shelf, StreamCommand, StreamHandle,
    StreamSnapshot, VideoSource,
};
use crate::stream::{
    HostTransport, StreamHost, StreamHostConfig, default_decoder_factory, native_gamepad_factory,
};
use crate::theme;
use crate::ui::device_code::DeviceCodeView;
use crate::ui::library_view::{LibraryAction, LibraryView};
use crate::ui::settings_sheet::SettingsSheet;
use crate::ui::signin::SignInView;
use crate::ui::stream_view::{GpuVideo, HUD_SHORTCUTS, StreamView};
use crate::ui::widgets::{ArtworkSource, NoArtwork};
use crate::user_data::{self, UserData, UserDataHandle, UserDataStore};

const LOG: &str = "App";

/// `ApplicationWindow.title`.
pub const WINDOW_TITLE: &str = "OpenXCloud";
/// `width: 1360; height: 900`.
pub const DEFAULT_WINDOW_SIZE: [f32; 2] = [1360.0, 900.0];
/// `minimumWidth: 900; minimumHeight: 560`.
pub const MIN_WINDOW_SIZE: [f32; 2] = [900.0, 560.0];
/// Reverse-DNS id for the window class / bundle.
pub const APP_ID: &str = "de.shyim.openxcloud";

/// The loading screen text (`loadingComponent`).
pub const LOADING_TEXT: &str = "Starting";
/// Title of the settings window.
pub const SETTINGS_TITLE: &str = "Settings";

/// Which component Main.qml's first `Loader` shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// `"unknown"`: the busy indicator with "Starting".
    Loading,
    /// `"authenticating"`: `DeviceCodeView`.
    DeviceCode,
    /// `"authenticated"`: `LibraryView`.
    Library,
    /// Everything else (`unauthenticated`, `error`): `SignInView`.
    SignIn,
}

/// The `switch (session.state)` of Main.qml.
pub fn route_for(state: AuthState) -> Route {
    match state {
        AuthState::Authenticated => Route::Library,
        AuthState::Authenticating => Route::DeviceCode,
        AuthState::Unknown => Route::Loading,
        AuthState::Unauthenticated | AuthState::Error => Route::SignIn,
    }
}

/// `item.heroImageUrl && item.heroImageUrl.length > 0 ? item.heroImageUrl : item.artUrl`.
pub fn hero_image_for(card: &LibraryCard) -> &str {
    if card.hero_image_url.is_empty() {
        &card.art_url
    } else {
        &card.hero_image_url
    }
}

/// `stream.startCloudStream(item.titleId, item.name, hero)` as a command.
pub fn play_command(card: &LibraryCard) -> StreamCommand {
    StreamCommand::StartCloudStream {
        title_id: card.title_id.clone(),
        title_name: card.name.clone(),
        hero_image: hero_image_for(card).to_owned(),
    }
}

// ----------------------------------------------------------------------------- app

/// The OpenXCloud window: Main.qml as an `eframe::App`.
pub struct App {
    controllers: Controllers,
    /// `None` on a detached shell (smoke run, tests).
    services: Option<Services>,
    session_events: Option<Receiver<SessionEvent>>,
    stream_host: Option<StreamHost>,
    artwork: Box<dyn ArtworkSource>,
    sign_in: SignInView,
    device_code: DeviceCodeView,
    library: LibraryView,
    stream_view: StreamView,
    settings_store: SettingsStore,
    settings_open: bool,
    settings_sheet: SettingsSheet,
    /// Keeps macOS from opening Games/Arcade on the controller Home button mid-stream.
    system_gestures: xcloud_input::system_gestures::SystemGestureGuard,
    /// `streamLoader.active` in the previous frame, to notice the view going away.
    stream_was_active: bool,
    /// The last settings save error, logged once.
    save_error_logged: bool,
    /// The route drawn last frame, to log screen changes once.
    last_route: Option<Route>,
    /// Favourites and play history (`user_data.json`; in memory when detached).
    user_data: UserDataStore,
    /// The title streaming right now and when its play time was last booked.
    playing: Option<(String, std::time::Instant)>,
}

/// How often connected time is booked while a stream runs, so a crash loses little.
const PLAY_TIME_TICK: std::time::Duration = std::time::Duration::from_secs(30);

impl App {
    /// Builds the production object graph (the `AppController` constructor plus
    /// `main.cpp`'s wiring) on eframe's context: controllers on `runtime`, the GPU video
    /// path on eframe's wgpu device when there is one, then `bootstrap()`.
    pub fn launch(
        cc: &eframe::CreationContext<'_>,
        runtime: RuntimeHandle,
        store: SettingsStore,
    ) -> anyhow::Result<Self> {
        let ctx = cc.egui_ctx.clone();
        theme::apply(&ctx);
        let repaint: Repaint = Arc::new({
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        });

        let services = Services::new(runtime.clone()).context("cannot build the services")?;
        let settings = Arc::new(RwLock::new(store.settings().clone()));

        // SessionController.
        let (session, session_events) = session::spawn(&services, Arc::clone(&repaint));

        // LibraryController, on its own thread like the C++ ran it on the GUI thread's
        // event loop (network work goes to the runtime).
        let (library, library_snapshot, library_commands) = Handle::new(LibrarySnapshot::default());
        // Store text in the user's language (queued before the first refresh).
        library.send(LibraryCommand::SetLocale(settings.read().locale.clone()));
        LibraryController::new(
            Arc::clone(&services.api),
            Some(Arc::clone(&services.catalog)),
            library_snapshot,
            library_commands,
            Arc::clone(&repaint),
            runtime.clone(),
        )
        .with_catalog_cache_dir(crate::library::default_catalog_cache_dir())
        .spawn()
        .context("cannot start the library controller")?;

        // StreamController and everything it owned.
        let transport = match &services.webrtc {
            Some(webrtc) => HostTransport::WebRtc(Arc::clone(webrtc)),
            None => HostTransport::Generic(Arc::clone(&services.transport)),
        };
        let host = StreamHost::spawn(
            StreamHostConfig {
                transport,
                auth: Some(Arc::clone(&services.auth)),
                api: Some(Arc::clone(&services.api)),
                settings: Arc::clone(&settings),
                repaint,
                decoder: default_decoder_factory(),
                audio: AudioOutput::new(),
                gamepad: native_gamepad_factory(),
            },
            &runtime,
        )
        .context("cannot start the stream host")?;

        // ArtworkProvider.
        let fetcher = ReqwestFetcher::new().context("cannot build the artwork HTTP client")?;
        let artwork = ArtworkCache::new(
            ctx.clone(),
            runtime.clone(),
            Arc::new(fetcher),
            ArtworkConfig::default(),
        );

        let controllers = Controllers {
            session,
            library,
            stream: host.handle(),
            video: host.video(),
            settings,
            streaming_available: services.streaming_available,
            app_version: services.app_version.clone(),
        };

        let gpu = cc
            .wgpu_render_state
            .as_ref()
            .map(|render_state| GpuVideo::new(render_state, &controllers.settings.read()));
        if gpu.is_none() {
            tracing::warn!(target: LOG, "no wgpu render state; video uses the CPU image path");
        }

        let mut app = Self::assemble(controllers, Box::new(artwork), store, gpu);
        app.set_user_data(UserDataStore::open());
        app.services = Some(services);
        app.session_events = Some(session_events);
        app.stream_host = Some(host);

        // `AppController::bootstrap` + the preferences half of `refreshPreferences`.
        if let Some(services) = &app.services {
            services.apply_preferences(&app.controllers.settings.read());
            services.bootstrap(&app.controllers.session);
        }
        tracing::info!(
            target: LOG,
            "OpenXCloud {} ready streamingAvailable={}",
            app.controllers.app_version,
            app.controllers.streaming_available
        );
        Ok(app)
    }

    /// The shell on handles without controllers (`Handle::detached`): the smoke run and
    /// the UI tests. The device-code view does not open a browser.
    pub fn detached(controllers: Controllers, store: SettingsStore) -> Self {
        let mut app = Self::assemble(controllers, Box::new(NoArtwork), store, None);
        app.device_code = DeviceCodeView::new().without_auto_open();
        app
    }

    fn assemble(
        controllers: Controllers,
        artwork: Box<dyn ArtworkSource>,
        store: SettingsStore,
        gpu: Option<GpuVideo>,
    ) -> Self {
        let stream_view = StreamView::new(
            controllers.stream.clone(),
            Arc::clone(&controllers.video),
            Arc::clone(&controllers.settings),
        )
        .with_gpu(gpu);
        Self {
            controllers,
            services: None,
            session_events: None,
            stream_host: None,
            artwork,
            sign_in: SignInView::new(),
            device_code: DeviceCodeView::new(),
            library: LibraryView::new(),
            stream_view,
            settings_store: store,
            settings_open: false,
            settings_sheet: SettingsSheet::new(),
            system_gestures: xcloud_input::system_gestures::SystemGestureGuard::new(),
            stream_was_active: false,
            save_error_logged: false,
            last_route: None,
            user_data: UserDataStore::in_memory(UserData::default()),
            playing: None,
        }
    }

    /// Swaps the favourites/history store (the launch path opens the file).
    pub fn set_user_data(&mut self, store: UserDataStore) {
        self.library.set_user_data(store.handle());
        self.user_data = store;
    }

    pub fn user_data(&self) -> UserDataHandle {
        self.user_data.handle()
    }

    /// Books connected streaming time on the playing title and writes the store
    /// when anything changed.
    fn track_play(&mut self, now: std::time::Instant) {
        let (connected, title_id) = self.controllers.stream.with(|s| {
            (
                s.active && s.lifecycle == StreamLifecycleState::Connected,
                s.title_id.clone(),
            )
        });
        let handle = self.user_data.handle();
        match &mut self.playing {
            Some((title, since)) if connected && *title == title_id => {
                if now.duration_since(*since) >= PLAY_TIME_TICK {
                    handle
                        .write()
                        .add_play_time(title, now.duration_since(*since).as_secs());
                    *since = now;
                }
            }
            playing => {
                if let Some((title, since)) = playing.take() {
                    handle
                        .write()
                        .add_play_time(&title, now.duration_since(since).as_secs());
                }
                if connected && !title_id.is_empty() {
                    *playing = Some((title_id, now));
                }
            }
        }
        if let Err(err) = self.user_data.save_if_changed() {
            tracing::warn!(target: LOG, "cannot save user data: {err}");
        }
    }

    /// Wires a `Services` and the session event channel into a detached shell (tests of
    /// the `wireSession` path without a real controller).
    pub fn attach_services(&mut self, services: Services, events: Receiver<SessionEvent>) {
        self.services = Some(services);
        self.session_events = Some(events);
    }

    /// Attaches the wgpu video path after construction (smoke run).
    pub fn attach_gpu(&mut self, render_state: &egui_wgpu::RenderState) {
        let settings = self.controllers.settings.read().clone();
        self.stream_view
            .set_gpu(Some(GpuVideo::new(render_state, &settings)));
    }

    pub fn controllers(&self) -> &Controllers {
        &self.controllers
    }

    pub fn settings_store(&self) -> &SettingsStore {
        &self.settings_store
    }

    /// Whether the settings window is open.
    pub fn settings_open(&self) -> bool {
        self.settings_open
    }

    /// Opens or closes the settings window (the sidebar gear does this on click).
    pub fn set_settings_open(&mut self, open: bool) {
        self.settings_open = open;
    }

    /// The route the shell draws this frame.
    pub fn route(&self) -> Route {
        route_for(self.controllers.session.with(|s| s.state))
    }

    /// `onPlayRequested`: starts the cloud stream of `card`.
    pub fn play(&self, card: &LibraryCard) {
        tracing::info!(target: LOG, "play requested titleId={} name={}", card.title_id, card.name);
        self.user_data
            .handle()
            .write()
            .record_launch(&card.title_id, user_data::now_secs());
        self.controllers.stream.send(play_command(card));
    }

    /// `wireSession`: drains the session controller's events.
    fn drain_session_events(&mut self) {
        let (Some(events), Some(services)) = (&self.session_events, &self.services) else {
            return;
        };
        while let Ok(event) = events.try_recv() {
            tracing::debug!(target: LOG, event = match &event {
                SessionEvent::TokensRestored(_) => "tokens restored",
                SessionEvent::Authenticated(_) => "authenticated",
                SessionEvent::SignedOut => "signed out",
            }, "session event");
            services.handle_session_event(&event, &self.controllers.library);
        }
    }

    /// Saves the shared settings when anything changed them since the last save
    /// (`QSettings::sync` on every setter).
    ///
    /// The settings lock is shared with the stream host (`setHudEnabled` writes it), so
    /// the guard is held only for the clone and released before any disk I/O. `QSettings`
    /// also wrote synchronously on the GUI thread, but held no lock another thread took.
    fn sync_settings(&mut self) {
        let current = self.controllers.settings.read().clone();
        match self.settings_store.save_if_changed(&current) {
            Ok(_) => self.save_error_logged = false,
            Err(error) => {
                if !self.save_error_logged {
                    tracing::error!(target: LOG, "cannot save settings: {error}");
                    self.save_error_logged = true;
                }
            }
        }
    }

    /// A settings-panel change: publish it, re-apply the live settings
    /// (`StreamController::applySettings` / `refreshPreferences`) and save.
    fn apply_settings(&mut self, settings: AppSettings) {
        let locale_changed = self.controllers.settings.read().locale != settings.locale;
        *self.controllers.settings.write() = settings.clone();
        if locale_changed {
            self.controllers
                .library
                .send(LibraryCommand::SetLocale(settings.locale.clone()));
        }
        if let Some(services) = &self.services {
            services.apply_preferences(&settings);
        }
        self.controllers
            .stream
            .send(StreamCommand::ApplySettings(settings));
        self.sync_settings();
    }

    /// One frame of Main.qml inside `ui` (eframe's central panel, or a test harness).
    pub fn show_in(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.drain_session_events();
        self.sync_settings();
        self.track_play(std::time::Instant::now());

        // Loader { active: stream.active; sourceComponent: streamSurfaceComponent }.
        let stream_active = self.controllers.stream.with(|s| s.active);
        self.system_gestures
            .update(stream_active, std::time::Instant::now());
        if stream_active {
            self.stream_was_active = true;
            self.stream_view.show(ui, self.artwork.as_ref());
            return;
        }
        if self.stream_was_active {
            self.stream_was_active = false;
            self.stream_view.reset();
        }

        // Shortcut { sequences: ["Ctrl+Shift+I", "Meta+Shift+I"] }.
        if ctx.input_mut(|i| HUD_SHORTCUTS.iter().any(|s| i.consume_shortcut(s))) {
            let hud = self.controllers.stream.with(|s| s.hud_enabled);
            self.controllers
                .stream
                .send(StreamCommand::SetHudEnabled(!hud));
        }

        let route = self.route();
        if self.last_route != Some(route) {
            tracing::info!(target: LOG, "screen {route:?}");
            self.last_route = Some(route);
        }
        match route {
            Route::Loading => loading_screen(ui),
            Route::SignIn => self.sign_in.show(
                ui,
                &self.controllers.session,
                self.controllers.streaming_available,
            ),
            Route::DeviceCode => self.device_code.show(ui, &self.controllers.session),
            Route::Library => {
                self.library.set_settings_open(self.settings_open);
                let action = self.library.show(
                    ui,
                    &self.controllers.library,
                    &self.controllers.session,
                    self.artwork.as_ref(),
                );
                match action {
                    Some(LibraryAction::Play(card)) => self.play(&card),
                    Some(LibraryAction::SignIn) => {
                        self.controllers.session.send(SessionCommand::BeginSignIn)
                    }
                    Some(LibraryAction::OpenSettings) => {
                        self.settings_open = !self.settings_open;
                    }
                    None => {}
                }
            }
        }
        if self.settings_open {
            self.settings_window(&ctx);
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut draft = self.controllers.settings.read().clone();
        let out = self.settings_sheet.show(
            ctx,
            &mut draft,
            &self.controllers.app_version,
            self.controllers.streaming_available,
        );
        if out.changed {
            self.apply_settings(draft);
        }
        if out.refresh {
            self.controllers
                .library
                .send(LibraryCommand::Refresh { force: true });
        }
        if out.sign_out {
            self.controllers.session.send(SessionCommand::SignOut);
            self.settings_open = false;
        }
        if out.close {
            self.settings_open = false;
        }
    }

    /// App exit: stop the stream, tear the host down, release the webrtc bridge and
    /// save. Called by `eframe::App::on_exit`.
    pub fn shutdown(&mut self) {
        tracing::info!(target: LOG, "shutting down");
        self.controllers.stream.send(StreamCommand::StopStreaming);
        if let Some(host) = self.stream_host.take() {
            host.shutdown();
        }
        // Dropping the last transport handle stops its owned WebRTC worker.
        self.services = None;
        self.session_events = None;
        if let Some((title, since)) = self.playing.take() {
            self.user_data
                .handle()
                .write()
                .add_play_time(&title, since.elapsed().as_secs());
        }
        if let Err(err) = self.user_data.save_if_changed() {
            tracing::warn!(target: LOG, "cannot save user data: {err}");
        }
        self.sync_settings();
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        egui::Frame::new()
            .fill(theme::BACKGROUND)
            .show(ui, |ui| self.show_in(ui));
    }

    fn on_exit(&mut self) {
        self.shutdown();
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        let [r, g, b, _] = theme::BACKGROUND.to_normalized_gamma_f32();
        [r, g, b, 1.0]
    }
}

/// `loadingComponent`: a busy indicator with "Starting" 24 pt below the centre.
pub fn loading_screen(ui: &mut Ui) {
    let full = ui.available_rect_before_wrap();
    ui.painter().rect_filled(full, 0.0, theme::BACKGROUND);
    let centre = full.center();
    let spinner = egui::Rect::from_center_size(centre, vec2(40.0, 40.0));
    ui.put(
        spinner,
        egui::Spinner::new().size(40.0).color(theme::ACCENT),
    );
    let text =
        egui::Rect::from_min_size(pos2(full.left(), centre.y + 24.0), vec2(full.width(), 24.0));
    ui.put(
        text,
        egui::Label::new(
            egui::RichText::new(LOADING_TEXT)
                .size(theme::FONT_BODY)
                .color(theme::TEXT_PRIMARY),
        ),
    );
}

// ----------------------------------------------------------------------------- detached

/// Snapshots and command inboxes of a controller-less shell: what `Handle::new` returns
/// for the three controllers, kept so a smoke run or a test can drive the screens.
pub struct Detached {
    pub controllers: Controllers,
    pub session: Arc<RwLock<SessionSnapshot>>,
    pub library: Arc<RwLock<LibrarySnapshot>>,
    pub stream: Arc<RwLock<StreamSnapshot>>,
    pub session_commands: Receiver<SessionCommand>,
    pub library_commands: Receiver<LibraryCommand>,
    pub stream_commands: Receiver<StreamCommand>,
    /// The writer half of the video mailbox (frames pushed here reach the stream view).
    pub video_writer: xcloud_media::MailboxWriter,
}

impl Detached {
    pub fn new(settings: AppSettings) -> Self {
        let (session, session_snapshot, session_commands): (SessionHandle, _, _) =
            Handle::new(SessionSnapshot::default());
        let (library, library_snapshot, library_commands): (LibraryHandle, _, _) =
            Handle::new(LibrarySnapshot::default());
        let (stream, stream_snapshot, stream_commands): (StreamHandle, _, _) =
            Handle::new(StreamSnapshot::default());
        let (video_writer, reader) = FrameMailbox::open();
        let video: VideoSource = Arc::new(parking_lot::Mutex::new(reader));
        Self {
            controllers: Controllers {
                session,
                library,
                stream,
                video,
                settings: Arc::new(RwLock::new(settings)),
                streaming_available: true,
                app_version: APP_VERSION.to_owned(),
            },
            session: session_snapshot,
            library: library_snapshot,
            stream: stream_snapshot,
            session_commands,
            library_commands,
            stream_commands,
            video_writer,
        }
    }

    pub fn set_auth_state(&self, state: AuthState) {
        self.session.write().state = state;
    }
}

// ----------------------------------------------------------------------------- smoke

/// The application states exercised by the `--smoke` run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmokeScreen {
    /// `session.state == "unknown"`: the loading component.
    Loading,
    /// `SignInView`, idle.
    SignIn,
    /// `SignInView` with `lastError` and streaming unavailable text paths.
    SignInError,
    /// `DeviceCodeView` with a code.
    DeviceCode,
    /// `LibraryView` home with shelves, feature groups and tiles (`HomeShelvesView`,
    /// `GameShelves`, `GameTile`, `RemoteImage`, `FilterMenu`).
    Library,
    /// `LibraryView` plus the settings window.
    LibrarySettings,
    /// `StreamView` while connecting (hero placeholder, controls).
    StreamConnecting,
    /// `StreamView` connected with the HUD (`StreamHUD`) and a decoded frame.
    StreamConnected,
}

impl SmokeScreen {
    pub const ALL: [SmokeScreen; 8] = [
        SmokeScreen::Loading,
        SmokeScreen::SignIn,
        SmokeScreen::SignInError,
        SmokeScreen::DeviceCode,
        SmokeScreen::Library,
        SmokeScreen::LibrarySettings,
        SmokeScreen::StreamConnecting,
        SmokeScreen::StreamConnected,
    ];
}

/// Frames each screen is drawn for.
pub const SMOKE_FRAMES_PER_SCREEN: usize = 3;

/// A sample library for the smoke run: two shelves, a feature group, a few cards.
pub fn smoke_library() -> LibrarySnapshot {
    let card = |id: &str, name: &str| LibraryCard {
        title_id: id.to_owned(),
        product_id: format!("P{id}"),
        name: name.to_owned(),
        short_description: "A sample title for the smoke run.".to_owned(),
        art_url: format!("https://example.invalid/{id}/art.png"),
        poster_image_url: format!("https://example.invalid/{id}/poster.png"),
        hero_image_url: format!("https://example.invalid/{id}/hero.png"),
        publisher_name: "RayLabs".to_owned(),
        is_in_mru: id == "T1",
        ..LibraryCard::default()
    };
    let cards = vec![
        card("T1", "Halo Infinite"),
        card("T2", "Forza Horizon 5"),
        card("T3", "Sea of Thieves"),
        card("T4", "Starfield"),
    ];
    let shelves = vec![
        Shelf {
            id: crate::library::SHELF_MRU.to_owned(),
            title: crate::library::SHELF_MRU_TITLE.to_owned(),
            items: cards[..1].to_vec(),
        },
        Shelf {
            id: crate::library::SHELF_RECENTLY_ADDED.to_owned(),
            title: crate::library::SHELF_RECENTLY_ADDED_TITLE.to_owned(),
            items: cards.clone(),
        },
    ];
    let shelf_items = shelves
        .iter()
        .map(|s| (s.id.clone(), s.items.clone()))
        .collect();
    LibrarySnapshot {
        items: cards.clone(),
        all_items: cards,
        shelves,
        shelf_items,
        feature_groups: vec![crate::state::FeatureGroup {
            id: "play-together".to_owned(),
            title: crate::library::GROUP_PLAY_TOGETHER.to_owned(),
            count: 2,
            features: vec!["Online co-op".to_owned(), "Cross-platform".to_owned()],
        }],
        status_text: "4 games".to_owned(),
        ..LibrarySnapshot::default()
    }
}

/// Drives one detached [`App`] through every [`SmokeScreen`], counting panics.
pub struct SmokeRunner {
    app: App,
    fixture: Detached,
    screens: Vec<SmokeScreen>,
    index: usize,
    frame: usize,
    failures: Vec<String>,
    /// Test hook: a screen that panics instead of drawing.
    #[cfg(test)]
    panic_on: Option<SmokeScreen>,
}

impl SmokeRunner {
    pub fn new() -> Self {
        let fixture = Detached::new(AppSettings::default());
        let app = App::detached(
            fixture.controllers.clone(),
            SettingsStore::in_memory(AppSettings::default()),
        );
        Self {
            app,
            fixture,
            screens: SmokeScreen::ALL.to_vec(),
            index: 0,
            frame: 0,
            failures: Vec::new(),
            #[cfg(test)]
            panic_on: None,
        }
    }

    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    pub fn finished(&self) -> bool {
        self.index >= self.screens.len()
    }

    pub fn failures(&self) -> &[String] {
        &self.failures
    }

    /// The screen being drawn, `None` once finished.
    pub fn current(&self) -> Option<SmokeScreen> {
        self.screens.get(self.index).copied()
    }

    /// Puts the snapshots into the state `screen` needs.
    fn enter(&mut self, screen: SmokeScreen) {
        {
            let mut session = self.fixture.session.write();
            *session = SessionSnapshot::default();
            session.state = match screen {
                SmokeScreen::Loading => AuthState::Unknown,
                SmokeScreen::SignIn => AuthState::Unauthenticated,
                SmokeScreen::SignInError => AuthState::Error,
                SmokeScreen::DeviceCode => AuthState::Authenticating,
                _ => AuthState::Authenticated,
            };
            match screen {
                SmokeScreen::SignInError => {
                    session.last_error = "Token refresh failed (smoke)".to_owned();
                }
                SmokeScreen::DeviceCode => {
                    session.user_code = "ABCD-EFGH".to_owned();
                    session.verification_uri = "https://microsoft.com/link".to_owned();
                    session.sign_in_url = "https://microsoft.com/link?otc=ABCDEFGH".to_owned();
                    session.status_text = "Waiting for you to finish signing in".to_owned();
                    session.is_busy = true;
                }
                _ => {}
            }
        }
        *self.fixture.library.write() = match screen {
            SmokeScreen::Library | SmokeScreen::LibrarySettings => smoke_library(),
            _ => LibrarySnapshot::default(),
        };
        {
            let mut stream = self.fixture.stream.write();
            *stream = StreamSnapshot::default();
            match screen {
                SmokeScreen::StreamConnecting => {
                    stream.active = true;
                    stream.lifecycle = StreamLifecycleState::ConnectingWebRTC;
                    stream.status_text = "Connecting".to_owned();
                    stream.title_id = "T1".to_owned();
                    stream.title_name = "Halo Infinite".to_owned();
                    stream.hero_image = "https://example.invalid/T1/hero.png".to_owned();
                }
                SmokeScreen::StreamConnected => {
                    stream.active = true;
                    stream.lifecycle = StreamLifecycleState::Connected;
                    stream.status_text = "Connected".to_owned();
                    stream.title_id = "T1".to_owned();
                    stream.title_name = "Halo Infinite".to_owned();
                    stream.hud_enabled = true;
                    stream.native_video = true;
                    stream.video_frame_serial = 1;
                    stream.stats.negotiated_width = 1920;
                    stream.stats.negotiated_height = 1080;
                    stream.stats.frames_per_second = 60.0;
                    stream.stats.bitrate_kbps = 18_000;
                    stream.stats.round_trip_time_ms = 23.0;
                    stream.stats.has_latency = true;
                    stream.controller_connected = true;
                    stream.controller_name = "Smoke Controller".to_owned();
                    stream.decode_ms_avg = 4.2;
                    stream.decode_ms_max = 7.9;
                    stream.jitter_ms_avg = 1.9;
                    stream.jitter_ms_max = 12.0;
                    stream.present_ms = 18.0;
                }
                _ => {}
            }
        }
        if screen == SmokeScreen::StreamConnected {
            self.fixture.video_writer.submit(smoke_frame(64, 36));
        }
        self.app
            .set_settings_open(screen == SmokeScreen::LibrarySettings);
    }

    /// Draws one frame. A panic inside a screen is caught, recorded and the run moves
    /// on to the next screen.
    pub fn step(&mut self, ui: &mut Ui) {
        let Some(screen) = self.current() else {
            return;
        };
        if self.frame == 0 {
            self.enter(screen);
        }
        let app = &mut self.app;
        #[cfg(test)]
        let panic_on = self.panic_on;
        let result = catch_unwind(AssertUnwindSafe(|| {
            #[cfg(test)]
            if panic_on == Some(screen) {
                panic!("injected smoke panic");
            }
            app.show_in(ui)
        }));
        if let Err(payload) = result {
            let message = panic_message(payload.as_ref());
            tracing::error!(target: LOG, "smoke: {screen:?} panicked: {message}");
            self.failures.push(format!("{screen:?}: {message}"));
            self.frame = 0;
            self.index += 1;
        } else {
            self.frame += 1;
            if self.frame >= SMOKE_FRAMES_PER_SCREEN {
                tracing::info!(target: LOG, "smoke: {screen:?} ok");
                self.frame = 0;
                self.index += 1;
            }
        }
        ui.ctx().request_repaint();
    }
}

impl Default for SmokeRunner {
    fn default() -> Self {
        Self::new()
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic".to_owned()
    }
}

/// A grey NV12 frame for the connected smoke screen.
fn smoke_frame(width: u32, height: u32) -> xcloud_media::Frame {
    use std::time::Instant;
    let now = Instant::now();
    let uv_w = width.div_ceil(2);
    let uv_h = height.div_ceil(2);
    xcloud_media::Frame {
        buf: xcloud_media::NativeBuffer::Nv12Cpu {
            y: vec![0x80; (width * height) as usize],
            uv: vec![0x80; (uv_w * uv_h * 2) as usize],
            y_stride: width as usize,
            uv_stride: (uv_w * 2) as usize,
        },
        width,
        height,
        color: xcloud_media::ColorInfo::default(),
        t_received: now,
        t_submitted: now,
        t_decoded: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::library_view::SETTINGS_LABEL;
    use crate::ui::signin::sign_in_button_text;
    use crate::ui::stream_view::SURFACE_LABEL;
    use egui::{Key, Modifiers};
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use xcloud_api::token_store::MemoryTokenStore;
    use xcloud_api::transport::FakeTransport;
    use xcloud_webrtc::null::NullTransport;

    fn harness<'a>(app: &'a mut App) -> Harness<'a> {
        Harness::builder()
            .with_size(vec2(1360.0, 900.0))
            .with_context_setup(theme::apply)
            .allow_missing_glyphs()
            .build_ui(move |ui| app.show_in(ui))
    }

    fn detached(state: AuthState) -> (Detached, App) {
        let fixture = Detached::new(AppSettings::default());
        fixture.set_auth_state(state);
        let app = App::detached(
            fixture.controllers.clone(),
            SettingsStore::in_memory(AppSettings::default()),
        );
        (fixture, app)
    }

    fn drain<T>(rx: &Receiver<T>) -> Vec<T> {
        rx.try_iter().collect()
    }

    fn temp_path(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("xcloud-rs-app-{}-{n}-{name}", std::process::id()))
    }

    #[test]
    fn play_records_the_launch_and_connected_time() {
        let (fixture, mut app) = detached(AuthState::Authenticated);
        let card = LibraryCard {
            title_id: "T9".into(),
            name: "Forza".into(),
            ..LibraryCard::default()
        };
        app.play(&card);
        let user = app.user_data();
        assert_eq!(user.read().history[0].title_id, "T9");
        assert_eq!(user.read().history[0].sessions, 1);

        let t0 = std::time::Instant::now();
        {
            let mut stream = fixture.stream.write();
            stream.active = true;
            stream.lifecycle = StreamLifecycleState::Connected;
            stream.title_id = "T9".into();
        }
        app.track_play(t0);
        app.track_play(t0 + std::time::Duration::from_secs(40));
        assert_eq!(user.read().history[0].seconds_played, 40);
        fixture.stream.write().active = false;
        app.track_play(t0 + std::time::Duration::from_secs(55));
        assert_eq!(user.read().history[0].seconds_played, 55);
        assert!(app.playing.is_none());
    }

    #[test]
    fn routes_follow_main_qml() {
        assert_eq!(route_for(AuthState::Unknown), Route::Loading);
        assert_eq!(route_for(AuthState::Authenticating), Route::DeviceCode);
        assert_eq!(route_for(AuthState::Authenticated), Route::Library);
        assert_eq!(route_for(AuthState::Unauthenticated), Route::SignIn);
        assert_eq!(route_for(AuthState::Error), Route::SignIn);
    }

    #[test]
    fn hero_falls_back_to_art_url() {
        let mut card = LibraryCard {
            title_id: "T1".into(),
            name: "Halo".into(),
            art_url: "art".into(),
            hero_image_url: "hero".into(),
            ..LibraryCard::default()
        };
        assert_eq!(hero_image_for(&card), "hero");
        card.hero_image_url.clear();
        assert_eq!(hero_image_for(&card), "art");
        assert_eq!(
            play_command(&card),
            StreamCommand::StartCloudStream {
                title_id: "T1".into(),
                title_name: "Halo".into(),
                hero_image: "art".into(),
            }
        );
    }

    #[test]
    fn unknown_state_shows_loading() {
        let (_fixture, mut app) = detached(AuthState::Unknown);
        let mut h = harness(&mut app);
        h.run_steps(2);
        h.get_by_label(LOADING_TEXT);
    }

    #[test]
    fn unauthenticated_and_error_show_sign_in() {
        for state in [AuthState::Unauthenticated, AuthState::Error] {
            let (fixture, mut app) = detached(state);
            let mut h = harness(&mut app);
            h.run_steps(2);
            let text = sign_in_button_text(&fixture.session.read());
            h.get_by_label(text);
        }
    }

    #[test]
    fn authenticating_shows_device_code() {
        let (fixture, mut app) = detached(AuthState::Authenticating);
        fixture.session.write().user_code = "ABCD-EFGH".into();
        let mut h = harness(&mut app);
        h.run_steps(2);
        h.get_by_label("ABCD-EFGH");
    }

    #[test]
    fn authenticated_shows_library_with_gear() {
        let (fixture, mut app) = detached(AuthState::Authenticated);
        *fixture.library.write() = smoke_library();
        let mut h = harness(&mut app);
        h.run_steps(2);
        h.get_by_label("Home");
        h.get_by_label(SETTINGS_LABEL).click();
        h.run_steps(2);
        h.get_by_label(SETTINGS_TITLE);
        drop(h);
        assert!(app.settings_open());
    }

    #[test]
    fn route_switches_when_the_session_changes() {
        let (fixture, mut app) = detached(AuthState::Unknown);
        let mut h = harness(&mut app);
        h.run_steps(1);
        h.get_by_label(LOADING_TEXT);
        fixture.set_auth_state(AuthState::Unauthenticated);
        h.run_steps(1);
        assert!(h.query_by_label(LOADING_TEXT).is_none());
        h.get_by_label("Sign in with Microsoft");
        fixture.set_auth_state(AuthState::Authenticated);
        h.run_steps(1);
        h.get_by_label("Home");
    }

    #[test]
    fn stream_view_overlays_while_active_and_resets_after() {
        let (fixture, mut app) = detached(AuthState::Authenticated);
        {
            let mut stream = fixture.stream.write();
            stream.active = true;
            stream.lifecycle = StreamLifecycleState::ConnectingWebRTC;
            stream.title_name = "Halo".into();
        }
        let mut h = harness(&mut app);
        h.run_steps(2);
        h.get_by_label(SURFACE_LABEL);
        assert!(h.query_by_label("Home").is_none(), "library hidden");

        fixture.stream.write().active = false;
        h.run_steps(2);
        assert!(h.query_by_label(SURFACE_LABEL).is_none());
        h.get_by_label("Home");
    }

    #[test]
    fn play_sends_start_cloud_stream() {
        let (fixture, app) = detached(AuthState::Authenticated);
        let card = LibraryCard {
            title_id: "T2".into(),
            name: "Forza".into(),
            art_url: "art".into(),
            ..LibraryCard::default()
        };
        app.play(&card);
        assert_eq!(
            drain(&fixture.stream_commands),
            [StreamCommand::StartCloudStream {
                title_id: "T2".into(),
                title_name: "Forza".into(),
                hero_image: "art".into(),
            }]
        );
    }

    #[test]
    fn hud_shortcut_toggles_without_a_stream() {
        let (fixture, mut app) = detached(AuthState::Unauthenticated);
        let mut h = harness(&mut app);
        h.run_steps(1);
        h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::I);
        h.run_steps(1);
        assert_eq!(
            drain(&fixture.stream_commands),
            [StreamCommand::SetHudEnabled(true)]
        );
        fixture.stream.write().hud_enabled = true;
        h.key_press_modifiers(Modifiers::CTRL | Modifiers::SHIFT, Key::I);
        h.run_steps(1);
        assert_eq!(
            drain(&fixture.stream_commands),
            [StreamCommand::SetHudEnabled(false)]
        );
    }

    #[test]
    fn controller_settings_changes_are_saved() {
        let dir = temp_path("hud");
        let path = dir.join("settings.json");
        let fixture = Detached::new(AppSettings::default());
        fixture.set_auth_state(AuthState::Unauthenticated);
        let mut app = App::detached(fixture.controllers.clone(), SettingsStore::at(path.clone()));
        let mut h = harness(&mut app);
        h.run_steps(1);
        assert!(!path.exists(), "nothing changed yet");
        // `StreamController::setHudEnabled` writes `hud/enabled` on the shared settings.
        fixture.controllers.settings.write().hud_enabled = true;
        h.run_steps(1);
        assert!(crate::settings::load_from(&path).hud_enabled);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The settings lock is shared with the stream host thread (`setHudEnabled`), so the
    /// per-frame save must not hold it while the store writes the disk.
    #[test]
    fn sync_settings_releases_the_lock_before_writing_the_disk() {
        let dir = temp_path("lock");
        let path = dir.join("settings.json");
        let fixture = Detached::new(AppSettings::default());
        fixture.set_auth_state(AuthState::Unauthenticated);
        let mut store = SettingsStore::at(path.clone());
        let shared = fixture.controllers.settings.clone();
        let writer_blocked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe_hits = std::sync::Arc::new(AtomicU64::new(0));
        store.save_probe = Some(std::sync::Arc::new({
            let shared = shared.clone();
            let writer_blocked = writer_blocked.clone();
            let probe_hits = probe_hits.clone();
            move || {
                probe_hits.fetch_add(1, Ordering::SeqCst);
                // What the stream host does from its own thread: a write that must not
                // wait for the save.
                match shared.try_write() {
                    Some(mut guard) => guard.hud_enabled = true,
                    None => writer_blocked.store(true, Ordering::SeqCst),
                }
            }
        }));
        let mut app = App::detached(fixture.controllers.clone(), store);
        let mut h = harness(&mut app);
        h.run_steps(1);
        fixture.controllers.settings.write().swap_ab = true;
        h.run_steps(1);
        drop(h);
        assert_eq!(
            probe_hits.load(Ordering::SeqCst),
            1,
            "one save for one change"
        );
        assert!(
            !writer_blocked.load(Ordering::SeqCst),
            "a writer was blocked by the UI thread during the disk write"
        );
        let saved = crate::settings::load_from(&path);
        assert!(saved.swap_ab);
        assert!(
            fixture.controllers.settings.try_write().is_some(),
            "the UI frame left the lock free"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_panel_change_is_applied_and_saved() {
        let dir = temp_path("panel");
        let path = dir.join("settings.json");
        let fixture = Detached::new(AppSettings::default());
        fixture.set_auth_state(AuthState::Authenticated);
        *fixture.library.write() = smoke_library();
        let mut app = App::detached(fixture.controllers.clone(), SettingsStore::at(path.clone()));
        app.set_settings_open(true);
        let mut h = harness(&mut app);
        h.run_steps(2);
        h.get_by_label("Controller").click();
        h.run_steps(2);
        h.get_by_label(crate::settings::LABEL_SWAP_AB).click();
        h.run_steps(2);
        drop(h);
        assert!(fixture.controllers.settings.read().swap_ab);
        let commands = drain(&fixture.stream_commands);
        assert!(
            commands.iter().any(|c| matches!(
                c,
                StreamCommand::ApplySettings(s) if s.swap_ab
            )),
            "ApplySettings carries the change: {commands:?}"
        );
        assert!(crate::settings::load_from(&path).swap_ab);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn authenticated_event_updates_credentials_and_refreshes_library() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        let services = Services::with_parts(
            Arc::new(FakeTransport::new()),
            Arc::new(MemoryTokenStore::new()),
            Arc::new(NullTransport::new()),
            runtime.handle().clone(),
        );
        let (fixture, mut app) = detached(AuthState::Authenticated);
        let (events_tx, events_rx) = crossbeam_channel::unbounded();
        app.attach_services(services.clone(), events_rx);
        let tokens = xcloud_api::auth::StreamTokens {
            xcloud_host: "cloud.example".into(),
            xcloud_token: "cloud-token".into(),
            ..Default::default()
        };
        events_tx
            .send(SessionEvent::Authenticated(Box::new(tokens)))
            .expect("send");
        let mut h = harness(&mut app);
        h.run_steps(1);
        drop(h);
        assert_eq!(services.api.base_host(), "cloud.example");
        assert_eq!(services.api.gs_token(), "cloud-token");
        assert_eq!(
            drain(&fixture.library_commands),
            [LibraryCommand::Refresh { force: true }]
        );
    }

    #[test]
    fn smoke_runner_passes_every_screen() {
        let mut runner = SmokeRunner::new();
        let mut h = Harness::builder()
            .with_size(vec2(1360.0, 900.0))
            .with_context_setup(theme::apply)
            .allow_missing_glyphs()
            .build_ui(|ui| runner.step(ui));
        let mut guard = 0;
        while guard < SmokeScreen::ALL.len() * SMOKE_FRAMES_PER_SCREEN + 4 {
            h.run_steps(1);
            guard += 1;
        }
        drop(h);
        assert!(runner.finished(), "ran out of frames");
        assert_eq!(runner.failures(), &[] as &[String]);
    }

    #[test]
    fn smoke_runner_records_a_panic_and_moves_on() {
        let mut runner = SmokeRunner::new();
        runner.panic_on = Some(SmokeScreen::SignIn);
        let mut h = Harness::builder()
            .with_size(vec2(1360.0, 900.0))
            .with_context_setup(theme::apply)
            .allow_missing_glyphs()
            .build_ui(|ui| runner.step(ui));
        let mut guard = 0;
        while guard < SmokeScreen::ALL.len() * SMOKE_FRAMES_PER_SCREEN + 4 {
            h.run_steps(1);
            guard += 1;
        }
        drop(h);
        assert!(runner.finished(), "the run continues past the failure");
        assert_eq!(
            runner.failures(),
            ["SignIn: injected smoke panic".to_owned()]
        );
        assert_eq!(panic_message(&"boom"), "boom");
        assert_eq!(panic_message(&String::from("bang")), "bang");
    }
}
