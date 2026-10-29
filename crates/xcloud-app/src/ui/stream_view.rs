//! Port of qt/qml/StreamView.qml (video surface, controls overlay, keyboard virtual
//! controller, focus hint), the `streamLoader` part of qt/qml/Main.qml, and the egui side
//! of qt/src/app/MetalVideoItem.mm (`updatePaintNode`: latest frame → GPU → letterboxed
//! image, `setOutput`).
//!
//! The view reads the [`StreamSnapshot`] once per frame and talks to the controller only
//! through [`StreamCommand`]s. Video frames come from the [`VideoSource`] mailbox
//! (`MailboxReader::latest`, once per frame) and are converted/upscaled by
//! [`xcloud_gpu::VideoRenderer`] inside an [`egui_wgpu::CallbackTrait::prepare`], so the
//! compute passes are encoded into egui's own command encoder ahead of its render pass
//! (PORTING_PLAN §4). The renderer lives in egui's `callback_resources`, i.e. it is created
//! on eframe's device. [`GpuVideo`] is optional: without it (tests, no wgpu) the picture
//! goes through [`CpuVideo`], the port of the `Image { source: "image://videoframe/…" }`
//! path (`VideoFrameProvider`): NV12 → RGBA on the CPU into an egui texture. The view also
//! reverts to that path when the GPU path keeps failing, like `MetalVideoItem` clearing
//! `nativeVideoEnabled` ("Native rendering unavailable; reverting to CPU image output").
//!
//! Keyboard: egui key events are translated with the table of `StreamView.qml` /
//! `qt/README.md` (`xcloud_input::keyboard`) and removed from egui's input so egui does not
//! act on them (Tab focus, Space/Enter activation). Main.qml's window-level
//! `Shortcut { sequences: ["Ctrl+Shift+I", "Meta+Shift+I"] }` toggles the HUD.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use egui::{
    Color32, ColorImage, CornerRadius, EventFilter, Id, Key, KeyboardShortcut, Modifiers, Rect,
    Response, Role, Sense, TextureHandle, TextureOptions, Vec2, ViewportCommand, WidgetInfo, pos2,
    vec2,
};
use egui_wgpu::CallbackTrait;
use parking_lot::RwLock;
use tracing::{info, warn};
use xcloud_gpu::{UpscalePolicy, Upscaler, VideoRenderer, VideoTexture};
use xcloud_input::keyboard::{KeyAction, key_action, left_stick_from_keys};
use xcloud_media::{ColorInfo, Frame};

use super::hud::StreamHud;
use super::icons;
use super::kit;
use super::widgets::{self, ArtworkKind, ArtworkSource, artwork_request_width, aspect_fill_uv};
use crate::state::{
    AppSettings, StreamCommand, StreamHandle, StreamSnapshot, TriggerSide, VideoSource,
};
use crate::theme;

const LOG: &str = "Video";

/// `Timer { interval: 3000 }`: controls hide this long after they were shown.
pub const CONTROLS_HIDE_AFTER_SECS: f64 = 3.0;
/// `anchors.margins: 20` of the two control rows.
const OVERLAY_MARGIN: f32 = 20.0;
/// `Row.spacing: 10`.
const OVERLAY_SPACING: f32 = 10.0;
/// `RoundIcon.size: 44`.
const ICON_SIZE: f32 = 44.0;
/// Focus hint pill: `topMargin: 76`, `height: 48`, `width: text + 36`.
const HINT_TOP: f32 = 76.0;
const HINT_HEIGHT: f32 = 48.0;
const HINT_PADDING: f32 = 36.0;
/// `RemoteImage { opacity: 0.45 }` of the hero artwork.
const HERO_OPACITY: f32 = 0.55;
/// How often timing-only changes (`gpu_ms`, `frame_age_ms`, `present_ms`) are reported
/// with `SetOutput`; size/upscale changes go out immediately.
const OUTPUT_REPORT_INTERVAL_SECS: f64 = 0.5;
/// The text shown next to the HUD toggle when no physical controller is connected.
pub const KEYBOARD_HINT: &str = "Keyboard · WASD / R F triggers";
/// Accessible labels (`RoundIcon.label`), also what the tests query.
pub const CLOSE_LABEL: &str = "Close stream";
pub const HUD_LABEL: &str = "Performance overlay";
pub const FULLSCREEN_LABEL: &str = "Toggle full screen";
pub const FOCUS_HINT_TEXT: &str = "Click to enable controls";
pub const SURFACE_LABEL: &str = "Stream surface";

/// Main.qml: `Shortcut { sequences: ["Ctrl+Shift+I", "Meta+Shift+I"] }` toggles
/// `stream.hudEnabled`. Qt maps `Ctrl` to ⌘ and `Meta` to ⌃ on macOS, so both the
/// platform command key and the control key work everywhere (egui's `COMMAND` is ⌘ on
/// macOS and Ctrl elsewhere; `CTRL` is the physical control key).
pub const HUD_SHORTCUTS: [KeyboardShortcut; 2] = [
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::I),
    KeyboardShortcut::new(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::I),
];

/// Consecutive GPU failures (`VideoRenderer::prepare` / `ensure_targets`) after which the
/// view reverts to the CPU image path. `MetalVideoItem::updatePaintNode` reverts on the
/// first failed `render`; the port tolerates a couple so one bad import does not condemn
/// the device for the rest of the stream.
pub const GPU_FALLBACK_AFTER_FAILURES: u64 = 3;

/// Renderers replaced by a settings change are kept until the GPU finished with their
/// frames; beyond this many (rapid toggling) the oldest is dropped anyway.
const MAX_RETIRED_RENDERERS: usize = 3;

// ----------------------------------------------------------------------------- gpu

/// What [`GpuVideo::prepare_frame`] / [`CpuVideo::prepare_frame`] learnt about the output.
#[derive(Debug, Clone, Copy, PartialEq)]
struct VideoOutput {
    width: u32,
    height: u32,
    upscaled: bool,
    /// `VideoRenderer::upscaler_label`: `"FSR"`, `"FX"` or `""` (bypass, CPU path).
    upscaler_label: &'static str,
    gpu_ms: f64,
    /// Decoder output → this frame being prepared ("frame age at prepare" in docs/perf.md).
    frame_age_ms: f64,
    /// Decoder output → GPU completion of the last finished frame
    /// (`VideoRenderer::last_present_latency_ms`): an upper bound quantised to the UI
    /// frame period, 0 on the CPU path.
    present_ms: f64,
}

/// Which path draws the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoPath {
    /// `xcloud_gpu::VideoRenderer` inside egui's wgpu callback (`MetalVideoItem`).
    Gpu,
    /// CPU conversion into an egui texture (`image://videoframe`).
    Cpu,
}

/// Failure bookkeeping shared between [`GpuVideo`] and the [`VideoCallback`]s it issues.
/// The callback runs inside egui's render pass, after the view's `show`, so the view sees a
/// failure one frame later.
#[derive(Debug, Default)]
struct GpuHealth {
    /// Frames whose GPU work failed so far.
    errors: AtomicU64,
    /// Failures since the last success; [`GPU_FALLBACK_AFTER_FAILURES`] triggers the fallback.
    consecutive: AtomicU64,
}

impl GpuHealth {
    /// Records one failure and returns the consecutive count including it.
    fn record_failure(&self) -> u64 {
        self.errors.fetch_add(1, Ordering::Relaxed);
        self.consecutive.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn record_success(&self) {
        self.consecutive.store(0, Ordering::Relaxed);
    }

    fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    fn consecutive_failures(&self) -> u64 {
        self.consecutive.load(Ordering::Relaxed)
    }

    fn should_fall_back(&self) -> bool {
        self.consecutive_failures() >= GPU_FALLBACK_AFTER_FAILURES
    }

    fn reset(&self) {
        self.errors.store(0, Ordering::Relaxed);
        self.consecutive.store(0, Ordering::Relaxed);
    }
}

/// Runs the renderer inside egui's `prepare` so the compute passes land in egui's command
/// encoder, ahead of its render pass, on the same queue (see `xcloud-gpu/examples/replay.rs`).
struct VideoCallback {
    frame: Arc<Frame>,
    health: Arc<GpuHealth>,
}

impl VideoCallback {
    fn fail(&self, what: &dyn std::fmt::Display) {
        let consecutive = self.health.record_failure();
        let total = self.health.errors();
        if total == 1 || total.is_multiple_of(300) || consecutive == GPU_FALLBACK_AFTER_FAILURES {
            warn!(
                target: LOG,
                "video frame prepare failed ({total} so far, {consecutive} in a row): {what}"
            );
        }
    }
}

impl CallbackTrait for VideoCallback {
    fn prepare(
        &self,
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(renderer) = resources.get_mut::<VideoRenderer>() else {
            self.fail(&"no video renderer in egui's callback resources");
            return Vec::new();
        };
        match renderer.prepare(encoder, &self.frame) {
            Ok(_) => self.health.record_success(),
            Err(e) => self.fail(&e),
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        _render_pass: &mut wgpu::RenderPass<'static>,
        _resources: &egui_wgpu::CallbackResources,
    ) {
        // The picture is drawn by egui itself through the registered native texture.
    }
}

/// The wgpu side of the stream view: owns the egui texture binding and keeps the
/// [`VideoRenderer`] in egui's `callback_resources` (so it is created on eframe's device).
/// Build it from `eframe::CreationContext::wgpu_render_state` /
/// `eframe::Frame::wgpu_render_state`.
pub struct GpuVideo {
    render_state: egui_wgpu::RenderState,
    texture: VideoTexture,
    upscale_enabled: bool,
    upscale_height: u32,
    sharpness: f32,
    /// The upscaler requested of the live renderer (`AppSettings::upscaler`, or
    /// `OPENXCLOUD_UPSCALER` when set).
    upscaler: Upscaler,
    /// `OPENXCLOUD_UPSCALER`, read once: it overrides the setting for the whole run.
    env_upscaler: Option<Upscaler>,
    health: Arc<GpuHealth>,
    /// Renderers replaced by [`GpuVideo::apply_settings`], kept alive (their imported
    /// frames with them) until the GPU finished with their last submission, instead of
    /// draining the device on the UI thread (PORTING_PLAN §4: no `waitUntilCompleted`).
    retired: Vec<VideoRenderer>,
}

impl GpuVideo {
    /// Creates the renderer on `render_state`'s device with the upscaler policy from the
    /// environment (`OPENXCLOUD_DISABLE_UPSCALE`, `OPENXCLOUD_UPSCALER`) and `settings`, and registers
    /// it with egui.
    pub fn new(render_state: &egui_wgpu::RenderState, settings: &AppSettings) -> Self {
        let mut this = Self {
            render_state: render_state.clone(),
            texture: VideoTexture::new(),
            upscale_enabled: settings.upscale_enabled,
            upscale_height: crate::settings::normalize_upscale_height(settings.upscale_height),
            sharpness: settings.sharpness,
            upscaler: Upscaler::default(),
            env_upscaler: Upscaler::requested_from_env(),
            health: Arc::new(GpuHealth::default()),
            retired: Vec::new(),
        };
        this.install_renderer(settings);
        this
    }

    fn policy(settings: &AppSettings) -> UpscalePolicy {
        let env = UpscalePolicy::from_env();
        UpscalePolicy {
            enabled: env.enabled && settings.upscale_enabled,
            max_height: crate::settings::normalize_upscale_height(settings.upscale_height),
            max_width: crate::settings::normalize_upscale_height(settings.upscale_height) * 16 / 9,
        }
    }

