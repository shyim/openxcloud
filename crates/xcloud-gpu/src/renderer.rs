//! The GPU half of `qt/src/app/MetalVideoItem.mm` (`VideoNode::render`): NV12 → RGBA
//! conversion, spatial upscaling to at most 3840×2160 and an output texture the UI samples.
//!
//! Differences from the Qt node, all deliberate (docs/archive/PORTING_PLAN.md sections 4–5):
//!
//! * The passes are encoded into the **caller's** command encoder ([`VideoRenderer::prepare`]),
//!   which egui submits on its own queue before its render pass. Same queue ⇒ ordered ⇒
//!   no `waitUntilCompleted`.
//! * The upscaler is selectable at runtime ([`Upscaler`]): AMD FSR 1.0 (EASU + RCAS) in
//!   WGSL by default so the same code runs on every backend, the Qt item's
//!   `MTLFXSpatialScaler` when built with `--features metalfx` on macOS
//!   ([`crate::metalfx`]), or bypass. Target-size policy is identical to the Qt item.
//!   MetalFX falls back to FSR (logged once) when the device or build lacks it.
//! * Imported frames are retired through `Queue::on_submitted_work_done`
//!   ([`VideoRenderer::after_submit`]), at most [`MAX_IN_FLIGHT`] at a time.
//! * GPU time per pass comes from timestamp queries when the device has
//!   `Features::TIMESTAMP_QUERY`, instead of `GPUEndTime - GPUStartTime`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{info, warn};
use xcloud_media::{ColorInfo, Frame};

use crate::import::{FrameImporter, ImportedFrame, LOG_TARGET};
use crate::metalfx::{self, SpatialScaler};
use crate::{Error, Result};

/// WGSL of the NV12 → RGBA kernel (port of the Metal `convertVideo` kernel).
pub const NV12_TO_RGBA_WGSL: &str = include_str!("shaders/nv12_to_rgba.wgsl");
/// WGSL of FSR 1.0 EASU.
pub const FSR_EASU_WGSL: &str = include_str!("shaders/fsr_easu.wgsl");
/// WGSL of FSR 1.0 RCAS.
pub const FSR_RCAS_WGSL: &str = include_str!("shaders/fsr_rcas.wgsl");

/// Environment variable that disables GPU upscaling.
pub const DISABLE_UPSCALE_ENV: &str = "OPENXCLOUD_DISABLE_UPSCALE";

/// Environment variable that picks the upscaler: `fsr` (default), `metalfx` or `bypass`.
pub const UPSCALER_ENV: &str = "OPENXCLOUD_UPSCALER";

/// Which spatial upscaler runs after the NV12 → RGBA pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Upscaler {
    /// AMD FSR 1.0 EASU + RCAS in WGSL (every backend).
    #[default]
    Fsr,
    /// Apple `MTLFXSpatialScaler` (macOS, `--features metalfx`); falls back to [`Upscaler::Fsr`]
    /// when unavailable.
    MetalFx,
    /// No upscaling: the output is the converted frame at source size.
    Bypass,
}

impl Upscaler {
    /// Parses a `OPENXCLOUD_UPSCALER` / `--upscaler` value. Case-insensitive; `None` when unknown.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "fsr" | "fsr1" | "fsr1.0" => Some(Self::Fsr),
            "metalfx" | "metal-fx" | "fx" => Some(Self::MetalFx),
            "bypass" | "none" | "off" => Some(Self::Bypass),
            _ => None,
        }
    }

    /// The value [`Upscaler::parse`] accepts for this variant.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fsr => "fsr",
            Self::MetalFx => "metalfx",
            Self::Bypass => "bypass",
        }
    }

    /// HUD suffix, as the Qt app printed it: `1440p → 2160p FX` vs `… FSR`, nothing when
    /// not upscaling.
    pub fn label(self) -> &'static str {
        match self {
            Self::Fsr => "FSR",
            Self::MetalFx => "FX",
            Self::Bypass => "",
        }
    }

    /// `OPENXCLOUD_UPSCALER` alone: `None` when unset or unparsable (the latter is logged).
    pub fn requested_from_env() -> Option<Self> {
        let value = std::env::var(UPSCALER_ENV).ok()?;
        let parsed = Self::parse(&value);
        if parsed.is_none() {
            warn!(target: LOG_TARGET, "{UPSCALER_ENV}={value:?} is not one of fsr|metalfx|bypass; using the default");
        }
        parsed
    }

    /// `OPENXCLOUD_DISABLE_UPSCALE` ⇒ bypass, otherwise `OPENXCLOUD_UPSCALER`, otherwise FSR.
    pub fn from_env() -> Self {
        if std::env::var_os(DISABLE_UPSCALE_ENV).is_some() {
            return Self::Bypass;
        }
        Self::requested_from_env().unwrap_or_default()
    }
}

/// Decides what actually runs for one source size: nothing when the policy says the
/// size is not upscaled, FSR when MetalFX was asked for but is not available (not
/// compiled in, not Metal, `supportsDevice` false or scaler creation failed), otherwise
/// what was requested.
pub fn select_upscaler(requested: Upscaler, upscaled: bool, metalfx_available: bool) -> Upscaler {
    if !upscaled {
        return Upscaler::Bypass;
    }
    match requested {
        Upscaler::MetalFx if !metalfx_available => Upscaler::Fsr,
        other => other,
    }
}

/// Upper bound of the upscaled output, as in `MetalVideoItem.mm` (`QSize(3840, 2160)`).
pub const TARGET_WIDTH: u32 = 3840;
/// See [`TARGET_WIDTH`].
pub const TARGET_HEIGHT: u32 = 2160;

/// Maximum number of imported frames whose GPU work may still be unfinished.
pub const MAX_IN_FLIGHT: usize = 3;

/// Default RCAS sharpness in stops (0 = sharpest, 2 = softest).
pub const DEFAULT_SHARPNESS: f32 = 0.25;

const WORKGROUP: u32 = 16;
const TIMESTAMP_COUNT: u32 = 6;
const TIMESTAMP_BYTES: u64 = TIMESTAMP_COUNT as u64 * wgpu::QUERY_SIZE as u64;
const TIMING_SLOTS: usize = 4;

/// Decides the output size for a source size. Mirrors `VideoNode::render` in
/// `MetalVideoItem.mm`: upscale only when the source is strictly smaller than the target
/// in both dimensions, scale to fit keeping the aspect ratio (Qt `QSize::scaled` integer
/// math), never downscale, and allow an environment switch to bypass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpscalePolicy {
    /// `false` bypasses the upscaler entirely (`OPENXCLOUD_DISABLE_UPSCALE`).
    pub enabled: bool,
    /// Largest output width.
    pub max_width: u32,
    /// Largest output height.
    pub max_height: u32,
}

impl Default for UpscalePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_width: TARGET_WIDTH,
            max_height: TARGET_HEIGHT,
        }
    }
}

impl UpscalePolicy {
    /// The default policy, disabled when `OPENXCLOUD_DISABLE_UPSCALE` is set.
    pub fn from_env() -> Self {
        Self {
            enabled: std::env::var_os(DISABLE_UPSCALE_ENV).is_none(),
            ..Self::default()
        }
    }

    /// Output size for a `width`×`height` source and whether that means upscaling.
    pub fn output_size(&self, width: u32, height: u32) -> ((u32, u32), bool) {
        if !self.enabled
            || width == 0
            || height == 0
            || width >= self.max_width
            || height >= self.max_height
        {
            return ((width, height), false);
        }
        // QSize::scaled(QSize(max_w, max_h), Qt::KeepAspectRatio), 64-bit integer math.
        let rw = u64::from(self.max_height) * u64::from(width) / u64::from(height);
        let target = if rw <= u64::from(self.max_width) {
            (rw as u32, self.max_height)
        } else {
            let rh = u64::from(self.max_width) * u64::from(height) / u64::from(width);
            (self.max_width, rh as u32)
        };
        if target == (width, height) {
            return (target, false);
        }
        (target, true)
    }
}

/// `FsrEasuCon` from `ffx_fsr1.h`: the four constant vectors EASU needs.
pub fn easu_constants(
    input_width: u32,
    input_height: u32,
    output_width: u32,
    output_height: u32,
) -> [[f32; 4]; 4] {
    let (iw, ih) = (input_width as f32, input_height as f32);
    let (ow, oh) = (output_width as f32, output_height as f32);
    [
        // Output integer position to a source position in pixel-space.
        [iw / ow, ih / oh, 0.5 * iw / ow - 0.5, 0.5 * ih / oh - 0.5],
        // Viewport pixel position to normalized image space (also the gather offsets).
        [1.0 / iw, 1.0 / ih, 1.0 / iw, -1.0 / ih],
        [-1.0 / iw, 2.0 / ih, 1.0 / iw, 2.0 / ih],
        [0.0, 4.0 / ih, 0.0, 0.0],
    ]
}

