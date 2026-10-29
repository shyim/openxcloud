//! OpenXCloud application entry point.
//!
//! * `Logging::openLogFile()` → [`init_logging`]: `tracing-subscriber` with `RUST_LOG` to
//!   stderr and to `openxcloud.log` under the data directory. Like the Qt logger
//!   (`g_level = LogLevel::Debug`, Logging.cpp) the default level is `debug`, so the
//!   on-disk log is a full trace of the app's own categories without a debugger attached;
//!   third-party crates (wgpu, hyper, winit, ...) are clamped to `info`/`warn` because the
//!   Qt logger never carried their output.
//! * The `QGuiApplication` + `QQmlApplicationEngine` → one tokio multi-thread runtime that
//!   lives for the whole process and `eframe::run_native` with the wgpu renderer.
//! * `--smoke`: instantiates every screen for a few frames in a real
//!   window on detached snapshots, prints `smoke: N failure(s)` and exits 0/1.
//! * `--present fifo|immediate` picks the surface present mode (default: the persisted
//!   `render/presentMode` setting, `immediate`).
//!
//! On exit the app stops the stream, joins the stream host and drops its transport.
//! The runtime is then shut down with a timeout so stray
//! keepalive tasks cannot keep the process alive.

use std::fs::File;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use parking_lot::Mutex;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use xcloud_app::app::{
    APP_ID, App, DEFAULT_WINDOW_SIZE, MIN_WINDOW_SIZE, SmokeRunner, WINDOW_TITLE,
};
use xcloud_app::services::APP_VERSION;
use xcloud_app::settings::{self, SettingsStore, present_mode_from_name};
use xcloud_app::theme;

const LOG: &str = "App";
/// `Logging::openLogFile`'s default file name.
const LOG_FILE_NAME: &str = "openxcloud.log";
/// Keep app diagnostics at debug while dependencies default to info. Opting app
/// targets in avoids per-packet logs from newly introduced transport crates.
const DEFAULT_FILTER: &str = "info,\
    App=debug,Auth=debug,Session=debug,Library=debug,Artwork=debug,UserData=debug,\
    XCloudAPI=debug,Http=debug,StreamSession=debug,Streaming=debug,\
    StreamController=debug,WebRTC=debug,Video=debug,Audio=debug,\
    Controller=debug,InputChannel=debug,Settings=debug,\
    wgpu_core=warn,wgpu_hal=warn,naga=warn";

/// The filter used when `RUST_LOG` is unset or invalid.
fn default_env_filter() -> EnvFilter {
    EnvFilter::new(DEFAULT_FILTER)
}

#[derive(Debug, Parser)]
#[command(name = "openxcloud", version = APP_VERSION, about = "OpenXCloud: Xbox Cloud Gaming for the desktop")]
struct Cli {
    /// Open a window, instantiate every screen for a few frames on detached snapshots,
    /// print `smoke: N failure(s)` and exit with 0 (none) or 1.
    #[arg(long)]
    smoke: bool,
    /// Surface present mode. Defaults to the persisted setting (`immediate`).
    #[arg(long, value_enum)]
    present: Option<PresentArg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum PresentArg {
    /// Vertical sync: no tearing, up to one frame of added latency.
    Fifo,
    /// Lowest latency; tearing is acceptable in a game stream.
    Immediate,
}

impl From<PresentArg> for wgpu::PresentMode {
    fn from(arg: PresentArg) -> Self {
        match arg {
            PresentArg::Fifo => wgpu::PresentMode::Fifo,
            PresentArg::Immediate => wgpu::PresentMode::Immediate,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let log_path = init_logging();
    tracing::info!(target: LOG, "OpenXCloud {APP_VERSION} starting");
    if let Some(path) = &log_path {
        tracing::info!(target: LOG, "log file {}", path.display());
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("xcloud-app-tokio")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: cannot start the tokio runtime: {error}");
            return ExitCode::FAILURE;
        }
    };

    let store = SettingsStore::open();
    let present_mode = cli.present.map(wgpu::PresentMode::from).unwrap_or_else(|| {
        present_mode_from_name(&store.settings().present_mode)
            .unwrap_or(wgpu::PresentMode::Immediate)
    });
    tracing::info!(target: LOG, "present mode {present_mode:?}");
    let options = native_options(present_mode);

    let code = if cli.smoke {
        run_smoke(options)
    } else {
        run_app(options, runtime.handle().clone(), store)
    };

    // Tasks that outlive the window (keepalive, orchestrator timers) must not keep the
    // process alive.
    runtime.shutdown_timeout(Duration::from_secs(2));
    code
}

/// `Logging::openLogFile` + the stderr sink: installs the global subscriber. Returns the
/// log file path when one could be opened.
fn init_logging() -> Option<PathBuf> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| default_env_filter());
    let stderr = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_writer(std::io::stderr);
    let (file, path) = match open_log_file() {
        Some((file, path)) => (Some(file), Some(path)),
        None => (None, None),
    };
    let file_layer = file.map(|file| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_target(true)
            .with_writer(Arc::new(file))
    });
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stderr)
        .with(file_layer)
        .try_init();
    path
}

