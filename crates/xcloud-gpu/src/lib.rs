//! GPU video path of the xCloud client: wgpu port of `qt/src/app/MetalVideoItem.{h,mm}`
//! plus the output-size/upscale state `qt/src/app/StreamController.h` exposes to the HUD
//! (`outputHeight`, `upscaled`, `gpuVideo`).
//!
//! * [`import`]: decoded [`xcloud_media::Frame`] → wgpu textures. Zero-copy on macOS
//!   (`CVPixelBuffer` IOSurface planes wrapped through `CVMetalTextureCache` and
//!   `Device::create_texture_from_hal`), `Queue::write_texture` everywhere else.
//! * [`renderer`]: WGSL NV12 → RGBA (the Metal `convertVideo` kernel), AMD FSR 1.0
//!   EASU + RCAS upscaling to at most 3840×2160 with the Qt item's size policy, encoded
//!   into the caller's (egui's) command encoder, GPU timings via timestamp queries.
//!   The upscaler is selectable at runtime ([`Upscaler`]: FSR, MetalFX, bypass).
//! * [`metalfx`]: the Qt item's `MTLFXSpatialScaler` on wgpu's Metal device/command
//!   buffer, behind `--features metalfx` (macOS); stubs elsewhere.
//! * [`egui_glue`]: the output texture as an `egui::TextureId`, drawn letterboxed.
//!
//! The renderer never waits on the GPU in the common case: imported frames stay alive
//! until `Queue::on_submitted_work_done` fires for the submission that used them.

pub mod egui_glue;
pub mod error;
pub mod import;
pub mod metalfx;
pub mod renderer;

pub use egui_glue::{VideoTexture, letterbox};
pub use error::{Error, MetalFxError, Result};
pub use import::{FORCE_CPU_UPLOAD_ENV, FrameImporter, ImportedFrame};
pub use renderer::{
    DEFAULT_SHARPNESS, DISABLE_UPSCALE_ENV, GpuTimings, MAX_IN_FLIGHT, TARGET_HEIGHT, TARGET_WIDTH,
    UPSCALER_ENV, UpscalePolicy, Upscaler, VideoRenderer, select_upscaler,
};