/// `FsrRcasCon` from `ffx_fsr1.h`: `exp2(-sharpness)` as the first constant; the second
/// one is this port's denoise switch.
pub fn rcas_constants(sharpness_stops: f32, denoise: bool) -> [f32; 4] {
    [
        (-sharpness_stops).exp2(),
        if denoise { 1.0 } else { 0.0 },
        0.0,
        0.0,
    ]
}

/// GPU time of the last frame for which a timestamp readback completed.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GpuTimings {
    /// NV12 → RGBA pass.
    pub convert_ms: f64,
    /// EASU pass (0 when bypassed).
    pub easu_ms: f64,
    /// RCAS pass (0 when bypassed).
    pub rcas_ms: f64,
    /// MetalFX spatial scaler (0 unless [`Upscaler::MetalFx`] is active). Measured as the
    /// `GPUEndTime − GPUStartTime` of the command buffer that carried it, not with a
    /// timestamp query, so it may be a frame older than the other fields.
    pub metalfx_ms: f64,
}

impl GpuTimings {
    /// Sum of the passes.
    pub fn total_ms(&self) -> f64 {
        self.convert_ms + self.easu_ms + self.rcas_ms + self.metalfx_ms
    }
}

/// A texture we render into, with the view(s) needed to bind it.
struct Target {
    texture: wgpu::Texture,
    /// Storage view (`Rgba8Unorm`), bound as `texture_storage_2d`.
    storage: wgpu::TextureView,
    /// Sampled view (`Rgba8Unorm`). The pixels are gamma-encoded video (the NV12 shader
    /// does no transfer-function work, exactly like the Qt Metal kernel), and egui-wgpu
    /// 0.37 samples textures as gamma values ("We expect normal textures that are NOT
    /// sRGB-aware" in egui.wgsl). Handing it an `Rgba8UnormSrgb` view made the GPU decode
    /// gamma→linear on sampling and egui decode again: a visibly dark picture.
    sampled: wgpu::TextureView,
    width: u32,
    height: u32,
}

impl Target {
    /// `render_target` adds `RENDER_ATTACHMENT`, which MetalFX needs on its output
    /// (`scaler.outputTextureUsage` includes `MTLTextureUsageRenderTarget`).
    fn new(
        device: &wgpu::Device,
        label: &str,
        width: u32,
        height: u32,
        display: bool,
        render_target: bool,
    ) -> Self {
        // `display` once selected an sRGB sampled view; egui-wgpu wants gamma-space
        // textures, so every target is sampled through its native Rgba8Unorm format.
        let _ = display;
        let view_formats: &[wgpu::TextureFormat] = &[];
        let mut usage = wgpu::TextureUsages::STORAGE_BINDING
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC;
        if render_target {
            usage |= wgpu::TextureUsages::RENDER_ATTACHMENT;
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage,
            view_formats,
        });
        let storage = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some(label),
            format: Some(wgpu::TextureFormat::Rgba8Unorm),
            usage: Some(wgpu::TextureUsages::STORAGE_BINDING),
            ..Default::default()
        });
        let sampled = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some(label),
            format: Some(wgpu::TextureFormat::Rgba8Unorm),
            usage: Some(wgpu::TextureUsages::TEXTURE_BINDING),
            ..Default::default()
        });
        Self {
            texture,
            storage,
            sampled,
            width,
            height,
        }
    }
}

struct Pass {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
    uniform: wgpu::Buffer,
}

fn storage_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format: wgpu::TextureFormat::Rgba8Unorm,
            view_dimension: wgpu::TextureViewDimension::D2,
        },
        count: None,
    }
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn make_pass(
    device: &wgpu::Device,
    label: &str,
    source: &str,
    entry_point: &str,
    entries: &[wgpu::BindGroupLayoutEntry],
    uniform_bytes: u64,
) -> Pass {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries,
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        module: &module,
        entry_point: Some(entry_point),
        compilation_options: Default::default(),
        cache: None,
    });
    let uniform = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: uniform_bytes,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    Pass {
        pipeline,
        layout,
        uniform,
    }
}

fn f32_bytes(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn dispatch_size(pixels: u32) -> u32 {
    pixels.div_ceil(WORKGROUP)
}

fn timestamp_writes(
    timing: Option<&Timing>,
    slot: Option<usize>,
    begin: u32,
) -> Option<wgpu::ComputePassTimestampWrites<'_>> {
    match (timing, slot) {
        (Some(t), Some(_)) => Some(wgpu::ComputePassTimestampWrites {
            query_set: &t.query_set,
            beginning_of_pass_write_index: Some(begin),
            end_of_pass_write_index: Some(begin + 1),
        }),
        _ => None,
    }
}

/// Imported frames whose submission has been made and whose completion we wait for.
struct InFlight {
    _frames: Vec<ImportedFrame>,
    /// MetalFX scalers replaced by a size change while their last encode may still run.
    _scalers: Vec<SpatialScaler>,
    done: Arc<AtomicBool>,
}

/// Timestamp query plumbing. Values travel: query set → resolve buffer → one of
/// [`TIMING_SLOTS`] mappable buffers → `map_async` (issued after the submit) → read on a
/// later frame.
struct Timing {
    query_set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    slots: Vec<TimingSlot>,
    period_ns: f32,
    latest: Option<GpuTimings>,
}

struct TimingSlot {
    buffer: wgpu::Buffer,
    state: SlotState,
    mapped: Arc<Mutex<Option<bool>>>,
    upscaled: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Free,
    Copied,
    Mapping,
}

impl Timing {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let query_set = device.create_query_set(&wgpu::QuerySetDescriptor {
            label: Some("video.timestamps"),
            ty: wgpu::QueryType::Timestamp,
            count: TIMESTAMP_COUNT,
        });
        let resolve = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("video.timestamps.resolve"),
            size: TIMESTAMP_BYTES,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let slots = (0..TIMING_SLOTS)
            .map(|_| TimingSlot {
                buffer: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("video.timestamps.readback"),
                    size: TIMESTAMP_BYTES,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                state: SlotState::Free,
                mapped: Arc::new(Mutex::new(None)),
                upscaled: false,
            })
            .collect();
        Self {
            query_set,
            resolve,
            slots,
            period_ns: queue.get_timestamp_period(),
            latest: None,
        }
    }

    /// Picks a free readback slot for this frame, if any.
    fn free_slot(&mut self) -> Option<usize> {
        self.slots.iter().position(|s| s.state == SlotState::Free)
    }

    /// Encodes resolve + copy into `slot` after the passes. `fsr` says whether the EASU
    /// and RCAS queries (2..6) were written this frame.
    fn encode_readback(&mut self, encoder: &mut wgpu::CommandEncoder, slot: usize, fsr: bool) {
        let queries = if fsr { 0..TIMESTAMP_COUNT } else { 0..2 };
        let bytes = u64::from(queries.end) * u64::from(wgpu::QUERY_SIZE);
        encoder.resolve_query_set(&self.query_set, queries, &self.resolve, 0);
        let s = &mut self.slots[slot];
        encoder.copy_buffer_to_buffer(&self.resolve, 0, &s.buffer, 0, bytes);
        s.state = SlotState::Copied;
        s.upscaled = fsr;
    }

    /// Called after the submission: collects finished readbacks, starts pending ones.
    fn after_submit(&mut self) {
        for slot in &mut self.slots {
            match slot.state {
                SlotState::Mapping => {
                    let outcome = slot.mapped.lock().map(|mut m| m.take()).unwrap_or(None);
                    match outcome {
                        Some(true) => {
                            let values: Vec<u64> = match slot.buffer.slice(..).get_mapped_range() {
                                Ok(data) => data
                                    .as_chunks::<8>()
                                    .0
                                    .iter()
                                    .map(|c| u64::from_le_bytes(*c))
                                    .collect(),
                                Err(_) => Vec::new(),
                            };
                            slot.buffer.unmap();
                            slot.state = SlotState::Free;
                            let ms = |a: usize, b: usize| -> f64 {
                                match (values.get(a), values.get(b)) {
                                    (Some(&t0), Some(&t1)) if t1 >= t0 => {
                                        (t1 - t0) as f64 * f64::from(self.period_ns) / 1e6
                                    }
                                    _ => 0.0,
                                }
                            };
                            let (easu_ms, rcas_ms) = if slot.upscaled {
                                (ms(2, 3), ms(4, 5))
                            } else {
                                (0.0, 0.0)
                            };
                            self.latest = Some(GpuTimings {
                                convert_ms: ms(0, 1),
                                easu_ms,
                                rcas_ms,
                                metalfx_ms: 0.0,
                            });
                        }
                        Some(false) => {
                            slot.state = SlotState::Free;
                        }
                        None => {}
                    }
                }
                SlotState::Copied => {
                    let mapped = Arc::clone(&slot.mapped);
                    slot.buffer
                        .slice(..)
                        .map_async(wgpu::MapMode::Read, move |result| {
                            if let Ok(mut m) = mapped.lock() {
                                *m = Some(result.is_ok());
                            }
                        });
                    slot.state = SlotState::Mapping;
                }
                SlotState::Free => {}
            }
        }
    }
}