/// `<data dir>/openxcloud.log`, falling back to `~/.openxcloud/`.
fn open_log_file() -> Option<(File, PathBuf)> {
    let dir = settings::data_dir()
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".openxcloud")))?;
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "warning: cannot create log directory {}: {error}",
            dir.display()
        );
        return None;
    }
    let path = dir.join(LOG_FILE_NAME);
    match File::create(&path) {
        Ok(file) => Some((file, path)),
        Err(error) => {
            eprintln!("warning: cannot open log file {}: {error}", path.display());
            None
        }
    }
}

/// `wgpu::DeviceDescriptor` for eframe's device: egui's defaults plus every feature
/// `xcloud-gpu` can use when the adapter has it (`TIMESTAMP_QUERY` for the HUD timings).
fn device_descriptor(adapter: &wgpu::Adapter) -> wgpu::DeviceDescriptor<'static> {
    let base_limits = if adapter.get_info().backend == wgpu::Backend::Gl {
        wgpu::Limits::downlevel_webgl2_defaults()
    } else {
        wgpu::Limits::default()
    };
    let desired = wgpu::Features::TIMESTAMP_QUERY;
    let required_features = adapter.features() & desired;
    wgpu::DeviceDescriptor {
        label: Some("openxcloud wgpu device"),
        required_features,
        required_limits: wgpu::Limits {
            max_texture_dimension_2d: 8192,
            ..base_limits
        },
        ..Default::default()
    }
}

/// `ApplicationWindow { width: 1360; height: 900; minimumWidth: 900; minimumHeight: 560 }`
/// with the wgpu renderer and the chosen present mode.
fn native_options(present_mode: wgpu::PresentMode) -> eframe::NativeOptions {
    let mut setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    setup.device_descriptor = Arc::new(device_descriptor);
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(WINDOW_TITLE)
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!(
                    "../../../assets/icons/openxcloud-256.png"
                ))
                .expect("embedded app icon is a valid PNG"),
            )
            .with_app_id(APP_ID)
            .with_inner_size(DEFAULT_WINDOW_SIZE)
            .with_min_inner_size(MIN_WINDOW_SIZE),
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: egui_wgpu::WgpuConfiguration {
            surface: egui_wgpu::SurfaceConfig {
                present_mode,
                desired_maximum_frame_latency: Some(1),
            },
            wgpu_setup: egui_wgpu::WgpuSetup::CreateNew(setup),
            ..Default::default()
        },
        centered: true,
        ..Default::default()
    }
}