    /// `OPENXCLOUD_UPSCALER` when set, otherwise `AppSettings::upscaler` (unknown names → FSR,
    /// like `settings::normalize_upscaler`).
    fn requested_upscaler(&self, settings: &AppSettings) -> Upscaler {
        self.env_upscaler
            .unwrap_or_else(|| Upscaler::parse(&settings.upscaler).unwrap_or_default())
    }

    fn install_renderer(&mut self, settings: &AppSettings) {
        let upscaler = self.requested_upscaler(settings);
        let mut renderer = VideoRenderer::with_upscaler(
            &self.render_state.device,
            &self.render_state.queue,
            Self::policy(settings),
            upscaler,
        );
        renderer.set_sharpness(settings.sharpness);
        info!(
            target: LOG,
            "video renderer ready: upscale={} upscaler={} sharpness={:.2} zero-copy={}",
            renderer.policy().enabled,
            upscaler.as_str(),
            renderer.sharpness(),
            renderer.importer().zero_copy_available()
        );
        let mut egui_renderer = self.render_state.renderer.write();
        self.texture.free(&mut egui_renderer);
        if let Some(old) = egui_renderer.callback_resources.insert(renderer) {
            drop(egui_renderer);
            self.retire(old);
        }
        self.upscale_enabled = settings.upscale_enabled;
        self.upscale_height = crate::settings::normalize_upscale_height(settings.upscale_height);
        self.upscaler = upscaler;
        self.sharpness = settings.sharpness;
    }

    /// Parks a replaced renderer until its in-flight frames are done (see
    /// [`GpuVideo::after_submit`]). Nothing to wait for: dropped right away.
    fn retire(&mut self, old: VideoRenderer) {
        if old.in_flight() == 0 {
            return;
        }
        self.retired.push(old);
        if self.retired.len() > MAX_RETIRED_RENDERERS {
            warn!(
                target: LOG,
                "{} retired video renderers still have GPU work in flight; dropping the oldest",
                self.retired.len()
            );
            self.retired.remove(0);
        }
    }

    /// Applies live settings: sharpness and the upscaler kind (FSR / MetalFX) are updated
    /// in place (`VideoRenderer::set_upscaler` rebuilds its targets on the next frame),
    /// toggling the upscaler on/off rebuilds the renderer (its targets depend on the
    /// policy). The old renderer is retired, not waited for: the UI thread never drains
    /// the GPU.
    fn apply_settings(&mut self, settings: &AppSettings) {
        if settings.upscale_enabled != self.upscale_enabled
            || crate::settings::normalize_upscale_height(settings.upscale_height)
                != self.upscale_height
        {
            self.install_renderer(settings);
            return;
        }
        let upscaler = self.requested_upscaler(settings);
        if upscaler == self.upscaler && settings.sharpness == self.sharpness {
            return;
        }
        let mut egui_renderer = self.render_state.renderer.write();
        if let Some(vr) = egui_renderer.callback_resources.get_mut::<VideoRenderer>() {
            vr.set_sharpness(settings.sharpness);
            if upscaler != self.upscaler {
                info!(target: LOG, "upscaler → {}", upscaler.as_str());
                vr.set_upscaler(upscaler);
            }
        }
        self.upscaler = upscaler;
        self.sharpness = settings.sharpness;
    }

    /// The previous frame's encoder has been submitted by the time the next `show` runs:
    /// release what the GPU finished and advance the timestamp readback. Retired renderers
    /// are released once their last submission completed (a non-blocking poll lets wgpu
    /// run the completion callbacks).
    fn after_submit(&mut self) {
        {
            let mut egui_renderer = self.render_state.renderer.write();
            if let Some(vr) = egui_renderer.callback_resources.get_mut::<VideoRenderer>() {
                vr.after_submit();
            }
        }
        if self.retired.is_empty() {
            return;
        }
        let _ = self.render_state.device.poll(wgpu::PollType::Poll);
        for old in &mut self.retired {
            old.after_submit();
        }
        self.retired.retain(|old| old.in_flight() > 0);
    }

    /// Renderers replaced by a settings change whose GPU work has not finished yet.
    pub fn retired_renderers(&self) -> usize {
        self.retired.len()
    }

    /// Whether the live renderer runs the upscaler.
    pub fn upscale_enabled(&self) -> bool {
        self.upscale_enabled
    }

    /// The upscaler requested of the live renderer (what actually runs may be FSR when
    /// MetalFX is unavailable; the HUD label says which).
    pub fn upscaler(&self) -> Upscaler {
        self.upscaler
    }

    /// Unbinds the output texture from egui (the CPU path draws its own texture).
    fn release_texture(&mut self) {
        let mut egui_renderer = self.render_state.renderer.write();
        self.texture.free(&mut egui_renderer);
    }

    /// Makes sure the output texture exists for `frame`'s size and is bound to an egui
    /// texture id, and returns what the HUD / `setOutput` need.
    fn prepare_frame(&mut self, frame: &Frame) -> Option<VideoOutput> {
        let mut egui_renderer = self.render_state.renderer.write();
        let vr = egui_renderer
            .callback_resources
            .get_mut::<VideoRenderer>()?;
        if let Err(e) = vr.ensure_targets(frame.width, frame.height) {
            warn!(target: LOG, "ensure_targets({}x{}): {e}", frame.width, frame.height);
            return None;
        }
        let (width, height) = vr.output_size()?;
        let output = VideoOutput {
            width,
            height,
            upscaled: vr.is_upscaled(),
            upscaler_label: vr.upscaler_label(),
            gpu_ms: vr.gpu_timings().map_or(0.0, |t| t.total_ms()),
            frame_age_ms: frame.t_decoded.elapsed().as_secs_f64() * 1e3,
            present_ms: vr.last_present_latency_ms().unwrap_or(0.0),
        };
        let view = vr.display_view().cloned()?;
        let generation = vr.output_generation();
        self.texture.sync(
            &mut egui_renderer,
            &self.render_state.device,
            &view,
            generation,
        );
        Some(output)
    }

    /// The paint callback that encodes `frame` this egui frame.
    fn callback(&self, rect: Rect, frame: Arc<Frame>) -> egui::epaint::PaintCallback {
        egui_wgpu::Callback::new_paint_callback(
            rect,
            VideoCallback {
                frame,
                health: Arc::clone(&self.health),
            },
        )
    }

    /// Draws the latest output letterboxed into `available`.
    fn draw(&self, painter: &egui::Painter, available: Rect, output: VideoOutput) -> Rect {
        self.texture
            .draw(painter, available, output.width, output.height)
    }

    /// Frames whose GPU work failed so far.
    pub fn prepare_errors(&self) -> u64 {
        self.health.errors()
    }

    /// Failures since the last successful frame.
    pub fn consecutive_failures(&self) -> u64 {
        self.health.consecutive_failures()
    }
}

// ----------------------------------------------------------------------------- cpu

/// Why a frame could not be converted on the CPU.
#[derive(Debug, thiserror::Error)]
pub enum CpuVideoError {
    #[error("frame is {width}x{height}")]
    EmptyFrame { width: u32, height: u32 },
    #[error("NV12 plane too short: {plane} has {len} bytes, needs {needed}")]
    ShortPlane {
        plane: &'static str,
        len: usize,
        needed: usize,
    },
    #[error("frame has no CPU-readable pixels")]
    NoPixels,
    #[error("reading the frame back failed: {0}")]
    Readback(String),
}

/// Borrowed NV12 planes (`Frame::nv12_planes`): `y` is `y_stride` bytes per row, `uv`
/// interleaved Cb/Cr at half resolution, `uv_stride` bytes per row.
#[derive(Debug, Clone, Copy)]
pub struct Nv12Planes<'a> {
    pub y: &'a [u8],
    pub uv: &'a [u8],
    pub y_stride: usize,
    pub uv_stride: usize,
}

/// NV12 → RGBA with the arithmetic of the `convert_video` kernel / WGSL
/// (`ColorInfo::shader_coefficients`), writing `width * height` pixels into `out` (cleared
/// first). Chroma is taken from the co-sited 2×2 block (nearest), where the shader samples
/// bilinearly; the CPU path is a fallback, not the reference.
pub fn nv12_to_rgba(
    planes: Nv12Planes<'_>,
    width: u32,
    height: u32,
    color: &ColorInfo,
    out: &mut Vec<Color32>,
) -> Result<(), CpuVideoError> {
    let Nv12Planes {
        y,
        uv,
        y_stride,
        uv_stride,
    } = planes;
    if width == 0 || height == 0 {
        return Err(CpuVideoError::EmptyFrame { width, height });
    }
    let (w, h) = (width as usize, height as usize);
    let chroma_rows = h.div_ceil(2);
    let chroma_bytes = w.div_ceil(2) * 2;
    let y_needed = (h - 1) * y_stride + w;
    if y_stride < w || y.len() < y_needed {
        return Err(CpuVideoError::ShortPlane {
            plane: "Y",
            len: y.len(),
            needed: y_needed,
        });
    }
    let uv_needed = (chroma_rows - 1) * uv_stride + chroma_bytes;
    if uv_stride < chroma_bytes || uv.len() < uv_needed {
        return Err(CpuVideoError::ShortPlane {
            plane: "CbCr",
            len: uv.len(),
            needed: uv_needed,
        });
    }

    let [y_offset, y_scale, c_scale, _, r_cr, g_cb, g_cr, b_cb] = color.shader_coefficients();
    let luma: Vec<f32> = (0..=255u32)
        .map(|v| (v as f32 / 255.0 - y_offset) * y_scale)
        .collect();
    let chroma: Vec<f32> = (0..=255u32)
        .map(|v| (v as f32 / 255.0 - 0.5) * c_scale)
        .collect();
    let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;

    out.clear();
    out.reserve(w * h);
    for row in 0..h {
        let y_row = &y[row * y_stride..row * y_stride + w];
        let uv_row = &uv[(row / 2) * uv_stride..(row / 2) * uv_stride + chroma_bytes];
        for (x, &l) in y_row.iter().enumerate() {
            let l = luma[usize::from(l)];
            let cb = chroma[usize::from(uv_row[(x / 2) * 2])];
            let cr = chroma[usize::from(uv_row[(x / 2) * 2 + 1])];
            out.push(Color32::from_rgb(
                to_u8(l + r_cr * cr),
                to_u8(l + g_cb * cb + g_cr * cr),
                to_u8(l + b_cb * cb),
            ));
        }
    }
    Ok(())
}

/// [`nv12_to_rgba`] for a whole [`Frame`]: CPU planes directly, an IOSurface-backed frame
/// through `Frame::to_nv12_cpu` (the `av_hwframe_transfer_data` read-back of the Qt
/// fallback).
fn convert_frame(frame: &Frame, out: &mut Vec<Color32>) -> Result<(), CpuVideoError> {
    if let Some((y, uv, y_stride, uv_stride)) = frame.nv12_planes() {
        let planes = Nv12Planes {
            y,
            uv,
            y_stride,
            uv_stride,
        };
        return nv12_to_rgba(planes, frame.width, frame.height, &frame.color, out);
    }
    #[cfg(target_os = "macos")]
    {
        let buf = frame
            .to_nv12_cpu()
            .map_err(|e| CpuVideoError::Readback(e.to_string()))?;
        if let xcloud_media::NativeBuffer::Nv12Cpu {
            y,
            uv,
            y_stride,
            uv_stride,
        } = &buf
        {
            let planes = Nv12Planes {
                y,
                uv,
                y_stride: *y_stride,
                uv_stride: *uv_stride,
            };
            return nv12_to_rgba(planes, frame.width, frame.height, &frame.color, out);
        }
    }
    Err(CpuVideoError::NoPixels)
}

/// The CPU image path: `Image { source: "image://videoframe/frame?" + serial }` fed by
/// `VideoFrameProvider`. Converts each new frame on the UI thread and uploads it as an egui
/// texture; drawn letterboxed like the GPU output.
#[derive(Default)]
struct CpuVideo {
    texture: Option<TextureHandle>,
    pixels: Vec<Color32>,
    failures: u64,
    /// Wall time of the last conversion, for the log.
    convert_ms: f64,
}

impl CpuVideo {
    /// Converts and uploads `frame` when it is new (or nothing is uploaded yet) and returns
    /// what `setOutput` / the HUD need. `None` when the frame cannot be converted.
    fn prepare_frame(
        &mut self,
        ctx: &egui::Context,
        frame: &Frame,
        is_new: bool,
    ) -> Option<VideoOutput> {
        if is_new || self.texture.is_none() {
            let t0 = Instant::now();
            if let Err(e) = convert_frame(frame, &mut self.pixels) {
                self.failures += 1;
                if self.failures == 1 || self.failures.is_multiple_of(300) {
                    warn!(target: LOG, "CPU video conversion failed ({} so far): {e}", self.failures);
                }
                return None;
            }
            let size = [frame.width as usize, frame.height as usize];
            let image = ColorImage::new(size, std::mem::take(&mut self.pixels));
            match &mut self.texture {
                Some(texture) => texture.set(image, TextureOptions::LINEAR),
                None => {
                    info!(
                        target: LOG,
                        "CPU video output {}x{}", frame.width, frame.height
                    );
                    self.texture =
                        Some(ctx.load_texture("xcloud-video-cpu", image, TextureOptions::LINEAR));
                }
            }
            self.convert_ms = t0.elapsed().as_secs_f64() * 1e3;
        }
        self.texture.as_ref()?;
        Some(VideoOutput {
            width: frame.width,
            height: frame.height,
            upscaled: false,
            upscaler_label: "",
            gpu_ms: 0.0,
            frame_age_ms: frame.t_decoded.elapsed().as_secs_f64() * 1e3,
            present_ms: 0.0,
        })
    }