/// Converts and upscales decoded frames into an RGBA texture for the UI.
pub struct VideoRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    importer: FrameImporter,
    policy: UpscalePolicy,
    /// What the user asked for.
    upscaler: Upscaler,
    /// What runs for the current targets ([`select_upscaler`]).
    active: Upscaler,
    /// `metalfx::supported` for this device, computed on first need.
    metalfx_supported: Option<bool>,
    /// The MetalFX fallback has been logged (once per renderer).
    metalfx_fallback_logged: bool,
    /// The MetalFX scaler for the current size (only when `active == MetalFx`).
    scaler: Option<SpatialScaler>,
    /// Scalers replaced since the last `after_submit`, kept alive until the GPU is done.
    retired_scalers: Vec<SpatialScaler>,
    convert: Pass,
    easu: Pass,
    rcas: Pass,
    uv_sampler: wgpu::Sampler,
    /// Conversion output at source size (only when upscaling).
    rgb: Option<Target>,
    /// EASU output at target size (only when upscaling).
    easu_out: Option<Target>,
    /// What the UI displays.
    output: Option<Target>,
    easu_bind_group: Option<wgpu::BindGroup>,
    rcas_bind_group: Option<wgpu::BindGroup>,
    source_size: (u32, u32),
    upscaled: bool,
    output_generation: u64,
    sharpness: f32,
    denoise: bool,
    rcas_dirty: bool,
    pending: Vec<ImportedFrame>,
    pending_decoded_at: Option<Instant>,
    in_flight: VecDeque<InFlight>,
    stalls: u64,
    frames_prepared: u64,
    last_zero_copy: bool,
    timing: Option<Timing>,
    gpu_done_age_us: Arc<AtomicU64>,
    max_prepare: Duration,
}