fn run_app(
    options: eframe::NativeOptions,
    runtime: tokio::runtime::Handle,
    store: SettingsStore,
) -> ExitCode {
    let result = eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(move |cc| {
            let app = App::launch(cc, runtime, store)?;
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(target: LOG, "eframe failed: {error}");
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// The `--smoke` self-check in a real window: every screen for a few frames.
fn run_smoke(options: eframe::NativeOptions) -> ExitCode {
    let outcome: Arc<Mutex<Option<Vec<String>>>> = Arc::new(Mutex::new(None));
    let shared = Arc::clone(&outcome);
    let result = eframe::run_native(
        "OpenXCloud smoke",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            let mut runner = SmokeRunner::new();
            if let Some(render_state) = &cc.wgpu_render_state {
                runner.app_mut().attach_gpu(render_state);
            }
            Ok(Box::new(SmokeApp {
                runner,
                outcome: shared,
            }) as Box<dyn eframe::App>)
        }),
    );
    let failures = match result {
        Ok(()) => outcome
            .lock()
            .take()
            .unwrap_or_else(|| vec!["window closed before every screen was drawn".to_owned()]),
        Err(error) => vec![format!("eframe failed: {error}")],
    };
    for failure in &failures {
        eprintln!("smoke: {failure}");
    }
    println!("smoke: {} failure(s)", failures.len());
    if failures.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Drives a [`SmokeRunner`] frame by frame and closes the window when it is done.
struct SmokeApp {
    runner: SmokeRunner,
    outcome: Arc<Mutex<Option<Vec<String>>>>,
}

impl eframe::App for SmokeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        egui::Frame::new()
            .fill(theme::BACKGROUND)
            .show(ui, |ui| self.runner.step(ui));
        if self.runner.finished() {
            let mut outcome = self.outcome.lock();
            if outcome.is_none() {
                *outcome = Some(self.runner.failures().to_vec());
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;

    /// App diagnostics stay at debug while dependencies use quieter defaults.
    #[test]
    fn default_log_level_is_debug_for_the_qt_categories() {
        let subscriber = tracing_subscriber::registry().with(default_env_filter());
        // `event_enabled!` builds a static callsite, so the target must be a literal.
        macro_rules! debug_enabled {
            ($($category:literal),+) => {
                $(assert!(
                    tracing::event_enabled!(target: $category, Level::DEBUG),
                    "{} debug must be logged by default", $category
                );)+
            };
        }
        tracing::subscriber::with_default(subscriber, || {
            debug_enabled!("App", "Video", "WebRTC", "Auth", "Controller", "Settings");
            assert!(!tracing::event_enabled!(target: "App", Level::TRACE));
            assert!(!tracing::event_enabled!(target: "wgpu_core::device", Level::DEBUG));
            assert!(!tracing::event_enabled!(target: "wgpu_core::device", Level::INFO));
            assert!(tracing::event_enabled!(target: "wgpu_core::device", Level::WARN));
            assert!(!tracing::event_enabled!(target: "hyper::proto", Level::DEBUG));
            assert!(tracing::event_enabled!(target: "hyper::proto", Level::INFO));
            macro_rules! dependency_quiet {
                ($($target:literal),+) => {
                    $(
                        assert!(!tracing::event_enabled!(target: $target, Level::DEBUG));
                        assert!(!tracing::event_enabled!(target: $target, Level::TRACE));
                        assert!(tracing::event_enabled!(target: $target, Level::WARN));
                        assert!(tracing::event_enabled!(target: $target, Level::ERROR));
                    )+
                };
            }
            dependency_quiet!(
                "rtc::peer_connection::handler::dtls",
                "rtc::peer_connection::handler::sctp",
                "rtc::peer_connection::handler::datachannel",
                "rtc::peer_connection::handler::ice",
                "rtc::peer_connection::handler::srtp",
                "rtc::peer_connection::handler::endpoint",
                "rtc_sctp::association",
                "rtc_dtls::handshaker",
                "webrtc::peer_connection::transport::stun_gatherer",
                "some_future_dependency"
            );
        });
    }

    #[test]
    fn cli_parses_present_mode_and_smoke() {
        let cli = Cli::try_parse_from(["xcloud-app"]).expect("defaults");
        assert!(!cli.smoke);
        assert_eq!(cli.present, None);

        let cli =
            Cli::try_parse_from(["xcloud-app", "--smoke", "--present", "fifo"]).expect("flags");
        assert!(cli.smoke);
        assert_eq!(cli.present, Some(PresentArg::Fifo));
        assert_eq!(
            wgpu::PresentMode::from(PresentArg::Fifo),
            wgpu::PresentMode::Fifo
        );
        assert_eq!(
            wgpu::PresentMode::from(PresentArg::Immediate),
            wgpu::PresentMode::Immediate
        );
        assert!(Cli::try_parse_from(["xcloud-app", "--present", "mailbox"]).is_err());
    }

    #[test]
    fn native_options_follow_main_qml() {
        let options = native_options(wgpu::PresentMode::Fifo);
        assert_eq!(options.viewport.inner_size, Some(egui::vec2(1360.0, 900.0)));
        assert_eq!(
            options.viewport.min_inner_size,
            Some(egui::vec2(900.0, 560.0))
        );
        assert_eq!(options.viewport.title.as_deref(), Some("OpenXCloud"));
        assert_eq!(
            options.wgpu_options.surface.present_mode,
            wgpu::PresentMode::Fifo
        );
        assert!(matches!(options.renderer, eframe::Renderer::Wgpu));
    }
}