    /// Draws the uploaded frame letterboxed into `available`.
    fn draw(&self, painter: &egui::Painter, available: Rect, output: VideoOutput) -> Rect {
        let rect = xcloud_gpu::letterbox(available, output.width, output.height);
        if let Some(texture) = &self.texture {
            painter.image(
                texture.id(),
                rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        rect
    }

    /// Frees the texture (`VideoFrameBuffer::clear`).
    fn release(&mut self) {
        self.texture = None;
        self.pixels = Vec::new();
    }
}

/// Outcome of one GPU attempt in [`StreamView::show_video`].
enum GpuStep {
    Drawn(VideoOutput),
    /// Failed this frame; keep trying.
    Failed,
    /// Failing persistently: switch to the CPU path.
    FallBack,
}

// ----------------------------------------------------------------------------- keyboard

/// `StreamView.keyName(event)`: the QML spelling of an egui key, `None` for keys the
/// table does not know. Named keys as Qt spells them, letters upper-cased.
pub fn qml_key_name(key: Key) -> Option<&'static str> {
    Some(match key {
        Key::Space => "Space",
        Key::Enter => "Return",
        Key::Escape => "Escape",
        Key::Backspace => "Backspace",
        Key::ArrowLeft => "Left",
        Key::ArrowRight => "Right",
        Key::ArrowUp => "Up",
        Key::ArrowDown => "Down",
        Key::ShiftLeft | Key::ShiftRight => "Shift",
        Key::ControlLeft | Key::ControlRight => "Control",
        Key::Tab => "Tab",
        Key::Q => "Q",
        Key::E => "E",
        Key::Z => "Z",
        Key::C => "C",
        Key::M => "M",
        Key::R => "R",
        Key::F => "F",
        Key::W => "W",
        Key::A => "A",
        Key::S => "S",
        Key::D => "D",
        _ => return None,
    })
}

/// A translated key event: the QML key name and whether it went down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KeyEvent {
    name: &'static str,
    pressed: bool,
}

/// Pulls the stream view's keys out of egui's input (so egui never sees them) and returns
/// them in order. Key repeats are dropped but still consumed. `physical_key` is the
/// fallback for keys without a logical mapping (the modifier keys).
fn take_key_events(input: &mut egui::InputState) -> Vec<KeyEvent> {
    let mut taken = Vec::new();
    input.events.retain(|event| {
        let egui::Event::Key {
            key,
            physical_key,
            pressed,
            repeat,
            ..
        } = event
        else {
            return true;
        };
        let name = qml_key_name(*key).or_else(|| physical_key.and_then(qml_key_name));
        let Some(name) = name.filter(|n| key_action(n).is_some()) else {
            return true;
        };
        if !(*pressed && *repeat) {
            taken.push(KeyEvent {
                name,
                pressed: *pressed,
            });
        }
        false
    });
    taken
}

// ----------------------------------------------------------------------------- view

/// What the view last told the controller with `SetOutput`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct OutputReport {
    height: u32,
    upscaled: bool,
    upscaler_label: &'static str,
    gpu_ms: f64,
    frame_age_ms: f64,
    present_ms: f64,
    at: f64,
}

/// The full-screen stream surface Main.qml's `streamLoader` shows while `stream.active`.
pub struct StreamView {
    stream: StreamHandle,
    video: VideoSource,
    settings: Arc<RwLock<AppSettings>>,
    hud: StreamHud,
    gpu: Option<GpuVideo>,
    cpu: CpuVideo,
    /// `!nativeVideoEnabled`: the GPU path failed persistently, the CPU path draws now.
    cpu_fallback: bool,
    id: Id,
    /// `root.showControls`.
    show_controls: bool,
    /// egui time at which the hide timer (re)started.
    controls_since: f64,
    was_connected: bool,
    /// `root.pressedKeys`: held WASD keys, by QML name.
    pressed_keys: BTreeSet<&'static str>,
    /// `root.inputFocused` as last reported; `None` before the first frame
    /// (`Component.onCompleted` has not run yet).
    input_focused: Option<bool>,
    /// Identity of the frame last handed to the GPU, so repaints for other reasons do not
    /// re-encode it.
    last_frame: Option<(usize, Instant)>,
    last_output: Option<VideoOutput>,
    last_report: Option<OutputReport>,
}

impl StreamView {
    pub fn new(
        stream: StreamHandle,
        video: VideoSource,
        settings: Arc<RwLock<AppSettings>>,
    ) -> Self {
        Self {
            hud: StreamHud::new(stream.clone()),
            stream,
            video,
            settings,
            gpu: None,
            cpu: CpuVideo::default(),
            cpu_fallback: false,
            id: Id::unique("xcloud-stream-view"),
            show_controls: true,
            controls_since: 0.0,
            was_connected: false,
            pressed_keys: BTreeSet::new(),
            input_focused: None,
            last_frame: None,
            last_output: None,
            last_report: None,
        }
    }

    /// Attaches (or detaches) the wgpu path. Tests leave it `None`.
    pub fn with_gpu(mut self, gpu: Option<GpuVideo>) -> Self {
        self.gpu = gpu;
        self
    }

    pub fn set_gpu(&mut self, gpu: Option<GpuVideo>) {
        self.gpu = gpu;
    }

    pub fn gpu(&self) -> Option<&GpuVideo> {
        self.gpu.as_ref()
    }

    /// `root.showControls`.
    pub fn controls_visible(&self) -> bool {
        self.show_controls
    }

    /// `root.inputFocused` as last reported to the controller.
    pub fn input_focused(&self) -> bool {
        self.input_focused.unwrap_or(false)
    }

    /// Output size of the last prepared frame (`(width, height)`), if any was drawn.
    pub fn output_size(&self) -> Option<(u32, u32)> {
        self.last_output.map(|o| (o.width, o.height))
    }

    /// Which path draws the picture: the GPU when one is attached and healthy, else the
    /// CPU image path.
    pub fn video_path(&self) -> VideoPath {
        if self.gpu.is_none() || self.cpu_fallback {
            VideoPath::Cpu
        } else {
            VideoPath::Gpu
        }
    }

    /// `!nativeVideoEnabled`: a GPU is attached but the view reverted to CPU conversion.
    pub fn cpu_fallback(&self) -> bool {
        self.cpu_fallback
    }

    /// The QML loader destroys the view when `stream.active` drops; call this when the
    /// view is hidden so the next launch starts like a fresh `StreamView` (controls shown,
    /// no held keys, focus reported again). The GPU resources stay; the CPU texture is
    /// released and a GPU fallback is forgotten so the next launch tries the GPU again
    /// (`MetalVideoItem` kept `nativeVideoEnabled == false` for the whole process; here
    /// the failure may have been specific to one stream's frames).
    pub fn reset(&mut self) {
        self.hud.reset();
        self.show_controls = true;
        self.was_connected = false;
        self.pressed_keys.clear();
        self.input_focused = None;
        self.last_frame = None;
        self.last_output = None;
        self.last_report = None;
        self.cpu.release();
        self.cpu_fallback = false;
        if let Some(gpu) = &self.gpu {
            gpu.health.reset();
        }
    }

    /// Draws the view into `ui`'s whole `max_rect`. Call it before anything else that
    /// handles keyboard input in the frame (it removes the controller keys from egui's
    /// input), ideally as the only content while `stream.active`.
    pub fn show(&mut self, ui: &mut egui::Ui, artwork: &dyn ArtworkSource) {
        let ctx = ui.ctx().clone();
        let snapshot = self.stream.read();
        let settings = self.settings.read().clone();
        let area = ui.max_rect();
        let now = ctx.input(|i| i.time);
        let connected = snapshot.connected();

        self.update_focus(&ctx);
        // Shortcut { sequences: ["Ctrl+Shift+I", "Meta+Shift+I"] }: stream.hudEnabled = !stream.hudEnabled.
        if ctx.input_mut(|i| HUD_SHORTCUTS.iter().any(|s| i.consume_shortcut(s))) {
            self.stream
                .send(StreamCommand::SetHudEnabled(!snapshot.hud_enabled));
        }
        if self.input_focused() {
            self.handle_keyboard(&ctx);
        }

        // Timer { running: stream.connected && root.showControls }.
        if connected && !self.was_connected {
            self.controls_since = now;
        }
        self.was_connected = connected;
        if connected && self.show_controls {
            let elapsed = now - self.controls_since;
            if elapsed >= CONTROLS_HIDE_AFTER_SECS {
                self.show_controls = false;
            } else {
                ctx.request_repaint_after(Duration::from_secs_f64(
                    CONTROLS_HIDE_AFTER_SECS - elapsed,
                ));
            }
        }

        // Rectangle { color: Theme.background }.
        ui.painter().rect_filled(area, 0.0, theme::BACKGROUND);

        self.show_video(ui, area, &settings);

        if !connected {
            self.show_placeholder(ui, area, &snapshot, artwork);
        }

        // MouseArea { z: 1 }: click activates the window, focuses the view and toggles the
        // controls. Added before the buttons so they win the hit test.
        let surface = ui.interact(area, self.surface_id(), Sense::click());
        surface.widget_info(|| WidgetInfo::labeled(Role::Group, true, SURFACE_LABEL));
        if surface.clicked() {
            self.request_activate(&ctx);
            if connected {
                self.show_controls = !self.show_controls;
                if self.show_controls {
                    self.controls_since = now;
                }
            }
        }
        self.keep_surface_focused(&ctx);

        if self.show_controls || !connected {
            self.show_controls_overlay(ui, area, &snapshot, &ctx);
        }

        self.hud
            .show_snapshot_for_path(ui, area, &snapshot, self.cpu_fallback);

        if connected && !self.input_focused() {
            self.show_focus_hint(ui, area, &ctx);
        }
    }