impl VideoRenderer {
    /// Creates the pipelines. Upscaling follows [`UpscalePolicy::from_env`] and the
    /// upscaler [`Upscaler::from_env`] (`OPENXCLOUD_UPSCALER`, default FSR).
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        Self::with_upscaler(
            device,
            queue,
            UpscalePolicy::from_env(),
            Upscaler::from_env(),
        )
    }

    /// Like [`VideoRenderer::new`] with an explicit upscaling policy. The upscaler still
    /// honours `OPENXCLOUD_UPSCALER` (default FSR); use [`VideoRenderer::with_upscaler`] or
    /// [`VideoRenderer::set_upscaler`] to pick it in code.
    pub fn with_policy(device: &wgpu::Device, queue: &wgpu::Queue, policy: UpscalePolicy) -> Self {
        Self::with_upscaler(
            device,
            queue,
            policy,
            Upscaler::requested_from_env().unwrap_or_default(),
        )
    }

    /// Like [`VideoRenderer::with_policy`] with an explicit [`Upscaler`].
    pub fn with_upscaler(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        policy: UpscalePolicy,
        upscaler: Upscaler,
    ) -> Self {
        let convert = make_pass(
            device,
            "video.nv12_to_rgba",
            NV12_TO_RGBA_WGSL,
            "convert_video",
            &[
                texture_entry(0),
                texture_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                storage_entry(3),
                uniform_entry(4),
            ],
            32,
        );
        let easu = make_pass(
            device,
            "video.fsr_easu",
            FSR_EASU_WGSL,
            "easu",
            &[texture_entry(0), storage_entry(1), uniform_entry(2)],
            64,
        );
        let rcas = make_pass(
            device,
            "video.fsr_rcas",
            FSR_RCAS_WGSL,
            "rcas",
            &[texture_entry(0), storage_entry(1), uniform_entry(2)],
            16,
        );
        let uv_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("video.uv_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let timing = if device.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
            Some(Timing::new(device, queue))
        } else {
            info!(target: LOG_TARGET, "device has no TIMESTAMP_QUERY feature; GPU timings unavailable");
            None
        };
        Self {
            device: device.clone(),
            queue: queue.clone(),
            importer: FrameImporter::new(device),
            policy,
            upscaler,
            active: Upscaler::Bypass,
            metalfx_supported: None,
            metalfx_fallback_logged: false,
            scaler: None,
            retired_scalers: Vec::new(),
            convert,
            easu,
            rcas,
            uv_sampler,
            rgb: None,
            easu_out: None,
            output: None,
            easu_bind_group: None,
            rcas_bind_group: None,
            source_size: (0, 0),
            upscaled: false,
            output_generation: 0,
            sharpness: DEFAULT_SHARPNESS,
            denoise: true,
            rcas_dirty: true,
            pending: Vec::new(),
            pending_decoded_at: None,
            in_flight: VecDeque::new(),
            stalls: 0,
            frames_prepared: 0,
            last_zero_copy: false,
            timing,
            gpu_done_age_us: Arc::new(AtomicU64::new(u64::MAX)),
            max_prepare: Duration::ZERO,
        }
    }

    /// RCAS sharpness in stops, clamped to 0–2 (0 = sharpest).
    pub fn set_sharpness(&mut self, stops: f32) {
        let stops = stops.clamp(0.0, 2.0);
        if stops != self.sharpness {
            self.sharpness = stops;
            self.rcas_dirty = true;
        }
    }

    /// Current RCAS sharpness in stops.
    pub fn sharpness(&self) -> f32 {
        self.sharpness
    }

    /// Enables the RCAS noise damping (on by default; game streams carry codec noise).
    pub fn set_denoise(&mut self, denoise: bool) {
        if denoise != self.denoise {
            self.denoise = denoise;
            self.rcas_dirty = true;
        }
    }

    /// The upscaling policy in use.
    pub fn policy(&self) -> UpscalePolicy {
        self.policy
    }

    /// The upscaler that was requested (constructor, `OPENXCLOUD_UPSCALER` or
    /// [`VideoRenderer::set_upscaler`]).
    pub fn upscaler(&self) -> Upscaler {
        self.upscaler
    }

    /// The upscaler actually running for the current source size: [`Upscaler::Bypass`]
    /// when the size is not upscaled, [`Upscaler::Fsr`] when MetalFX was requested but is
    /// unavailable.
    pub fn active_upscaler(&self) -> Upscaler {
        self.active
    }

    /// `"FSR"`, `"FX"` or `""` for the HUD (`1440p → 2160p FX` in the Qt app).
    pub fn upscaler_label(&self) -> &'static str {
        self.active.label()
    }

    /// Switches the upscaler. The targets are rebuilt on the next
    /// [`VideoRenderer::ensure_targets`] / [`VideoRenderer::prepare`], which bumps
    /// [`VideoRenderer::output_generation`].
    pub fn set_upscaler(&mut self, upscaler: Upscaler) {
        if upscaler != self.upscaler {
            self.upscaler = upscaler;
            self.metalfx_fallback_logged = false;
            // Force `ensure_targets` to rebuild for the current size.
            self.output = None;
        }
    }

    /// Whether `MTLFXSpatialScaler` can be used on this device (compiled in, Metal
    /// backend, `supportsDevice`). Cached after the first call.
    pub fn metalfx_supported(&mut self) -> bool {
        if let Some(v) = self.metalfx_supported {
            return v;
        }
        let supported = metalfx::COMPILED && metalfx::supported(&self.device);
        self.metalfx_supported = Some(supported);
        supported
    }

    /// Logs the MetalFX → FSR fallback once per renderer / upscaler change.
    fn log_metalfx_fallback(&mut self, reason: &dyn std::fmt::Display) {
        if !self.metalfx_fallback_logged {
            self.metalfx_fallback_logged = true;
            warn!(target: LOG_TARGET, "MetalFX requested but unavailable, using FSR 1.0: {reason}");
        }
    }

    /// Makes sure the intermediate and output textures exist for a `width`×`height`
    /// source. Called by [`VideoRenderer::prepare`]; call it earlier when the UI needs the
    /// output view (to register it with egui) before the first frame is encoded.
    pub fn ensure_targets(&mut self, width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 {
            return Err(Error::EmptyFrame { width, height });
        }
        if self.source_size == (width, height) && self.output.is_some() {
            return Ok(());
        }
        let ((out_w, out_h), upscaled) = if self.upscaler == Upscaler::Bypass {
            ((width, height), false)
        } else {
            self.policy.output_size(width, height)
        };
        self.source_size = (width, height);
        self.upscaled = upscaled;
        if let Some(old) = self.scaler.take() {
            self.retired_scalers.push(old);
        }
        let metalfx_available = self.upscaler == Upscaler::MetalFx && self.metalfx_supported();
        if self.upscaler == Upscaler::MetalFx && upscaled && !metalfx_available {
            let reason = if metalfx::COMPILED {
                "MTLFXSpatialScalerDescriptor.supportsDevice is false or wgpu is not on Metal"
            } else {
                "not compiled in (build with --features metalfx on macOS)"
            };
            self.log_metalfx_fallback(&reason);
        }
        let mut active = select_upscaler(self.upscaler, upscaled, metalfx_available);
        let output = Target::new(
            &self.device,
            "video.output",
            out_w,
            out_h,
            true,
            active == Upscaler::MetalFx,
        );
        self.output_generation += 1;
        if active == Upscaler::MetalFx {
            let rgb = Target::new(&self.device, "video.rgb", width, height, false, false);
            match SpatialScaler::new(
                &self.device,
                &rgb.texture,
                &output.texture,
                (width, height),
                (out_w, out_h),
            ) {
                Ok(scaler) => {
                    // wgpu never sees MetalFX's writes, so its init tracker would zero the
                    // output before the first wgpu read (egui's sampling, a readback).
                    // One wgpu-side clear marks the texture initialised for good.
                    self.mark_initialised(&output);
                    self.scaler = Some(scaler);
                    self.rgb = Some(rgb);
                    self.easu_out = None;
                    self.easu_bind_group = None;
                    self.rcas_bind_group = None;
                }
                Err(e) => {
                    self.log_metalfx_fallback(&e);
                    active = Upscaler::Fsr;
                }
            }
        }
        self.output = Some(output);
        self.active = active;
        if active == Upscaler::Fsr {
            let rgb = Target::new(&self.device, "video.rgb", width, height, false, false);
            let easu_out = Target::new(&self.device, "video.easu", out_w, out_h, false, false);
            let output = self
                .output
                .as_ref()
                .ok_or(Error::EmptyFrame { width, height })?;
            self.queue.write_buffer(
                &self.easu.uniform,
                0,
                &f32_bytes(&easu_constants(width, height, out_w, out_h).concat()),
            );
            self.easu_bind_group =
                Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("video.fsr_easu"),
                    layout: &self.easu.layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&rgb.sampled),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&easu_out.storage),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: self.easu.uniform.as_entire_binding(),
                        },
                    ],
                }));
            self.rcas_bind_group =
                Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("video.fsr_rcas"),
                    layout: &self.rcas.layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&easu_out.sampled),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&output.storage),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: self.rcas.uniform.as_entire_binding(),
                        },
                    ],
                }));
            self.rgb = Some(rgb);
            self.easu_out = Some(easu_out);
        } else if active == Upscaler::Bypass {
            self.rgb = None;
            self.easu_out = None;
            self.easu_bind_group = None;
            self.rcas_bind_group = None;
        }
        // Same line as the Qt item's "VideoToolbox/Metal source=… output=… MetalFX=…".
        info!(
            target: LOG_TARGET,
            "video source={}x{} output={}x{} upscaler={} MetalFX={} zero-copy={}",
            width,
            height,
            out_w,
            out_h,
            active.as_str(),
            active == Upscaler::MetalFx,
            self.importer.zero_copy_available()
        );
        Ok(())
    }

    /// Clears `target` through wgpu (a render pass with `LoadOp::Clear`) so wgpu-core's
    /// texture init tracker records it as initialised. Needed for textures written only by
    /// MetalFX; the target must have `RENDER_ATTACHMENT`.
    fn mark_initialised(&self, target: &Target) {
        let view = target.texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("video.output.clear"),
            format: Some(wgpu::TextureFormat::Rgba8Unorm),
            usage: Some(wgpu::TextureUsages::RENDER_ATTACHMENT),
            ..Default::default()
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("video.output.clear"),
            });
        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("video.output.clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        self.queue.submit([encoder.finish()]);
    }

    /// Imports `frame` and encodes NV12 → RGBA (+ EASU + RCAS when upscaling with FSR)
    /// into `encoder`. Returns the view the UI should sample (gamma-space `Rgba8Unorm` view of the output).
    ///
    /// With [`Upscaler::MetalFx`] active the convert pass is instead encoded into an
    /// encoder of the renderer's own and submitted on the queue immediately, followed by
    /// the scaler's `MTLCommandBuffer`; `encoder` is left untouched. Either way the caller
    /// must submit `encoder` on this renderer's queue afterwards and then call
    /// [`VideoRenderer::after_submit`] so the imported frame can be released when the GPU
    /// is done with it.
    pub fn prepare(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        frame: &Frame,
    ) -> Result<&wgpu::TextureView> {
        let t0 = Instant::now();
        self.retire_finished();
        self.throttle_in_flight();
        self.ensure_targets(frame.width, frame.height)?;

        let imported = self.importer.import(&self.device, &self.queue, frame)?;
        self.last_zero_copy = imported.zero_copy;

        let output = self.output.as_ref().ok_or(Error::EmptyFrame {
            width: frame.width,
            height: frame.height,
        })?;
        let convert_target = if self.active == Upscaler::Bypass {
            output
        } else {
            self.rgb.as_ref().unwrap_or(output)
        };

        self.queue.write_buffer(
            &self.convert.uniform,
            0,
            &f32_bytes(&convert_coefficients(&frame.color)),
        );
        if self.rcas_dirty {
            self.queue.write_buffer(
                &self.rcas.uniform,
                0,
                &f32_bytes(&rcas_constants(self.sharpness, self.denoise)),
            );
            self.rcas_dirty = false;
        }

        let convert_bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("video.nv12_to_rgba"),
            layout: &self.convert.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&imported.y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&imported.uv_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.uv_sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&convert_target.storage),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: self.convert.uniform.as_entire_binding(),
                },
            ],
        });

        let timing_slot = self.timing.as_mut().and_then(Timing::free_slot);
        let timestamps = |begin: u32| timestamp_writes(self.timing.as_ref(), timing_slot, begin);

        // With MetalFX the convert pass goes into an encoder of our own that is submitted
        // right away, so the scaler's command buffer (committed next, on the same
        // MTLCommandQueue) runs after it and before the caller's encoder. See metalfx.rs.
        let mut own_encoder = (self.active == Upscaler::MetalFx).then(|| {
            self.device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("video.convert"),
                })
        });
        let enc: &mut wgpu::CommandEncoder = match own_encoder.as_mut() {
            Some(own) => own,
            None => encoder,
        };

        {
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("video.nv12_to_rgba"),
                timestamp_writes: timestamps(0),
            });
            pass.set_pipeline(&self.convert.pipeline);
            pass.set_bind_group(0, &convert_bind_group, &[]);
            pass.dispatch_workgroups(
                dispatch_size(convert_target.width),
                dispatch_size(convert_target.height),
                1,
            );
        }
        if self.active == Upscaler::Fsr
            && let (Some(easu_bg), Some(rcas_bg)) = (&self.easu_bind_group, &self.rcas_bind_group)
        {
            {
                let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("video.fsr_easu"),
                    timestamp_writes: timestamps(2),
                });
                pass.set_pipeline(&self.easu.pipeline);
                pass.set_bind_group(0, easu_bg, &[]);
                pass.dispatch_workgroups(
                    dispatch_size(output.width),
                    dispatch_size(output.height),
                    1,
                );
            }
            {
                let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("video.fsr_rcas"),
                    timestamp_writes: timestamps(4),
                });
                pass.set_pipeline(&self.rcas.pipeline);
                pass.set_bind_group(0, rcas_bg, &[]);
                pass.dispatch_workgroups(
                    dispatch_size(output.width),
                    dispatch_size(output.height),
                    1,
                );
            }
        }
        if let (Some(timing), Some(slot)) = (self.timing.as_mut(), timing_slot) {
            timing.encode_readback(enc, slot, self.active == Upscaler::Fsr);
        }
        if let Some(own) = own_encoder {
            self.queue.submit([own.finish()]);
            let result = match self.scaler.as_mut() {
                Some(scaler) => scaler.encode(&self.queue),
                None => Err(metalfx::MetalFxError::NotCompiled),
            };
            if let Err(e) = result {
                // Do not keep showing a stale output: rebuild with FSR on the next frame
                // (`ensure_targets` sees a new source size).
                warn!(target: LOG_TARGET, "MetalFX encode failed, switching to FSR 1.0: {e}");
                self.metalfx_supported = Some(false);
                self.source_size = (0, 0);
            }
        }

        self.pending.push(imported);
        self.pending_decoded_at = Some(frame.t_decoded);
        self.frames_prepared += 1;
        self.max_prepare = self.max_prepare.max(t0.elapsed());
        if self.frames_prepared.is_multiple_of(300) {
            info!(
                target: LOG_TARGET,
                "frames={} maxPrepare={:.2}ms GPU={} upscaler={} inFlight={} stalls={} zeroCopy={}",
                self.frames_prepared,
                self.max_prepare.as_secs_f64() * 1e3,
                self.gpu_timings().map_or("n/a".to_string(), |t| format!("{:.2}ms", t.total_ms())),
                self.active.as_str(),
                self.in_flight.len(),
                self.stalls,
                self.last_zero_copy
            );
            self.max_prepare = Duration::ZERO;
        }
        self.output
            .as_ref()
            .map(|t| &t.sampled)
            .ok_or(Error::EmptyFrame {
                width: frame.width,
                height: frame.height,
            })
    }

    /// Tells the renderer that the encoder given to the last [`VideoRenderer::prepare`]
    /// call(s) has been submitted. Registers the completion callback that releases the
    /// imported frame(s) and advances the timestamp readback.
    pub fn after_submit(&mut self) {
        if !self.pending.is_empty() || !self.retired_scalers.is_empty() {
            let frames = std::mem::take(&mut self.pending);
            let scalers = std::mem::take(&mut self.retired_scalers);
            let done = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&done);
            let age = Arc::clone(&self.gpu_done_age_us);
            let decoded_at = self.pending_decoded_at.take();
            self.queue.on_submitted_work_done(move || {
                flag.store(true, Ordering::Release);
                if let Some(t) = decoded_at {
                    let us = u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX);
                    age.store(us, Ordering::Relaxed);
                }
            });
            self.in_flight.push_back(InFlight {
                _frames: frames,
                _scalers: scalers,
                done,
            });
        }
        if let Some(timing) = &mut self.timing {
            timing.after_submit();
        }
        self.retire_finished();
    }

    fn retire_finished(&mut self) {
        while let Some(front) = self.in_flight.front() {
            if front.done.load(Ordering::Acquire) {
                self.in_flight.pop_front();
            } else {
                break;
            }
        }
    }

    /// Keeps at most [`MAX_IN_FLIGHT`] imported frames alive: first gives wgpu a chance to
    /// run completion callbacks without blocking, then, only if the GPU is really that far
    /// behind, waits for it.
    fn throttle_in_flight(&mut self) {
        if self.in_flight.len() < MAX_IN_FLIGHT {
            return;
        }
        let _ = self.device.poll(wgpu::PollType::Poll);
        self.retire_finished();
        if self.in_flight.len() < MAX_IN_FLIGHT {
            return;
        }
        self.stalls += 1;
        warn!(target: LOG_TARGET, "{} frames in flight, waiting for the GPU (stall #{})", self.in_flight.len(), self.stalls);
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        self.retire_finished();
    }

    /// The view to sample for display, once a frame size is known.
    pub fn display_view(&self) -> Option<&wgpu::TextureView> {
        self.output.as_ref().map(|t| &t.sampled)
    }

    /// The output texture itself (for readback or MetalFX interop).
    pub fn output_texture(&self) -> Option<&wgpu::Texture> {
        self.output.as_ref().map(|t| &t.texture)
    }

    /// Output size in pixels.
    pub fn output_size(&self) -> Option<(u32, u32)> {
        self.output.as_ref().map(|t| (t.width, t.height))
    }

    /// Source (decoded) size the targets were built for.
    pub fn source_size(&self) -> Option<(u32, u32)> {
        (self.source_size != (0, 0)).then_some(self.source_size)
    }

    /// `true` when an upscaler (FSR or MetalFX) runs (the Qt `upscaled` property).
    pub fn is_upscaled(&self) -> bool {
        self.upscaled
    }

    /// The MetalFX scaler in use, if [`Upscaler::MetalFx`] is active.
    pub fn metalfx_scaler(&self) -> Option<&SpatialScaler> {
        self.scaler.as_ref()
    }

    /// Bumps every time the output texture is recreated; UIs re-register the view then.
    pub fn output_generation(&self) -> u64 {
        self.output_generation
    }

    /// Latest resolved GPU timings, `None` without `TIMESTAMP_QUERY` or before the first
    /// readback completed. With MetalFX active, `metalfx_ms` is the last completed
    /// command-buffer time of the scaler (and the only field available without
    /// `TIMESTAMP_QUERY`).
    pub fn gpu_timings(&self) -> Option<GpuTimings> {
        let metalfx_ms = self.scaler.as_ref().and_then(SpatialScaler::last_gpu_ms);
        match (self.timing.as_ref().and_then(|t| t.latest), metalfx_ms) {
            (Some(mut t), m) => {
                t.metalfx_ms = m.unwrap_or(0.0);
                Some(t)
            }
            (None, Some(metalfx_ms)) => Some(GpuTimings {
                metalfx_ms,
                ..GpuTimings::default()
            }),
            (None, None) => None,
        }
    }

    /// Whether timestamp queries are in use.
    pub fn timestamps_supported(&self) -> bool {
        self.timing.is_some()
    }

    /// Age of the most recently completed frame (decoder output → GPU finished), if a
    /// completion callback has fired yet.
    pub fn last_gpu_done_age(&self) -> Option<Duration> {
        let us = self.gpu_done_age_us.load(Ordering::Relaxed);
        (us != u64::MAX).then(|| Duration::from_micros(us))
    }

    /// Present time of the most recently completed frame in milliseconds: `t_decoded`
    /// (captured in [`VideoRenderer::prepare`]) → the `on_submitted_work_done` callback of
    /// the submission that drew it. wgpu runs that callback only when the device is
    /// polled, which happens on the next UI frame's submit, so this is an upper bound
    /// quantised to the UI frame period (docs/perf.md, "frame age at GPU done"), not the
    /// GPU completion time itself. `None` before the first completion.
    pub fn last_present_latency_ms(&self) -> Option<f64> {
        self.last_gpu_done_age().map(|d| d.as_secs_f64() * 1e3)
    }

    /// Whether the last prepared frame was imported without a copy.
    pub fn last_frame_zero_copy(&self) -> bool {
        self.last_zero_copy
    }

    /// The importer, for its counters and the zero-copy status.
    pub fn importer(&self) -> &FrameImporter {
        &self.importer
    }

    /// Imported frames waiting for GPU completion.
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Times `prepare` had to block because [`MAX_IN_FLIGHT`] frames were unfinished.
    pub fn stalls(&self) -> u64 {
        self.stalls
    }

    /// Frames encoded so far.
    pub fn frames_prepared(&self) -> u64 {
        self.frames_prepared
    }
}