    fn surface_id(&self) -> Id {
        self.id.with("surface")
    }

    /// `Window.window.requestActivate()` + `root.forceActiveFocus()`.
    fn request_activate(&self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(ViewportCommand::Focus);
        let id = self.surface_id();
        ctx.memory_mut(|m| m.request_focus(id));
    }

    /// Keeps egui's keyboard focus on the surface while the window is focused, and locks
    /// Tab / arrows / Escape so egui's focus navigation never reacts to the controller keys.
    fn keep_surface_focused(&self, ctx: &egui::Context) {
        if !self.input_focused() {
            return;
        }
        let id = self.surface_id();
        ctx.memory_mut(|m| {
            if m.focused().is_none() {
                m.request_focus(id);
            }
            if m.has_focus(id) {
                m.set_focus_lock_filter(
                    id,
                    EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                );
            }
        });
    }

    /// `readonly property bool inputFocused: visible && Window.window.active` and its
    /// change handler: report it, release everything on loss, take focus on gain.
    fn update_focus(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        if self.input_focused == Some(focused) {
            return;
        }
        let first = self.input_focused.is_none();
        self.input_focused = Some(focused);
        self.stream.send(StreamCommand::SetInputFocused(focused));
        if focused {
            // Component.onCompleted: requestActivate(); forceActiveFocus().
            self.request_activate(ctx);
        } else {
            self.pressed_keys.clear();
            if !first {
                self.stream.send(StreamCommand::ReleaseAll);
            }
        }
    }

    /// `Keys.onPressed` / `Keys.onReleased`.
    fn handle_keyboard(&mut self, ctx: &egui::Context) {
        let events = ctx.input_mut(take_key_events);
        for event in events {
            match key_action(event.name) {
                Some(KeyAction::Button("left-trigger")) => {
                    self.stream.send(StreamCommand::SetTrigger {
                        side: TriggerSide::Left,
                        value: if event.pressed { 1.0 } else { 0.0 },
                    });
                }
                Some(KeyAction::Button("right-trigger")) => {
                    self.stream.send(StreamCommand::SetTrigger {
                        side: TriggerSide::Right,
                        value: if event.pressed { 1.0 } else { 0.0 },
                    });
                }
                Some(KeyAction::Button(name)) => {
                    self.stream.send(StreamCommand::SetKey {
                        name: name.to_string(),
                        pressed: event.pressed,
                    });
                }
                Some(KeyAction::LeftStick(_)) => {
                    if event.pressed {
                        self.pressed_keys.insert(event.name);
                    } else {
                        self.pressed_keys.remove(event.name);
                    }
                    self.update_left_stick();
                }
                None => {}
            }
        }
    }

    /// `updateLeftStick()`: the summed deflection of every held WASD key (y up).
    fn update_left_stick(&self) {
        let stick = left_stick_from_keys(self.pressed_keys.iter().copied());
        self.stream.send(StreamCommand::SetLeftStick {
            x: f64::from(stick.x),
            y: f64::from(stick.y),
        });
    }

    /// `MetalVideoItem::updatePaintNode`: latest frame → renderer → letterboxed image, and
    /// `setOutput` when the result changed. Falls back to the CPU image path when the GPU
    /// path failed [`GPU_FALLBACK_AFTER_FAILURES`] times in a row (the `fallback` lambda of
    /// `updatePaintNode`), or when no GPU is attached.
    fn show_video(&mut self, ui: &mut egui::Ui, area: Rect, settings: &AppSettings) {
        let frame = self.video.lock().latest();
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.after_submit();
            gpu.apply_settings(settings);
        }

        let Some(frame) = frame else {
            return;
        };
        let identity = (Arc::as_ptr(&frame) as usize, frame.t_decoded);
        let is_new = self.last_frame != Some(identity);
        let now = ui.ctx().input(|i| i.time);

        if self.video_path() == VideoPath::Gpu {
            match self.show_video_gpu(ui, area, &frame, is_new) {
                GpuStep::Drawn(output) => {
                    self.last_frame = Some(identity);
                    self.last_output = Some(output);
                    self.report_output(output, now);
                    return;
                }
                GpuStep::Failed => return,
                GpuStep::FallBack => self.fall_back_to_cpu(),
            }
        }

        let Some(output) = self.cpu.prepare_frame(ui.ctx(), &frame, is_new) else {
            return;
        };
        self.last_frame = Some(identity);
        self.last_output = Some(output);
        self.cpu.draw(ui.painter(), area, output);
        self.report_output(output, now);
    }

    /// One frame through the GPU path: checks the callbacks' health, sizes the targets,
    /// queues the paint callback for a new frame and draws the output texture.
    fn show_video_gpu(
        &mut self,
        ui: &mut egui::Ui,
        area: Rect,
        frame: &Arc<Frame>,
        is_new: bool,
    ) -> GpuStep {
        let Some(gpu) = self.gpu.as_mut() else {
            return GpuStep::FallBack;
        };
        if gpu.health.should_fall_back() {
            return GpuStep::FallBack;
        }
        let Some(output) = gpu.prepare_frame(frame) else {
            return if gpu.health.record_failure() >= GPU_FALLBACK_AFTER_FAILURES {
                GpuStep::FallBack
            } else {
                GpuStep::Failed
            };
        };
        let video_rect = xcloud_gpu::letterbox(area, output.width, output.height);
        if is_new {
            ui.painter()
                .add(gpu.callback(video_rect, Arc::clone(frame)));
        }
        gpu.draw(ui.painter(), area, output);
        GpuStep::Drawn(output)
    }

    /// `fallback()` in `MetalVideoItem::updatePaintNode`: log once, clear the native path.
    fn fall_back_to_cpu(&mut self) {
        let failures = self.gpu.as_ref().map_or(0, GpuVideo::prepare_errors);
        warn!(
            target: LOG,
            "Native rendering unavailable; reverting to CPU image output ({failures} GPU failures)"
        );
        self.cpu_fallback = true;
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.release_texture();
        }
    }

    /// `controller->setOutput(height, scaled)`: size/upscale changes go out at once,
    /// timing-only changes every [`OUTPUT_REPORT_INTERVAL_SECS`].
    fn report_output(&mut self, output: VideoOutput, now: f64) {
        let due = match self.last_report {
            None => true,
            Some(last) => {
                last.height != output.height
                    || last.upscaled != output.upscaled
                    || last.upscaler_label != output.upscaler_label
                    || (now - last.at >= OUTPUT_REPORT_INTERVAL_SECS
                        && (last.gpu_ms != output.gpu_ms
                            || last.frame_age_ms != output.frame_age_ms
                            || last.present_ms != output.present_ms))
            }
        };
        if !due {
            return;
        }
        self.last_report = Some(OutputReport {
            height: output.height,
            upscaled: output.upscaled,
            upscaler_label: output.upscaler_label,
            gpu_ms: output.gpu_ms,
            frame_age_ms: output.frame_age_ms,
            present_ms: output.present_ms,
            at: now,
        });
        self.stream.send(StreamCommand::SetOutput {
            height: output.height,
            upscaled: output.upscaled,
            upscaler_label: output.upscaler_label,
            gpu_ms: output.gpu_ms,
            frame_age_ms: output.frame_age_ms,
            present_ms: output.present_ms,
        });
    }

    /// The launch screen: the game's hero art full-bleed, its name, a spinner with the
    /// status text, a four-step progress bar and any error.
    fn show_placeholder(
        &self,
        ui: &mut egui::Ui,
        area: Rect,
        s: &StreamSnapshot,
        artwork: &dyn ArtworkSource,
    ) {
        if !s.hero_image.is_empty() {
            let max_px =
                artwork_request_width(ArtworkKind::Hero, area.width(), ui.pixels_per_point());
            let state = artwork.request(
                std::slice::from_ref(&s.hero_image),
                ArtworkKind::Hero,
                max_px,
            );
            if let Some(texture) = state.texture() {
                let uv = aspect_fill_uv(texture.size_vec2(), area.size());
                ui.painter().image(
                    texture.id(),
                    area,
                    uv,
                    Color32::WHITE.gamma_multiply(HERO_OPACITY),
                );
            }
        }
        let bg = theme::BACKGROUND;
        ui.painter()
            .rect_filled(area, 0.0, Color32::from_black_alpha(95));
        widgets::vertical_gradient(
            ui,
            Rect::from_min_max(pos2(area.left(), area.center().y - 120.0), area.max),
            Color32::TRANSPARENT,
            bg.gamma_multiply(0.92),
        );
        widgets::radial_glow(
            ui,
            area.center(),
            vec2(area.width() * 0.5, area.height() * 0.45),
            Color32::BLACK,
            0.55,
        );

        let width = (area.width() - 80.0).clamp(0.0, 560.0);
        let cx = area.center().x;
        let title_font = theme::font_w(
            if area.width() > 900.0 { 40.0 } else { 30.0 },
            theme::Weight::Bold,
        );
        let title = (!s.title_name.is_empty()).then(|| {
            widgets::layout_font(ui, &s.title_name, title_font, theme::TEXT_PRIMARY, width, 2)
        });
        let status = ui.painter().layout_no_wrap(
            s.status_text.clone(),
            theme::font_w(16.0, theme::Weight::Medium),
            theme::TEXT_PRIMARY,
        );
        let error = (!s.error.is_empty()).then(|| {
            widgets::layout_font(
                ui,
                &s.error,
                theme::font(14.0),
                theme::TEXT_PRIMARY,
                width - 48.0,
                6,
            )
        });
        let title_h = title.as_ref().map_or(0.0, |g| g.size().y + 22.0);
        let error_h = error.as_ref().map_or(0.0, |g| g.size().y + 24.0 + 22.0);
        let total = title_h + 24.0 + 26.0 + 40.0 + error_h;
        let mut y = area.center().y - total / 2.0;

        if let Some(title) = title {
            let pos = pos2(cx - title.size().x / 2.0, y);
            let rect = Rect::from_min_size(pos, title.size());
            ui.painter().galley(pos, title, theme::TEXT_PRIMARY);
            label_rect(ui, rect, self.id.with("launch-title"), &s.title_name);
            y += rect.height() + 22.0;
        }

        // Spinner + status on one line.
        let spinner = 20.0;
        let line_w = spinner + 12.0 + status.size().x;
        let left = cx - line_w / 2.0;
        if s.error.is_empty() {
            egui::Spinner::new()
                .size(spinner)
                .color(theme::ACCENT)
                .paint_at(
                    ui,
                    Rect::from_min_size(pos2(left, y + 2.0), Vec2::splat(spinner)),
                );
        } else {
            icons::paint(
                ui.painter(),
                Rect::from_min_size(pos2(left, y + 2.0), Vec2::splat(spinner)),
                icons::Icon::Info,
                theme::WARNING,
            );
        }
        let status_rect = Rect::from_min_size(
            pos2(left + spinner + 12.0, y + 12.0 - status.size().y / 2.0),
            status.size(),
        );
        ui.painter()
            .galley(status_rect.min, status, theme::TEXT_PRIMARY);
        label_rect(ui, status_rect, self.id.with("status"), &s.status_text);
        y += 24.0 + 26.0;

        // Progress: four segments.
        let step = launch_step(s.lifecycle);
        let bar_w = 320.0_f32.min(width);
        let seg_gap = 6.0;
        let seg_w = (bar_w - seg_gap * 3.0) / 4.0;
        let t = ui.input(|i| i.time) as f32;
        let steps = if s.error.is_empty() { 4 } else { 0 };
        for i in 0..steps {
            let r = Rect::from_min_size(
                pos2(cx - bar_w / 2.0 + i as f32 * (seg_w + seg_gap), y),
                vec2(seg_w, 4.0),
            );
            ui.painter()
                .rect_filled(r, CornerRadius::same(2), Color32::from_white_alpha(36));
            if i < step {
                ui.painter()
                    .rect_filled(r, CornerRadius::same(2), theme::ACCENT);
            } else if i == step && s.error.is_empty() {
                let pulse = 0.35 + 0.65 * (0.5 + 0.5 * (t * 3.0).sin());
                ui.painter().rect_filled(
                    r,
                    CornerRadius::same(2),
                    theme::ACCENT.gamma_multiply(pulse),
                );
            }
        }
        if s.error.is_empty() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(33));
        }
        y += 40.0;

        if let Some(error) = error {
            y += 22.0;
            let box_rect = Rect::from_min_size(
                pos2(cx - width / 2.0, y),
                vec2(width, error.size().y + 24.0),
            );
            ui.painter().rect_filled(
                box_rect,
                CornerRadius::same(12),
                theme::DANGER.gamma_multiply(0.16),
            );
            let pos = pos2(box_rect.left() + 24.0, box_rect.top() + 12.0);
            let rect = Rect::from_min_size(pos, error.size());
            ui.painter().galley(pos, error, theme::TEXT_PRIMARY);
            label_rect(ui, rect, self.id.with("error"), &s.error);
        }
    }

    /// The two control rows (`z: 2`).
    fn show_controls_overlay(
        &mut self,
        ui: &mut egui::Ui,
        area: Rect,
        s: &StreamSnapshot,
        ctx: &egui::Context,
    ) {
        let connected = s.connected();
        if connected {
            widgets::vertical_gradient(
                ui,
                Rect::from_min_size(area.min, vec2(area.width(), 120.0)),
                Color32::from_black_alpha(150),
                Color32::TRANSPARENT,
            );
        }

        // Top-left: close.
        let close_rect = Rect::from_min_size(
            area.min + vec2(OVERLAY_MARGIN, OVERLAY_MARGIN),
            Vec2::splat(ICON_SIZE),
        );
        if icon_at(ui, close_rect, icons::Icon::Close, CLOSE_LABEL, false).clicked() {
            // onCloseRequested: stream.input.releaseAll(); Main.qml: stream.stopStreaming().
            self.pressed_keys.clear();
            self.stream.send(StreamCommand::ReleaseAll);
            self.stream.send(StreamCommand::StopStreaming);
        }

        // Top-right row, laid out from the right edge: [title] [hud] controller [fullscreen].
        let font = theme::font(theme::FONT_SMALL);
        let mut right = area.max.x - OVERLAY_MARGIN;
        let top = area.min.y + OVERLAY_MARGIN;
        let row_centre_y = top + ICON_SIZE / 2.0;

        if connected {
            let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
            let rect = Rect::from_min_size(pos2(right - ICON_SIZE, top), Vec2::splat(ICON_SIZE));
            let icon = if fullscreen {
                icons::Icon::ExitFullscreen
            } else {
                icons::Icon::Fullscreen
            };
            if icon_at(ui, rect, icon, FULLSCREEN_LABEL, false).clicked() {
                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(!fullscreen));
            }
            right -= ICON_SIZE + OVERLAY_SPACING;
        }

        let (controller_text, controller_color) = if s.controller_connected {
            (s.controller_name.as_str(), theme::ACCENT)
        } else {
            (KEYBOARD_HINT, theme::TEXT_SECONDARY)
        };
        let galley = ui.painter().layout_no_wrap(
            controller_text.to_string(),
            font.clone(),
            controller_color,
        );
        let chip_w = galley.size().x + 18.0 + 22.0 + 18.0;
        let chip = Rect::from_min_size(pos2(right - chip_w, top), vec2(chip_w, ICON_SIZE));
        ui.painter().rect_filled(
            chip,
            CornerRadius::same((ICON_SIZE / 2.0) as u8),
            theme::SURFACE_GLASS,
        );
        icons::paint(
            ui.painter(),
            icons::centered(pos2(chip.left() + 18.0 + 10.0, row_centre_y), 20.0),
            if s.controller_connected {
                icons::Icon::Gamepad
            } else {
                icons::Icon::Info
            },
            controller_color,
        );
        let text_rect = Rect::from_min_size(
            pos2(
                chip.left() + 18.0 + 22.0 + 8.0,
                row_centre_y - galley.size().y / 2.0,
            ),
            galley.size(),
        );
        ui.painter().galley(text_rect.min, galley, controller_color);
        label_rect(ui, text_rect, self.id.with("controller"), controller_text);
        right -= chip_w + OVERLAY_SPACING;

        if connected {
            let rect = Rect::from_min_size(pos2(right - ICON_SIZE, top), Vec2::splat(ICON_SIZE));
            if icon_at(ui, rect, icons::Icon::Gauge, HUD_LABEL, s.hud_enabled).clicked() {
                self.stream
                    .send(StreamCommand::SetHudEnabled(!s.hud_enabled));
            }
            right -= ICON_SIZE + OVERLAY_SPACING;

            if !s.title_name.is_empty() {
                let galley = ui.painter().layout_no_wrap(
                    s.title_name.clone(),
                    theme::font_w(15.0, theme::Weight::Semibold),
                    theme::TEXT_PRIMARY,
                );
                let rect = Rect::from_min_size(
                    pos2(
                        right - galley.size().x,
                        row_centre_y - galley.size().y / 2.0,
                    ),
                    galley.size(),
                );
                ui.painter().galley(rect.min, galley, theme::TEXT_PRIMARY);
                label_rect(ui, rect, self.id.with("title"), &s.title_name);
            }
        }
    }

    /// The "Click to enable controls" pill (`z: 3`).
    fn show_focus_hint(&self, ui: &mut egui::Ui, area: Rect, ctx: &egui::Context) {
        let galley = ui.painter().layout_no_wrap(
            FOCUS_HINT_TEXT.to_string(),
            theme::font(theme::FONT_SMALL),
            theme::TEXT_PRIMARY,
        );
        let size = vec2(galley.size().x + HINT_PADDING, HINT_HEIGHT);
        let rect = Rect::from_min_size(
            pos2(area.center().x - size.x / 2.0, area.min.y + HINT_TOP),
            size,
        );
        ui.painter().rect_filled(
            rect,
            CornerRadius::same((HINT_HEIGHT / 2.0) as u8),
            Color32::from_black_alpha(200),
        );
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same((HINT_HEIGHT / 2.0) as u8),
            egui::Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.5)),
            egui::StrokeKind::Inside,
        );
        ui.painter().galley(
            rect.center() - galley.size() / 2.0,
            galley,
            theme::TEXT_PRIMARY,
        );
        let response = ui.interact(rect, self.id.with("focus-hint"), Sense::click());
        response.widget_info(|| WidgetInfo::labeled(Role::Button, true, FOCUS_HINT_TEXT));
        if response.clicked() {
            self.request_activate(ctx);
        }
    }
}

/// A glass round icon button placed at an absolute `rect`; `active` highlights it.
fn icon_at(
    ui: &mut egui::Ui,
    rect: Rect,
    icon: icons::Icon,
    label: &str,
    active: bool,
) -> Response {
    kit::place(ui, rect, |ui| {
        kit::icon_button_toned(ui, icon, ICON_SIZE, label, kit::Tone::Glass, active)
    })
}

/// Which of the four launch steps the lifecycle is in.
pub fn launch_step(state: xcloud_proto::streaming_types::StreamLifecycleState) -> usize {
    use xcloud_proto::streaming_types::StreamLifecycleState;
    match state {
        StreamLifecycleState::Idle | StreamLifecycleState::StartingSession => 0,
        StreamLifecycleState::Provisioning | StreamLifecycleState::WaitingForResources => 1,
        StreamLifecycleState::ReadyToConnect | StreamLifecycleState::ConnectingWebRTC => 2,
        _ => 3,
    }
}