/// The `float coefficients[8]` of `MetalVideoItem.mm` for this frame's colour info.
pub fn convert_coefficients(color: &ColorInfo) -> [f32; 8] {
    color.shader_coefficients()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::sync::mpsc;
    use std::task::{Context, Poll, Waker};
    use xcloud_media::{NativeBuffer, YCbCrMatrix};

    fn block_on<F: Future>(fut: F) -> F::Output {
        let mut cx = Context::from_waker(Waker::noop());
        let mut fut = std::pin::pin!(fut);
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::yield_now();
        }
    }

    fn validate_wgsl(name: &str, source: &str) {
        let module = wgpu::naga::front::wgsl::parse_str(source)
            .unwrap_or_else(|e| panic!("{name}: {}", e.emit_to_string(source)));
        wgpu::naga::valid::Validator::new(
            wgpu::naga::valid::ValidationFlags::all(),
            wgpu::naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .unwrap_or_else(|e| panic!("{name}: {e:?}"));
    }

    #[test]
    fn shaders_parse_and_validate() {
        validate_wgsl("nv12_to_rgba", NV12_TO_RGBA_WGSL);
        validate_wgsl("fsr_easu", FSR_EASU_WGSL);
        validate_wgsl("fsr_rcas", FSR_RCAS_WGSL);
    }

    #[test]
    fn shaders_carry_the_mit_header() {
        for src in [FSR_EASU_WGSL, FSR_RCAS_WGSL] {
            assert!(src.contains("Copyright (c) 2021 Advanced Micro Devices"));
            assert!(src.contains("Permission is hereby granted, free of charge"));
        }
    }

    #[test]
    fn upscale_policy_matches_metal_video_item() {
        let p = UpscalePolicy::default();
        // 1080p60 game stream → UHD, aspect kept.
        assert_eq!(p.output_size(1920, 1080), ((3840, 2160), true));
        assert_eq!(p.output_size(1280, 720), ((3840, 2160), true));
        assert_eq!(p.output_size(1600, 900), ((3840, 2160), true));
        // 21:9: width-limited, QSize::scaled integer math.
        assert_eq!(p.output_size(2560, 1080), ((3840, 1620), true));
        // Portrait-ish: height-limited.
        assert_eq!(p.output_size(1080, 1920), ((1215, 2160), true));
        // Already at or above the target in one dimension: bypass, never downscale.
        assert_eq!(p.output_size(3840, 2160), ((3840, 2160), false));
        assert_eq!(p.output_size(4096, 2160), ((4096, 2160), false));
        assert_eq!(p.output_size(1920, 2160), ((1920, 2160), false));
        assert_eq!(p.output_size(3840, 1080), ((3840, 1080), false));
        // Degenerate.
        assert_eq!(p.output_size(0, 1080), ((0, 1080), false));
    }

    #[test]
    fn upscale_policy_can_be_disabled() {
        let p = UpscalePolicy {
            enabled: false,
            ..UpscalePolicy::default()
        };
        assert_eq!(p.output_size(1920, 1080), ((1920, 1080), false));
        let small = UpscalePolicy {
            enabled: true,
            max_width: 128,
            max_height: 72,
        };
        assert_eq!(small.output_size(64, 36), ((128, 72), true));
        assert_eq!(small.output_size(128, 36), ((128, 36), false));
    }

    #[test]
    fn upscaler_parses_cli_and_env_values() {
        assert_eq!(Upscaler::parse("fsr"), Some(Upscaler::Fsr));
        assert_eq!(Upscaler::parse("FSR"), Some(Upscaler::Fsr));
        assert_eq!(Upscaler::parse(" metalfx "), Some(Upscaler::MetalFx));
        assert_eq!(Upscaler::parse("fx"), Some(Upscaler::MetalFx));
        assert_eq!(Upscaler::parse("bypass"), Some(Upscaler::Bypass));
        assert_eq!(Upscaler::parse("off"), Some(Upscaler::Bypass));
        assert_eq!(Upscaler::parse("dlss"), None);
        assert_eq!(Upscaler::parse(""), None);
        for u in [Upscaler::Fsr, Upscaler::MetalFx, Upscaler::Bypass] {
            assert_eq!(Upscaler::parse(u.as_str()), Some(u));
        }
        assert_eq!(Upscaler::default(), Upscaler::Fsr);
    }

    #[test]
    fn upscaler_labels_match_the_qt_hud() {
        // StreamHUD.qml: "1440p → 2160p FX" with MetalFX; this port prints "FSR" for FSR
        // and nothing when the frame is shown at source size.
        assert_eq!(Upscaler::MetalFx.label(), "FX");
        assert_eq!(Upscaler::Fsr.label(), "FSR");
        assert_eq!(Upscaler::Bypass.label(), "");
        let hud = |u: Upscaler| {
            format!("1440p → 2160p {}", u.label())
                .trim_end()
                .to_string()
        };
        assert_eq!(hud(Upscaler::MetalFx), "1440p → 2160p FX");
        assert_eq!(hud(Upscaler::Fsr), "1440p → 2160p FSR");
    }

    #[test]
    fn select_upscaler_falls_back_to_fsr_and_respects_bypass() {
        // Default stays FSR.
        assert_eq!(select_upscaler(Upscaler::Fsr, true, true), Upscaler::Fsr);
        assert_eq!(select_upscaler(Upscaler::Fsr, true, false), Upscaler::Fsr);
        // MetalFX only when available, FSR otherwise (not compiled in / unsupported device).
        assert_eq!(
            select_upscaler(Upscaler::MetalFx, true, true),
            Upscaler::MetalFx
        );
        assert_eq!(
            select_upscaler(Upscaler::MetalFx, true, false),
            Upscaler::Fsr
        );
        // A size the policy does not upscale runs nothing, whatever was requested.
        for u in [Upscaler::Fsr, Upscaler::MetalFx, Upscaler::Bypass] {
            assert_eq!(select_upscaler(u, false, true), Upscaler::Bypass);
            assert_eq!(select_upscaler(u, false, false), Upscaler::Bypass);
        }
        assert_eq!(
            select_upscaler(Upscaler::Bypass, true, true),
            Upscaler::Bypass
        );
    }

    #[test]
    fn metalfx_stub_reports_not_compiled_without_the_feature() {
        // Without the cargo feature (or off macOS) the renderer must never try MetalFX.
        if !metalfx::COMPILED {
            assert_eq!(
                select_upscaler(Upscaler::MetalFx, true, metalfx::COMPILED),
                Upscaler::Fsr
            );
        }
    }

    #[test]
    fn gpu_timings_total_includes_metalfx() {
        let t = GpuTimings {
            convert_ms: 0.1,
            easu_ms: 0.0,
            rcas_ms: 0.0,
            metalfx_ms: 0.7,
        };
        assert!((t.total_ms() - 0.8).abs() < 1e-9);
    }

    #[test]
    fn easu_constants_match_fsr_easu_con() {
        let c = easu_constants(1920, 1080, 3840, 2160);
        assert_eq!(c[0], [0.5, 0.5, -0.25, -0.25]);
        assert_eq!(
            c[1],
            [1.0 / 1920.0, 1.0 / 1080.0, 1.0 / 1920.0, -1.0 / 1080.0]
        );
        assert_eq!(
            c[2],
            [-1.0 / 1920.0, 2.0 / 1080.0, 1.0 / 1920.0, 2.0 / 1080.0]
        );
        assert_eq!(c[3], [0.0, 4.0 / 1080.0, 0.0, 0.0]);
    }

    #[test]
    fn rcas_constants_match_fsr_rcas_con() {
        assert_eq!(rcas_constants(0.0, false), [1.0, 0.0, 0.0, 0.0]);
        assert_eq!(rcas_constants(1.0, true), [0.5, 1.0, 0.0, 0.0]);
        assert_eq!(rcas_constants(2.0, false)[0], 0.25);
    }

    #[test]
    fn convert_coefficients_are_the_metal_float8() {
        let bytes = f32_bytes(&convert_coefficients(&ColorInfo::default()));
        assert_eq!(bytes.len(), 32);
        let limited_709 = ColorInfo::default().shader_coefficients();
        assert_eq!(limited_709[0], 16.0 / 255.0);
        assert_eq!(&limited_709[4..], &[1.5748, -0.187_324, -0.468_124, 1.8556]);
        let full_601 = convert_coefficients(&ColorInfo {
            matrix: YCbCrMatrix::Bt601,
            full_range: true,
        });
        assert_eq!(&full_601[..4], &[0.0, 1.0, 1.0, 0.0]);
    }

    // ---- GPU tests: skipped when no adapter is available (CI without a GPU) ----------

    fn gpu() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter =
            block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
        let features = adapter.features() & wgpu::Features::TIMESTAMP_QUERY;
        block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("xcloud-gpu test"),
            required_features: features,
            ..Default::default()
        }))
        .ok()
    }

    fn cpu_frame(
        w: u32,
        h: u32,
        y_fn: impl Fn(u32, u32) -> u8,
        cb: u8,
        cr: u8,
        color: ColorInfo,
    ) -> Frame {
        let mut y = vec![0u8; (w * h) as usize];
        for yy in 0..h {
            for xx in 0..w {
                y[(yy * w + xx) as usize] = y_fn(xx, yy);
            }
        }
        let uv: Vec<u8> = (0..(w.div_ceil(2) * h.div_ceil(2)))
            .flat_map(|_| [cb, cr])
            .collect();
        let now = Instant::now();
        Frame {
            buf: NativeBuffer::Nv12Cpu {
                y,
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

    fn render_and_read(
        renderer: &mut VideoRenderer,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &Frame,
    ) -> (Vec<[u8; 4]>, u32, u32) {
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        renderer.prepare(&mut encoder, frame).expect("prepare");
        let texture = renderer.output_texture().expect("output").clone();
        let (w, h) = renderer.output_size().expect("size");
        let bytes_per_row = (w * 4).div_ceil(256) * 256;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: u64::from(bytes_per_row) * u64::from(h),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);
        renderer.after_submit();
        let (tx, rx) = mpsc::channel();
        readback.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            tx.send(r).expect("send");
        });
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        rx.recv().expect("recv").expect("map");
        let data = readback.slice(..).get_mapped_range().expect("mapped range");
        let mut pixels = Vec::with_capacity((w * h) as usize);
        for row in 0..h {
            let start = (row * bytes_per_row) as usize;
            pixels.extend_from_slice(data[start..start + (w * 4) as usize].as_chunks::<4>().0);
        }
        drop(data);
        readback.unmap();
        (pixels, w, h)
    }

    fn reference_rgb(y: u8, cb: u8, cr: u8, color: &ColorInfo) -> [u8; 3] {
        let p = color.shader_coefficients();
        let l = (f32::from(y) / 255.0 - p[0]) * p[1];
        let cbf = (f32::from(cb) / 255.0 - 0.5) * p[2];
        let crf = (f32::from(cr) / 255.0 - 0.5) * p[2];
        let r = l + p[4] * crf;
        let g = l + p[5] * cbf + p[6] * crf;
        let b = l + p[7] * cbf;
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        [q(r), q(g), q(b)]
    }

    #[test]
    fn nv12_convert_matches_cpu_reference_when_bypassed() {
        let Some((device, queue)) = gpu() else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let mut renderer = VideoRenderer::with_policy(
            &device,
            &queue,
            UpscalePolicy {
                enabled: false,
                ..UpscalePolicy::default()
            },
        );
        for color in [
            ColorInfo::default(),
            ColorInfo {
                matrix: YCbCrMatrix::Bt601,
                full_range: true,
            },
            ColorInfo {
                matrix: YCbCrMatrix::Bt2020,
                full_range: false,
            },
        ] {
            let (cb, cr) = (0x60, 0xA0);
            let frame = cpu_frame(32, 16, |x, y| (x * 8 + y) as u8, cb, cr, color);
            let (pixels, w, h) = render_and_read(&mut renderer, &device, &queue, &frame);
            assert_eq!((w, h), (32, 16));
            assert!(!renderer.is_upscaled());
            assert!(!renderer.last_frame_zero_copy());
            for yy in 0..16u32 {
                for xx in 0..32u32 {
                    let got = pixels[(yy * 32 + xx) as usize];
                    let want = reference_rgb((xx * 8 + yy) as u8, cb, cr, &color);
                    for c in 0..3 {
                        assert!(
                            (i32::from(got[c]) - i32::from(want[c])).abs() <= 2,
                            "{color:?} pixel ({xx},{yy}) channel {c}: got {got:?} want {want:?}"
                        );
                    }
                    assert_eq!(got[3], 255);
                }
            }
        }
        assert_eq!(renderer.frames_prepared(), 3);
    }

    #[test]
    fn fsr_upscale_keeps_flat_colour_and_reports_size() {
        let Some((device, queue)) = gpu() else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        // Explicit upscaler: `with_policy` honours `OPENXCLOUD_UPSCALER`, and this test must
        // pass whatever the developer exported to try MetalFX or bypass in the app.
        let mut renderer = VideoRenderer::with_upscaler(
            &device,
            &queue,
            UpscalePolicy {
                enabled: true,
                max_width: 128,
                max_height: 72,
            },
            Upscaler::Fsr,
        );
        let color = ColorInfo::default();
        let frame = cpu_frame(64, 36, |_, _| 0x80, 0x70, 0x90, color);
        let want = reference_rgb(0x80, 0x70, 0x90, &color);
        let generation_before = renderer.output_generation();
        let (pixels, w, h) = render_and_read(&mut renderer, &device, &queue, &frame);
        assert_eq!(renderer.active_upscaler(), Upscaler::Fsr);
        assert_eq!((w, h), (128, 72));
        assert!(renderer.is_upscaled());
        assert_eq!(renderer.source_size(), Some((64, 36)));
        assert!(renderer.output_generation() > generation_before);
        for (i, px) in pixels.iter().enumerate() {
            for c in 0..3 {
                assert!(
                    (i32::from(px[c]) - i32::from(want[c])).abs() <= 2,
                    "pixel {i} channel {c}: got {px:?} want {want:?}"
                );
            }
        }
        // A second frame at another size recreates the output.
        let frame2 = cpu_frame(32, 18, |_, _| 0x80, 0x70, 0x90, color);
        let generation = renderer.output_generation();
        let (_, w2, h2) = render_and_read(&mut renderer, &device, &queue, &frame2);
        assert_eq!((w2, h2), (128, 72));
        assert!(renderer.output_generation() > generation);
        // Frames retire once the GPU is done.
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        renderer.after_submit();
        assert_eq!(renderer.in_flight(), 0);
        assert_eq!(renderer.stalls(), 0);
    }

    #[test]
    fn requesting_metalfx_without_support_runs_fsr_and_labels_it() {
        let Some((device, queue)) = gpu() else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let mut renderer = VideoRenderer::with_upscaler(
            &device,
            &queue,
            UpscalePolicy {
                enabled: true,
                max_width: 128,
                max_height: 72,
            },
            Upscaler::MetalFx,
        );
        assert_eq!(renderer.upscaler(), Upscaler::MetalFx);
        let supported = renderer.metalfx_supported();
        let color = ColorInfo::default();
        let frame = cpu_frame(64, 36, |_, _| 0x80, 0x70, 0x90, color);
        let (pixels, w, h) = render_and_read(&mut renderer, &device, &queue, &frame);
        assert_eq!((w, h), (128, 72));
        assert!(renderer.is_upscaled());
        if supported {
            assert_eq!(renderer.active_upscaler(), Upscaler::MetalFx);
            assert_eq!(renderer.upscaler_label(), "FX");
        } else {
            assert_eq!(renderer.active_upscaler(), Upscaler::Fsr);
            assert_eq!(renderer.upscaler_label(), "FSR");
        }
        let want = reference_rgb(0x80, 0x70, 0x90, &color);
        for (i, px) in pixels.iter().enumerate() {
            for c in 0..3 {
                assert!(
                    (i32::from(px[c]) - i32::from(want[c])).abs() <= 3,
                    "pixel {i} channel {c}: got {px:?} want {want:?}"
                );
            }
        }
        // Bypass at runtime: output at source size, no label.
        renderer.set_upscaler(Upscaler::Bypass);
        let generation = renderer.output_generation();
        let (_, w2, h2) = render_and_read(&mut renderer, &device, &queue, &frame);
        assert_eq!((w2, h2), (64, 36));
        assert!(!renderer.is_upscaled());
        assert_eq!(renderer.active_upscaler(), Upscaler::Bypass);
        assert_eq!(renderer.upscaler_label(), "");
        assert!(renderer.output_generation() > generation);
        // And back to FSR.
        renderer.set_upscaler(Upscaler::Fsr);
        let (_, w3, h3) = render_and_read(&mut renderer, &device, &queue, &frame);
        assert_eq!((w3, h3), (128, 72));
        assert_eq!(renderer.active_upscaler(), Upscaler::Fsr);
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        renderer.after_submit();
        assert_eq!(renderer.in_flight(), 0);
    }

    /// The present-time hook: `t_decoded` is captured in `prepare`, handed to the
    /// `on_submitted_work_done` closure in `after_submit` and published as
    /// `last_gpu_done_age` / `last_present_latency_ms` once the GPU finished the frame.
    #[test]
    fn present_latency_is_measured_from_t_decoded_at_gpu_completion() {
        let Some((device, queue)) = gpu() else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        let mut renderer = VideoRenderer::with_upscaler(
            &device,
            &queue,
            UpscalePolicy {
                enabled: false,
                ..UpscalePolicy::default()
            },
            Upscaler::Bypass,
        );
        // Nothing completed yet.
        assert_eq!(renderer.last_gpu_done_age(), None);
        assert_eq!(renderer.last_present_latency_ms(), None);
        assert!(renderer.pending_decoded_at.is_none());

        // A frame decoded a known time ago: the measured age must be at least that, so the
        // assertion holds whatever the GPU takes, yet still proves the closure stored it.
        let color = ColorInfo::default();
        let backdate = Duration::from_millis(5);
        let mut frame = cpu_frame(64, 36, |_, _| 0x80, 0x70, 0x90, color);
        frame.t_decoded = Instant::now()
            .checked_sub(backdate)
            .expect("backdated t_decoded");
        // `render_and_read` submits, calls `after_submit` and then polls with
        // `wait_indefinitely`, which runs the completion callback.
        let _ = render_and_read(&mut renderer, &device, &queue, &frame);
        assert!(
            renderer.pending_decoded_at.is_none(),
            "after_submit must consume the pending t_decoded"
        );
        let first = renderer
            .last_gpu_done_age()
            .expect("completion callback stored the frame age");
        assert!(first >= backdate, "age {first:?} < backdate {backdate:?}");
        let first_ms = renderer
            .last_present_latency_ms()
            .expect("present latency after completion");
        assert!(first_ms >= 5.0, "present latency {first_ms} ms < 5 ms");
        assert!(
            first_ms < 60_000.0,
            "present latency {first_ms} ms is not a sane age"
        );
        assert!((first_ms - first.as_secs_f64() * 1e3).abs() < 1e-6);

        // `after_submit` with nothing pending registers no callback: the value stays, and
        // the finished frame retires.
        renderer.after_submit();
        assert_eq!(renderer.in_flight(), 0);
        assert_eq!(renderer.last_gpu_done_age(), Some(first));

        // Every completion re-stores the age: a frame decoded much earlier reads older.
        let backdate2 = Duration::from_millis(1_500);
        let mut frame2 = cpu_frame(64, 36, |_, _| 0x80, 0x70, 0x90, color);
        frame2.t_decoded = Instant::now()
            .checked_sub(backdate2)
            .expect("backdated t_decoded");
        let _ = render_and_read(&mut renderer, &device, &queue, &frame2);
        let second = renderer
            .last_gpu_done_age()
            .expect("second completion stored the frame age");
        assert!(
            second >= backdate2,
            "age {second:?} < backdate {backdate2:?}"
        );
        assert!(second > first);
        let second_ms = renderer
            .last_present_latency_ms()
            .expect("present latency after second completion");
        assert!(
            second_ms >= 1_500.0,
            "present latency {second_ms} ms < 1500 ms"
        );
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        renderer.after_submit();
        assert_eq!(renderer.in_flight(), 0);
        assert_eq!(renderer.stalls(), 0);
    }

    #[cfg(all(target_os = "macos", feature = "metalfx"))]
    #[test]
    fn metalfx_upscale_keeps_flat_colour_and_survives_resize() {
        let Some((device, queue)) = gpu() else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        if !metalfx::supported(&device) {
            eprintln!("MetalFX spatial scaler unsupported on this device/backend; skipping");
            return;
        }
        let mut renderer = VideoRenderer::with_upscaler(
            &device,
            &queue,
            UpscalePolicy {
                enabled: true,
                max_width: 640,
                max_height: 360,
            },
            Upscaler::MetalFx,
        );
        let color = ColorInfo::default();
        let frame = cpu_frame(320, 180, |_, _| 0x80, 0x70, 0x90, color);
        let want = reference_rgb(0x80, 0x70, 0x90, &color);
        let (pixels, w, h) = render_and_read(&mut renderer, &device, &queue, &frame);
        assert_eq!((w, h), (640, 360));
        assert_eq!(
            renderer.active_upscaler(),
            Upscaler::MetalFx,
            "scaler creation must succeed on a supported device"
        );
        assert_eq!(renderer.upscaler_label(), "FX");
        let scaler = renderer.metalfx_scaler().expect("scaler");
        assert_eq!(scaler.input_size(), (320, 180));
        assert_eq!(scaler.output_size(), (640, 360));
        assert_eq!(scaler.frames(), 1);
        // The output must carry what MetalFX asked for (ShaderWrite and/or RenderTarget).
        assert_ne!(scaler.output_texture_usage() & 0x6, 0);
        // Perceptual mode round-trips a flat colour; allow a little quantisation.
        for (i, px) in pixels.iter().enumerate() {
            for c in 0..3 {
                assert!(
                    (i32::from(px[c]) - i32::from(want[c])).abs() <= 4,
                    "pixel {i} channel {c}: got {px:?} want {want:?}"
                );
            }
            assert_eq!(px[3], 255, "pixel {i} alpha");
        }
        // Resize: the scaler is recreated for the new source size, the old one retired.
        let frame2 = cpu_frame(160, 90, |_, _| 0x80, 0x70, 0x90, color);
        let generation = renderer.output_generation();
        let (pixels2, w2, h2) = render_and_read(&mut renderer, &device, &queue, &frame2);
        assert_eq!((w2, h2), (640, 360));
        assert!(renderer.output_generation() > generation);
        let scaler = renderer.metalfx_scaler().expect("scaler after resize");
        assert_eq!(scaler.input_size(), (160, 90));
        assert_eq!(scaler.frames(), 1);
        for (i, px) in pixels2.iter().enumerate() {
            for c in 0..3 {
                assert!(
                    (i32::from(px[c]) - i32::from(want[c])).abs() <= 4,
                    "resized pixel {i} channel {c}: got {px:?} want {want:?}"
                );
            }
        }
        // Completion handler delivers a GPU time for the scaler's command buffer.
        for _ in 0..3 {
            let _ = render_and_read(&mut renderer, &device, &queue, &frame2);
            device
                .poll(wgpu::PollType::wait_indefinitely())
                .expect("poll");
            renderer.after_submit();
        }
        let timings = renderer.gpu_timings().expect("timings");
        assert!(
            timings.metalfx_ms > 0.0 && timings.metalfx_ms < 100.0,
            "{timings:?}"
        );
        assert_eq!(timings.easu_ms, 0.0);
        assert_eq!(timings.rcas_ms, 0.0);
        assert_eq!(renderer.in_flight(), 0);
        assert_eq!(renderer.stalls(), 0);
    }

    #[test]
    fn fsr_upscale_sharpens_an_edge_without_ringing_past_the_inputs() {
        let Some((device, queue)) = gpu() else {
            eprintln!("no GPU adapter; skipping");
            return;
        };
        // Explicit upscaler: `with_policy` honours `OPENXCLOUD_UPSCALER`, and this test must
        // pass whatever the developer exported to try MetalFX or bypass in the app.
        let mut renderer = VideoRenderer::with_upscaler(
            &device,
            &queue,
            UpscalePolicy {
                enabled: true,
                max_width: 128,
                max_height: 72,
            },
            Upscaler::Fsr,
        );
        renderer.set_sharpness(0.0);
        let color = ColorInfo {
            matrix: YCbCrMatrix::Bt709,
            full_range: true,
        };
        // Left half dark, right half bright, neutral chroma.
        let frame = cpu_frame(
            64,
            36,
            |x, _| if x < 32 { 0x20 } else { 0xE0 },
            0x80,
            0x80,
            color,
        );
        let (pixels, w, h) = render_and_read(&mut renderer, &device, &queue, &frame);
        assert_eq!(renderer.active_upscaler(), Upscaler::Fsr);
        assert_eq!((w, h), (128, 72));
        let row = &pixels[(36 * 128) as usize..(37 * 128) as usize];
        // Far from the edge the output equals the input.
        assert!(
            (i32::from(row[4][0]) - 0x20).abs() <= 2,
            "left: {:?}",
            row[4]
        );
        assert!(
            (i32::from(row[123][0]) - 0xE0).abs() <= 2,
            "right: {:?}",
            row[123]
        );
        // EASU derings to the 4 nearest inputs, so the 2× step stays sharp: at most a
        // handful of intermediate pixels instead of a bilinear ramp.
        let intermediate = row.iter().filter(|p| p[0] > 0x30 && p[0] < 0xD0).count();
        assert!(
            intermediate <= 4,
            "edge smeared over {intermediate} pixels: {row:?}"
        );
        // RCAS overshoots on purpose, but only within FSR_RCAS_LIMIT of the local contrast
        // (0.1875 × 0.75 ≈ 36/255 at most; in practice far less at this contrast).
        assert!(
            row.iter().all(|p| p[0] >= 0x20 - 12 && p[0] <= 0xE0 + 12),
            "ringing: {row:?}"
        );
        // Dark plateau, transition, bright plateau: no second transition anywhere.
        let first_bright = row.iter().position(|p| p[0] > 0x80).expect("bright half");
        assert!(
            row[first_bright + 4..].iter().all(|p| p[0] > 0xD0),
            "right plateau: {row:?}"
        );
        assert!(
            row[..first_bright.saturating_sub(4)]
                .iter()
                .all(|p| p[0] < 0x30),
            "left plateau: {row:?}"
        );
        if renderer.timestamps_supported() {
            // Drive a few frames so a timestamp readback can complete.
            for _ in 0..4 {
                let _ = render_and_read(&mut renderer, &device, &queue, &frame);
                device
                    .poll(wgpu::PollType::wait_indefinitely())
                    .expect("poll");
                renderer.after_submit();
            }
            let timings = renderer
                .gpu_timings()
                .expect("timings after several frames");
            assert!(timings.convert_ms >= 0.0 && timings.convert_ms < 100.0);
            assert!(timings.easu_ms >= 0.0 && timings.rcas_ms >= 0.0);
        }
    }
}