/// Registers an already painted text as a label for accessibility (and egui_kittest).
fn label_rect(ui: &mut egui::Ui, rect: Rect, id: Id, text: &str) {
    let text = text.to_string();
    ui.interact(rect, id, Sense::hover())
        .widget_info(|| WidgetInfo::labeled(Role::Label, true, text.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Handle;
    use crate::ui::widgets::ArtworkState;
    use crate::ui::widgets::NoArtwork;
    use crossbeam_channel::Receiver;
    use egui::ViewportId;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use xcloud_media::{FrameMailbox, MailboxWriter, NativeBuffer, YCbCrMatrix};
    use xcloud_proto::streaming_types::StreamLifecycleState;

    struct Fixture {
        view: StreamView,
        snapshot: Arc<RwLock<StreamSnapshot>>,
        rx: Receiver<StreamCommand>,
        writer: MailboxWriter,
    }

    fn fixture(initial: StreamSnapshot) -> Fixture {
        let (handle, snapshot, rx) = Handle::new(initial);
        let (writer, reader) = FrameMailbox::open();
        let video: VideoSource = Arc::new(parking_lot::Mutex::new(reader));
        let settings = Arc::new(RwLock::new(AppSettings::default()));
        Fixture {
            view: StreamView::new(handle, video, settings),
            snapshot,
            rx,
            writer,
        }
    }

    /// A `w`×`h` NV12 frame in CPU memory with constant luma and chroma.
    fn cpu_frame(w: u32, h: u32, luma: u8, cb: u8, cr: u8, color: ColorInfo) -> Frame {
        let uv: Vec<u8> = (0..(w.div_ceil(2) * h.div_ceil(2)))
            .flat_map(|_| [cb, cr])
            .collect();
        let now = Instant::now();
        Frame {
            buf: NativeBuffer::Nv12Cpu {
                y: vec![luma; (w * h) as usize],
                uv,
                y_stride: w as usize,
                uv_stride: (w.div_ceil(2) * 2) as usize,
            },
            width: w,
            height: h,
            color,
            t_received: now,
            t_submitted: now,
            t_decoded: now,
        }
    }

    /// An [`ArtworkSource`] that records what it was asked for.
    #[derive(Default)]
    struct RecordingArtwork {
        requests: parking_lot::Mutex<Vec<(ArtworkKind, u32)>>,
    }

    impl ArtworkSource for RecordingArtwork {
        fn request(&self, _urls: &[String], kind: ArtworkKind, max_px: u32) -> ArtworkState {
            self.requests.lock().push((kind, max_px));
            ArtworkState::Loading
        }
    }

    fn set_output_heights(commands: &[StreamCommand]) -> Vec<(u32, bool)> {
        commands
            .iter()
            .filter_map(|c| match c {
                StreamCommand::SetOutput {
                    height, upscaled, ..
                } => Some((*height, *upscaled)),
                _ => None,
            })
            .collect()
    }

    fn connected() -> StreamSnapshot {
        StreamSnapshot {
            lifecycle: StreamLifecycleState::Connected,
            title_name: "Halo Infinite".into(),
            ..StreamSnapshot::default()
        }
    }

    /// `RoundIcon` glyphs come from the system fallback fonts at runtime; the test fonts
    /// do not carry them, so missing glyphs are allowed here. Steps advance egui time by
    /// one 60 Hz frame (kittest's default of 0.25 s would run into the 3 s controls
    /// auto-hide within a few clicks).
    fn builder() -> egui_kittest::HarnessBuilder<()> {
        Harness::builder()
            .with_size(vec2(1280.0, 720.0))
            .with_step_dt(1.0 / 60.0)
            .with_context_setup(crate::theme::apply)
            // Hover fades on the glass buttons settle over a few frames.
            .with_max_steps(16)
            .allow_missing_glyphs()
    }

    fn harness(view: &mut StreamView) -> Harness<'_> {
        builder().build_ui(move |ui| view.show(ui, &NoArtwork))
    }

    fn drain(rx: &Receiver<StreamCommand>) -> Vec<StreamCommand> {
        rx.try_iter().collect()
    }

    fn set_window_focus(h: &mut Harness<'_>, focused: bool) {
        if let Some(info) = h.input_mut().viewports.get_mut(&ViewportId::ROOT) {
            info.focused = Some(focused);
        }
    }

    #[test]
    fn first_frame_reports_focus() {
        let Fixture { mut view, rx, .. } = fixture(connected());
        let mut h = harness(&mut view);
        h.run();
        assert_eq!(drain(&rx), [StreamCommand::SetInputFocused(true)]);
        drop(h);
        assert!(view.input_focused());
    }

    #[test]
    fn key_press_sends_set_key_and_release() {
        let Fixture { mut view, rx, .. } = fixture(connected());
        let mut h = harness(&mut view);
        h.run();
        drain(&rx);

        h.key_down(Key::Space);
        h.step();
        assert_eq!(
            drain(&rx),
            [StreamCommand::SetKey {
                name: "a".into(),
                pressed: true
            }]
        );
        h.key_up(Key::Space);
        h.step();
        assert_eq!(
            drain(&rx),
            [StreamCommand::SetKey {
                name: "a".into(),
                pressed: false
            }]
        );

        // The rest of the README table, one press each.
        for (key, name) in [
            (Key::Enter, "a"),
            (Key::Escape, "b"),
            (Key::Backspace, "b"),
            (Key::Q, "x"),
            (Key::E, "y"),
            (Key::ArrowLeft, "dpad-left"),
            (Key::ArrowRight, "dpad-right"),
            (Key::ArrowUp, "dpad-up"),
            (Key::ArrowDown, "dpad-down"),
            (Key::Z, "left-shoulder"),
            (Key::C, "right-shoulder"),
            (Key::ShiftLeft, "left-thumb"),
            (Key::ControlRight, "right-thumb"),
            (Key::Tab, "menu"),
            (Key::M, "view"),
        ] {
            h.key_press(key);
            h.step();
            assert_eq!(
                drain(&rx),
                [
                    StreamCommand::SetKey {
                        name: name.into(),
                        pressed: true
                    },
                    StreamCommand::SetKey {
                        name: name.into(),
                        pressed: false
                    }
                ],
                "{key:?}"
            );
        }
    }

    #[test]
    fn modifier_keys_arrive_as_physical_keys() {
        let Fixture { mut view, rx, .. } = fixture(connected());
        let mut h = harness(&mut view);
        h.run();
        drain(&rx);
        // egui-winit has no logical key for Shift; the physical key carries it.
        h.event(egui::Event::Key {
            key: Key::F35,
            physical_key: Some(Key::ShiftLeft),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::SHIFT,
        });
        h.step();
        assert_eq!(
            drain(&rx),
            [StreamCommand::SetKey {
                name: "left-thumb".into(),
                pressed: true
            }]
        );
    }

    #[test]
    fn r_and_f_drive_the_triggers() {
        let Fixture { mut view, rx, .. } = fixture(connected());
        let mut h = harness(&mut view);
        h.run();
        drain(&rx);
        h.key_down(Key::R);
        h.step();
        assert_eq!(
            drain(&rx),
            [StreamCommand::SetTrigger {
                side: TriggerSide::Left,
                value: 1.0
            }]
        );
        h.key_up(Key::R);
        h.key_down(Key::F);
        h.step();
        assert_eq!(
            drain(&rx),
            [
                StreamCommand::SetTrigger {
                    side: TriggerSide::Left,
                    value: 0.0
                },
                StreamCommand::SetTrigger {
                    side: TriggerSide::Right,
                    value: 1.0
                }
            ]
        );
    }

    #[test]
    fn wasd_sums_into_the_left_stick() {
        let Fixture { mut view, rx, .. } = fixture(connected());
        let mut h = harness(&mut view);
        h.run();
        drain(&rx);

        h.key_down(Key::W);
        h.step();
        assert_eq!(drain(&rx), [StreamCommand::SetLeftStick { x: 0.0, y: 1.0 }]);
        h.key_down(Key::D);
        h.step();
        assert_eq!(drain(&rx), [StreamCommand::SetLeftStick { x: 1.0, y: 1.0 }]);
        h.key_down(Key::S);
        h.step();
        assert_eq!(drain(&rx), [StreamCommand::SetLeftStick { x: 1.0, y: 0.0 }]);
        h.key_up(Key::W);
        h.key_up(Key::D);
        h.step();
        assert_eq!(
            drain(&rx),
            [
                StreamCommand::SetLeftStick { x: 1.0, y: -1.0 },
                StreamCommand::SetLeftStick { x: 0.0, y: -1.0 }
            ]
        );
        h.key_up(Key::S);
        h.step();
        assert_eq!(drain(&rx), [StreamCommand::SetLeftStick { x: 0.0, y: 0.0 }]);
    }

    #[test]
    fn key_repeats_and_unmapped_keys_do_nothing() {
        let Fixture { mut view, rx, .. } = fixture(connected());
        let mut h = harness(&mut view);
        h.run();
        drain(&rx);
        // egui marks a second press without a release as a repeat.
        h.key_down(Key::Space);
        h.key_down(Key::Space);
        h.key_down(Key::Space);
        h.key_press(Key::P);
        h.key_press(Key::F5);
        h.step();
        assert_eq!(
            drain(&rx),
            [StreamCommand::SetKey {
                name: "a".into(),
                pressed: true
            }],
            "one press, no repeats, nothing for unmapped keys"
        );
    }

    #[test]
    fn controller_keys_are_consumed_before_egui_sees_them() {
        let Fixture { mut view, rx, .. } = fixture(connected());
        let seen = Arc::new(parking_lot::Mutex::new(Vec::<Key>::new()));
        let seen_in_ui = Arc::clone(&seen);
        let mut h = builder().build_ui(move |ui| {
            view.show(ui, &NoArtwork);
            // A widget drawn after the view: it must not see the controller keys.
            ui.input(|i| {
                for event in &i.events {
                    if let egui::Event::Key {
                        key, pressed: true, ..
                    } = event
                    {
                        seen_in_ui.lock().push(*key);
                    }
                }
            });
        });
        h.run();
        drain(&rx);
        h.key_press(Key::Tab);
        h.key_press(Key::Space);
        h.key_press(Key::P);
        h.step();
        assert_eq!(*seen.lock(), [Key::P], "only the unmapped key reaches egui");
        assert_eq!(
            drain(&rx),
            [
                StreamCommand::SetKey {
                    name: "menu".into(),
                    pressed: true
                },
                StreamCommand::SetKey {
                    name: "menu".into(),
                    pressed: false
                },
                StreamCommand::SetKey {
                    name: "a".into(),
                    pressed: true
                },
                StreamCommand::SetKey {
                    name: "a".into(),
                    pressed: false
                },
            ]
        );
    }

    #[test]
    fn close_click_releases_inputs_and_stops_streaming() {
        let Fixture { mut view, rx, .. } = fixture(connected());
        let mut h = harness(&mut view);
        h.run();
        drain(&rx);
        h.get_by_label(CLOSE_LABEL).click();
        h.run();
        assert_eq!(
            drain(&rx),
            [StreamCommand::ReleaseAll, StreamCommand::StopStreaming]
        );
    }

    #[test]
    fn hud_toggle_sends_set_hud_enabled() {
        let Fixture {
            mut view,
            rx,
            snapshot,
            ..
        } = fixture(connected());
        let mut h = harness(&mut view);
        h.run();
        drain(&rx);
        h.get_by_label(HUD_LABEL).click();
        h.run();
        assert_eq!(drain(&rx), [StreamCommand::SetHudEnabled(true)]);

        snapshot.write().hud_enabled = true;
        h.run_steps(20); // a third of a second later: not a double-click
        h.get_by_label(HUD_LABEL).click();
        h.run();
        assert_eq!(drain(&rx), [StreamCommand::SetHudEnabled(false)]);
    }

    #[test]
    fn overlay_shows_title_and_keyboard_hint_or_controller_name() {
        let Fixture {
            mut view, snapshot, ..
        } = fixture(connected());
        let mut h = harness(&mut view);
        h.run();
        h.get_by_label("Halo Infinite");
        h.get_by_label(KEYBOARD_HINT);
        h.get_by_label(FULLSCREEN_LABEL);
        {
            let mut s = snapshot.write();
            s.controller_connected = true;
            s.controller_name = "Xbox Wireless Controller".into();
        }
        h.run();
        h.get_by_label("Xbox Wireless Controller");
        assert!(h.query_by_label(KEYBOARD_HINT).is_none());
    }

    #[test]
    fn hud_pill_appears_when_enabled_and_connected() {
        let mut s = connected();
        s.hud_enabled = true;
        s.stats.frames_per_second = 60.0;
        s.stats.negotiated_height = 1080;
        let Fixture { mut view, .. } = fixture(s);
        let mut h = harness(&mut view);
        h.run();
        h.get_by_label("1080p  60 fps");
    }

    #[test]
    fn placeholder_shows_status_and_error_until_connected() {
        let initial = StreamSnapshot {
            lifecycle: StreamLifecycleState::Provisioning,
            status_text: "Provisioning…".into(),
            error: "Region unavailable".into(),
            ..StreamSnapshot::default()
        };
        let Fixture {
            mut view, snapshot, ..
        } = fixture(initial);
        let mut h = harness(&mut view);
        // The busy indicator repaints every frame, so step instead of `run`.
        h.run_steps(2);
        h.get_by_label("Provisioning…");
        h.get_by_label("Region unavailable");
        h.get_by_label(CLOSE_LABEL);
        // HUD / fullscreen / title only once connected.
        assert!(h.query_by_label(HUD_LABEL).is_none());
        assert!(h.query_by_label(FULLSCREEN_LABEL).is_none());

        *snapshot.write() = connected();
        h.run();
        assert!(h.query_by_label("Provisioning…").is_none());
        h.get_by_label(HUD_LABEL);
    }

    #[test]
    fn controls_hide_after_three_seconds_and_a_click_brings_them_back() {
        let Fixture { mut view, rx, .. } = fixture(connected());
        {
            let mut h = builder()
                .with_step_dt(0.5)
                .build_ui(|ui| view.show(ui, &NoArtwork));
            h.run();
            drain(&rx);
            h.get_by_label(CLOSE_LABEL);
            h.run_steps(7); // 3.5 s of egui time
            assert!(
                h.query_by_label(CLOSE_LABEL).is_none(),
                "controls auto-hide"
            );
            // Clicking the surface toggles them back on.
            h.get_by_label(SURFACE_LABEL).click();
            h.run();
            h.get_by_label(CLOSE_LABEL);
            assert!(drain(&rx).is_empty(), "a surface click sends nothing");
        }
        assert!(view.controls_visible());
    }

    #[test]
    fn controls_stay_while_not_connected() {
        let Fixture { mut view, .. } = fixture(StreamSnapshot {
            lifecycle: StreamLifecycleState::ConnectingWebRTC,
            status_text: "Connecting".into(),
            ..StreamSnapshot::default()
        });
        let mut h = builder()
            .with_step_dt(0.5)
            .build_ui(|ui| view.show(ui, &NoArtwork));
        h.run_steps(10);
        h.get_by_label(CLOSE_LABEL);
    }

    #[test]
    fn focus_loss_releases_everything_and_shows_the_hint() {
        let Fixture { mut view, rx, .. } = fixture(connected());
        let mut h = harness(&mut view);
        h.run();
        drain(&rx);
        h.key_down(Key::W);
        h.step();
        drain(&rx);

        set_window_focus(&mut h, false);
        h.event(egui::Event::WindowFocused(false));
        h.run();
        assert_eq!(
            drain(&rx),
            [
                StreamCommand::SetInputFocused(false),
                StreamCommand::ReleaseAll
            ]
        );
        h.get_by_label(FOCUS_HINT_TEXT);

        // Keys are ignored while unfocused.
        h.key_press(Key::Space);
        h.step();
        assert!(drain(&rx).is_empty());

        set_window_focus(&mut h, true);
        h.event(egui::Event::WindowFocused(true));
        h.run();
        assert_eq!(drain(&rx), [StreamCommand::SetInputFocused(true)]);
        assert!(h.query_by_label(FOCUS_HINT_TEXT).is_none());
        // The held key was forgotten: W again starts from zero.
        h.key_down(Key::W);
        h.step();
        assert_eq!(drain(&rx), [StreamCommand::SetLeftStick { x: 0.0, y: 1.0 }]);
    }

    #[test]
    fn reset_starts_over_like_a_fresh_view() {
        let Fixture { mut view, rx, .. } = fixture(connected());
        {
            let mut h = harness(&mut view);
            h.run();
            h.key_down(Key::W);
            h.step();
            h.get_by_label(SURFACE_LABEL).click();
            h.run();
        }
        drain(&rx);
        assert!(!view.controls_visible());
        view.reset();
        assert!(view.controls_visible());
        assert!(!view.input_focused());
        let mut h = harness(&mut view);
        h.run();
        assert_eq!(drain(&rx), [StreamCommand::SetInputFocused(true)]);
    }

    #[test]
    fn qml_key_names_match_the_streamview_table() {
        assert_eq!(qml_key_name(Key::Space), Some("Space"));
        assert_eq!(qml_key_name(Key::Enter), Some("Return"));
        assert_eq!(qml_key_name(Key::ShiftRight), Some("Shift"));
        assert_eq!(qml_key_name(Key::ControlLeft), Some("Control"));
        assert_eq!(qml_key_name(Key::ArrowUp), Some("Up"));
        assert_eq!(qml_key_name(Key::F1), None);
        assert_eq!(
            qml_key_name(Key::I),
            None,
            "Ctrl/Cmd+Shift+I stays with the app"
        );
        for key in [
            Key::Space,
            Key::Enter,
            Key::Escape,
            Key::Backspace,
            Key::Q,
            Key::E,
            Key::ArrowLeft,
            Key::ArrowRight,
            Key::ArrowUp,
            Key::ArrowDown,
            Key::Z,
            Key::C,
            Key::ShiftLeft,
            Key::ControlLeft,
            Key::Tab,
            Key::M,
            Key::R,
            Key::F,
            Key::W,
            Key::A,
            Key::S,
            Key::D,
        ] {
            let name = qml_key_name(key).unwrap_or_else(|| panic!("{key:?} unmapped"));
            assert!(key_action(name).is_some(), "{key:?} → {name} has no action");
        }
    }

    #[test]
    fn ctrl_or_cmd_shift_i_toggles_the_hud() {
        let Fixture {
            mut view,
            rx,
            snapshot,
            ..
        } = fixture(connected());
        let mut h = harness(&mut view);
        h.run();
        drain(&rx);

        // ⌘⇧I on macOS / Ctrl+Shift+I elsewhere (`Ctrl+Shift+I` in Qt's spelling).
        h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::I);
        h.step();
        assert_eq!(drain(&rx), [StreamCommand::SetHudEnabled(true)]);

        // The physical control key (`Meta+Shift+I` in Qt's macOS spelling).
        snapshot.write().hud_enabled = true;
        h.key_press_modifiers(Modifiers::CTRL | Modifiers::SHIFT, Key::I);
        h.step();
        assert_eq!(drain(&rx), [StreamCommand::SetHudEnabled(false)]);

        // Without the modifiers `I` is nothing (not a controller key either).
        h.key_press(Key::I);
        h.key_press_modifiers(Modifiers::SHIFT, Key::I);
        h.step();
        assert!(drain(&rx).is_empty());
    }

    #[test]
    fn hud_shortcut_works_before_the_stream_connected() {
        let Fixture { mut view, rx, .. } = fixture(StreamSnapshot {
            lifecycle: StreamLifecycleState::ConnectingWebRTC,
            ..StreamSnapshot::default()
        });
        let mut h = harness(&mut view);
        h.step();
        drain(&rx);
        h.key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::I);
        h.step();
        assert_eq!(drain(&rx), [StreamCommand::SetHudEnabled(true)]);
    }

    #[test]
    fn hero_artwork_is_requested_at_device_pixels() {
        // 1280 pt wide: sourceSize.width = min(800, ceil(1280 / 64) * 64) = 800 logical
        // pixels, scaled by the display like Qt does: 1600 on a 2x display.
        for (ppp, expected) in [(2.0, 1600), (1.0, 800), (1.5, 1200)] {
            let Fixture { mut view, .. } = fixture(StreamSnapshot {
                lifecycle: StreamLifecycleState::Provisioning,
                hero_image: "https://example.test/hero.jpg".into(),
                ..StreamSnapshot::default()
            });
            let artwork = RecordingArtwork::default();
            let mut h = builder()
                .with_pixels_per_point(ppp)
                .build_ui(|ui| view.show(ui, &artwork));
            h.step();
            drop(h);
            let requests = artwork.requests.lock();
            assert!(!requests.is_empty(), "hero requested at {ppp}x");
            for (kind, max_px) in requests.iter() {
                assert_eq!(*kind, ArtworkKind::Hero);
                assert_eq!(*max_px, expected, "at {ppp}x");
            }
        }
    }

    #[test]
    fn nv12_to_rgba_matches_the_shader_arithmetic() {
        let limited = ColorInfo::default();
        let full = ColorInfo {
            matrix: YCbCrMatrix::Bt709,
            full_range: true,
        };
        let convert = |luma: u8, cb: u8, cr: u8, color: &ColorInfo| {
            let f = cpu_frame(2, 2, luma, cb, cr, *color);
            let mut out = Vec::new();
            convert_frame(&f, &mut out).expect("convert");
            assert_eq!(out.len(), 4);
            assert!(out.iter().all(|p| *p == out[0]), "uniform frame");
            out[0]
        };
        // The `convert_video` kernel in f64: what the GPU path produces (±1 for rounding).
        let reference = |luma: u8, cb: u8, cr: u8, color: &ColorInfo| {
            let c = color.shader_coefficients().map(f64::from);
            let l = (f64::from(luma) / 255.0 - c[0]) * c[1];
            let cb = (f64::from(cb) / 255.0 - 0.5) * c[2];
            let cr = (f64::from(cr) / 255.0 - 0.5) * c[2];
            let q = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as i32;
            [
                q(l + c[4] * cr),
                q(l + c[5] * cb + c[6] * cr),
                q(l + c[7] * cb),
            ]
        };
        let check = |luma: u8, cb: u8, cr: u8, color: &ColorInfo| {
            let got = convert(luma, cb, cr, color);
            let want = reference(luma, cb, cr, color);
            for (g, w) in [got.r(), got.g(), got.b()].into_iter().zip(want) {
                assert!(
                    (i32::from(g) - w).abs() <= 1,
                    "Y={luma} Cb={cb} Cr={cr} {color:?}: got {got:?}, want {want:?}"
                );
            }
            got
        };
        // Limited range: 16 → black, 235 → white, 128 → (128 - 16) / 219 ≈ 0.511. The
        // 8-bit "neutral" chroma 128 is 0.502, not 0.5, so greys carry a 1/255 tint like
        // on the GPU.
        let black = check(16, 128, 128, &limited);
        assert!(
            black.r() <= 1 && black.g() <= 1 && black.b() <= 1,
            "{black:?}"
        );
        assert_eq!(
            check(235, 128, 128, &limited),
            Color32::from_rgb(255, 255, 255)
        );
        let grey = check(128, 128, 128, &limited);
        assert!((i32::from(grey.g()) - 130).abs() <= 1, "{grey:?}");
        // Full range: the identity on luma.
        let grey = check(128, 128, 128, &full);
        assert!((i32::from(grey.g()) - 128).abs() <= 1, "{grey:?}");
        assert_eq!(
            check(255, 128, 128, &full),
            Color32::from_rgb(255, 255, 255)
        );
        // BT.709 full range, Cr at maximum: R saturates, G = 0.502 - 0.468 * 0.498, B ≈ Y.
        let red = check(128, 128, 255, &full);
        assert_eq!(red.r(), 255);
        assert!((i32::from(red.g()) - 69).abs() <= 1, "{red:?}");
        // Cb at maximum: B saturates.
        let blue = check(128, 255, 128, &full);
        assert_eq!(blue.b(), 255);
        assert!(blue.r() < blue.b());
        // BT.601 differs from BT.709 in the chroma weights.
        let bt601 = ColorInfo {
            matrix: YCbCrMatrix::Bt601,
            full_range: false,
        };
        assert_ne!(check(128, 64, 192, &bt601), check(128, 64, 192, &limited));
    }

    #[test]
    fn nv12_to_rgba_rejects_short_planes_and_empty_frames() {
        let color = ColorInfo::default();
        let mut out = Vec::new();
        let planes = |y: &'static [u8], uv: &'static [u8], y_stride, uv_stride| Nv12Planes {
            y,
            uv,
            y_stride,
            uv_stride,
        };
        let err = nv12_to_rgba(planes(&[0; 8], &[128; 4], 4, 4), 4, 2, &color, &mut out);
        assert!(err.is_ok(), "{err:?}");
        assert_eq!(out.len(), 8);
        // Odd width: the chroma row needs ceil(w / 2) * 2 bytes.
        assert!(nv12_to_rgba(planes(&[0; 6], &[128; 4], 3, 4), 3, 2, &color, &mut out).is_ok());
        assert!(matches!(
            nv12_to_rgba(planes(&[0; 7], &[128; 4], 4, 4), 4, 2, &color, &mut out),
            Err(CpuVideoError::ShortPlane { plane: "Y", .. })
        ));
        assert!(matches!(
            nv12_to_rgba(planes(&[0; 8], &[128; 3], 4, 4), 4, 2, &color, &mut out),
            Err(CpuVideoError::ShortPlane { plane: "CbCr", .. })
        ));
        assert!(matches!(
            nv12_to_rgba(planes(&[0; 8], &[128; 4], 2, 4), 4, 2, &color, &mut out),
            Err(CpuVideoError::ShortPlane { plane: "Y", .. })
        ));
        assert!(matches!(
            nv12_to_rgba(planes(&[], &[], 0, 0), 0, 2, &color, &mut out),
            Err(CpuVideoError::EmptyFrame { .. })
        ));
    }

    #[test]
    fn cpu_path_draws_frames_without_a_gpu() {
        let Fixture {
            mut view,
            rx,
            mut writer,
            ..
        } = fixture(connected());
        assert_eq!(view.video_path(), VideoPath::Cpu);
        assert!(!view.cpu_fallback(), "no GPU to fall back from");
        writer.submit(cpu_frame(64, 36, 128, 128, 128, ColorInfo::default()));
        {
            let mut h = harness(&mut view);
            h.run();
        }
        assert_eq!(view.output_size(), Some((64, 36)));
        assert_eq!(set_output_heights(&drain(&rx)), [(36, false)]);

        // A new, differently sized frame re-uploads; the same frame does not report again.
        writer.submit(cpu_frame(32, 18, 128, 128, 128, ColorInfo::default()));
        {
            let mut h = harness(&mut view);
            h.run();
            h.run();
        }
        assert_eq!(view.output_size(), Some((32, 18)));
        assert_eq!(set_output_heights(&drain(&rx)), [(18, false)]);

        view.reset();
        assert_eq!(view.output_size(), None);
        assert!(view.cpu.texture.is_none(), "reset releases the CPU texture");
    }

    #[test]
    fn gpu_health_falls_back_after_consecutive_failures() {
        let health = GpuHealth::default();
        assert!(!health.should_fall_back());
        for _ in 0..GPU_FALLBACK_AFTER_FAILURES - 1 {
            health.record_failure();
        }
        assert!(!health.should_fall_back());
        health.record_success();
        assert_eq!(health.consecutive_failures(), 0);
        assert_eq!(health.errors(), GPU_FALLBACK_AFTER_FAILURES - 1);
        for _ in 0..GPU_FALLBACK_AFTER_FAILURES {
            health.record_failure();
        }
        assert!(health.should_fall_back());
        assert_eq!(health.errors(), 2 * GPU_FALLBACK_AFTER_FAILURES - 1);
        health.reset();
        assert!(!health.should_fall_back());
        assert_eq!(health.errors(), 0);
    }

    // ---- GPU tests: skipped when no adapter is available (CI without a GPU) ----------

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        use std::task::{Context, Poll, Waker};
        let mut cx = Context::from_waker(Waker::noop());
        let mut fut = std::pin::pin!(fut);
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::yield_now();
        }
    }

    /// An eframe-like render state on a headless device, or `None` without a GPU.
    fn render_state() -> Option<egui_wgpu::RenderState> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter =
            block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
        let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("xcloud-app stream_view test"),
            ..Default::default()
        }))
        .ok()?;
        let format = wgpu::TextureFormat::Bgra8Unorm;
        let renderer =
            egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::default());
        Some(egui_wgpu::RenderState {
            adapter,
            available_adapters: Vec::new(),
            instance,
            device,
            queue,
            target_format: format,
            renderer: Arc::new(egui::mutex::RwLock::new(renderer)),
        })
    }

    #[test]
    fn toggling_the_upscaler_retires_the_renderer_instead_of_draining_the_gpu() {
        let Some(rs) = render_state() else {
            eprintln!("no wgpu adapter; skipping");
            return;
        };
        let mut settings = AppSettings::default();
        let mut gpu = GpuVideo::new(&rs, &settings);
        assert!(gpu.upscale_enabled());
        assert_eq!(gpu.retired_renderers(), 0);
        {
            let _renderer = rs.renderer.write();
            gpu.apply_settings(&settings);
        }
        settings.sharpness = 0.8;
        gpu.apply_settings(&settings);
        assert_eq!(
            rs.renderer
                .read()
                .callback_resources
                .get::<VideoRenderer>()
                .unwrap()
                .sharpness(),
            0.8
        );

        // Encode one frame the way egui's callback does, submit it, and tell the renderer.
        let frame = cpu_frame(64, 32, 128, 128, 128, ColorInfo::default());
        let output = gpu.prepare_frame(&frame).expect("targets");
        assert!(output.height >= 32, "{output:?}");
        let mut encoder = rs
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut egui_renderer = rs.renderer.write();
            let vr = egui_renderer
                .callback_resources
                .get_mut::<VideoRenderer>()
                .expect("renderer installed");
            vr.prepare(&mut encoder, &frame).expect("prepare");
        }
        rs.queue.submit([encoder.finish()]);
        gpu.after_submit();

        // Flip the upscaler: the old renderer (with its in-flight frame) is parked, not
        // waited for, and the new one is live at once.
        settings.upscale_enabled = false;
        gpu.apply_settings(&settings);
        assert!(!gpu.upscale_enabled());
        assert!(gpu.retired_renderers() <= 1);
        {
            let egui_renderer = rs.renderer.read();
            let vr = egui_renderer
                .callback_resources
                .get::<VideoRenderer>()
                .expect("new renderer installed");
            assert!(!vr.policy().enabled);
            assert_eq!(vr.frames_prepared(), 0, "a fresh renderer");
        }
        // Flipping back without new GPU work retires nothing (no frames in flight).
        settings.upscale_enabled = true;
        gpu.apply_settings(&settings);
        assert!(gpu.upscale_enabled());
        assert!(gpu.retired_renderers() <= 1);

        // Once the GPU is done the parked renderer goes away on the next frame.
        let _ = rs.device.poll(wgpu::PollType::wait_indefinitely());
        gpu.after_submit();
        assert_eq!(gpu.retired_renderers(), 0);

        // Output target changes also update the live GPU policy.
        for (height, width) in [(1440, 2560), (2160, 3840), (1080, 1920)] {
            settings.upscale_height = height;
            gpu.apply_settings(&settings);
            let renderer = rs.renderer.read();
            let policy = renderer
                .callback_resources
                .get::<VideoRenderer>()
                .unwrap()
                .policy();
            assert_eq!(policy.output_size(1280, 720), ((width, height), true));
            assert_eq!(policy.output_size(3840, 2160), ((3840, 2160), false));
            assert_eq!(policy.output_size(640, 480).0, (height * 4 / 3, height));
        }

        // The upscaler kind switches in place (no renderer rebuild). MetalFX falls back to
        // FSR when it is not compiled in / unsupported, and the HUD label says which ran.
        // `OPENXCLOUD_UPSCALER` overrides the setting, so only check without it.
        if std::env::var_os(xcloud_gpu::UPSCALER_ENV).is_none() {
            settings.upscaler = "metalfx".into();
            gpu.apply_settings(&settings);
            assert_eq!(gpu.upscaler(), Upscaler::MetalFx);
            assert_eq!(gpu.retired_renderers(), 0, "switched in place");
            let output = gpu.prepare_frame(&frame).expect("targets");
            let egui_renderer = rs.renderer.read();
            let vr = egui_renderer
                .callback_resources
                .get::<VideoRenderer>()
                .expect("renderer installed");
            assert_eq!(vr.upscaler(), Upscaler::MetalFx);
            let expected = match vr.active_upscaler() {
                Upscaler::MetalFx => "FX",
                Upscaler::Fsr => "FSR",
                Upscaler::Bypass => "",
            };
            assert_eq!(output.upscaler_label, expected);
            assert!(output.upscaled, "{output:?}");
        }
    }

    #[test]
    fn persistent_gpu_failures_fall_back_to_the_cpu_path() {
        let Some(rs) = render_state() else {
            eprintln!("no wgpu adapter; skipping");
            return;
        };
        let mut s = connected();
        s.hud_enabled = true;
        s.stats.frames_per_second = 60.0;
        s.stats.negotiated_height = 36;
        let Fixture {
            mut view,
            rx,
            mut writer,
            ..
        } = fixture(s);
        let gpu = GpuVideo::new(&rs, &AppSettings::default());
        // What GPU_FALLBACK_AFTER_FAILURES failed paint callbacks would have recorded.
        for _ in 0..GPU_FALLBACK_AFTER_FAILURES {
            gpu.health.record_failure();
        }
        view.set_gpu(Some(gpu));
        assert_eq!(view.video_path(), VideoPath::Gpu);

        writer.submit(cpu_frame(64, 36, 128, 128, 128, ColorInfo::default()));
        {
            let mut h = harness(&mut view);
            h.run();
            // The HUD tells the user which path draws.
            h.get_by_label(&format!(
                "36p  60 fps  {}",
                super::super::hud::CPU_VIDEO_LABEL
            ));
        }
        assert_eq!(view.video_path(), VideoPath::Cpu);
        assert!(view.cpu_fallback());
        assert_eq!(view.output_size(), Some((64, 36)));
        assert_eq!(set_output_heights(&drain(&rx)), [(36, false)]);

        // A new launch tries the GPU again.
        view.reset();
        assert!(!view.cpu_fallback());
        assert_eq!(view.video_path(), VideoPath::Gpu);
        assert_eq!(view.gpu().map(GpuVideo::consecutive_failures), Some(0));
    }

    #[test]
    fn output_reports_go_out_on_change_and_are_rate_limited() {
        let Fixture { mut view, rx, .. } = fixture(connected());
        let a = VideoOutput {
            width: 3840,
            height: 2160,
            upscaled: true,
            upscaler_label: "FSR",
            gpu_ms: 0.8,
            frame_age_ms: 10.0,
            present_ms: 17.0,
        };
        view.report_output(a, 0.0);
        assert_eq!(
            drain(&rx),
            [StreamCommand::SetOutput {
                height: 2160,
                upscaled: true,
                upscaler_label: "FSR",
                gpu_ms: 0.8,
                frame_age_ms: 10.0,
                present_ms: 17.0
            }]
        );
        // A different upscaler running (MetalFX ↔ FSR fallback): immediate, like a size change.
        view.report_output(
            VideoOutput {
                upscaler_label: "FX",
                ..a
            },
            0.01,
        );
        assert!(matches!(
            drain(&rx).as_slice(),
            [StreamCommand::SetOutput {
                upscaler_label: "FX",
                ..
            }]
        ));
        view.report_output(a, 0.02);
        assert_eq!(drain(&rx).len(), 1);
        // Timing-only change within the interval: nothing.
        view.report_output(VideoOutput { gpu_ms: 0.9, ..a }, 0.1);
        assert!(drain(&rx).is_empty());
        // After the interval: reported.
        view.report_output(VideoOutput { gpu_ms: 0.9, ..a }, 0.6);
        assert_eq!(drain(&rx).len(), 1);
        // Unchanged timings: nothing, however long it has been.
        view.report_output(VideoOutput { gpu_ms: 0.9, ..a }, 5.0);
        assert!(drain(&rx).is_empty());
        // A present-time change alone is a timing change too.
        view.report_output(
            VideoOutput {
                gpu_ms: 0.9,
                present_ms: 33.0,
                ..a
            },
            5.0,
        );
        assert!(matches!(
            drain(&rx).as_slice(),
            [StreamCommand::SetOutput { present_ms, .. }] if *present_ms == 33.0
        ));
        // Size change: immediate.
        view.report_output(
            VideoOutput {
                height: 1080,
                upscaled: false,
                ..a
            },
            5.01,
        );
        assert_eq!(
            drain(&rx),
            [StreamCommand::SetOutput {
                height: 1080,
                upscaled: false,
                upscaler_label: "FSR",
                gpu_ms: 0.8,
                frame_age_ms: 10.0,
                present_ms: 17.0
            }]
        );
    }
}
