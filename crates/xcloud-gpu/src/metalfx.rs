//! MetalFX spatial upscaling: the `MTLFXSpatialScaler` half of `qt/src/app/MetalVideoItem.mm`
//! (`VideoNode::render`), driven through wgpu's Metal escape hatches.
//!
//! What the Qt node did, and what this module does the same way:
//!
//! * `MTLFXSpatialScalerDescriptor` with `inputWidth/Height` = source size,
//!   `outputWidth/Height` = the renderer's target size (`UpscalePolicy`, the Qt
//!   `QSize::scaled(QSize(3840, 2160), KeepAspectRatio)` policy), `colorProcessingMode =
//!   Perceptual`, both texture formats 8-bit unorm. Qt used `BGRA8Unorm`; this port keeps
//!   the renderer's `RGBA8Unorm` textures, which MetalFX accepts for both ends.
//! * `[MTLFXSpatialScalerDescriptor supportsDevice:]` gates the whole thing; the renderer
//!   falls back to FSR when it says no (logged once).
//! * The scaler is recreated whenever the source size changes
//!   (`VideoRenderer::ensure_targets`).
//! * `colorTexture` is the NV12 → RGBA output at source size, `outputTexture` the
//!   renderer's output, `inputContentWidth/Height` = source.
//! * The input/output textures carry `scaler.colorTextureUsage` /
//!   `scaler.outputTextureUsage`; [`SpatialScaler::new`] verifies that against the raw
//!   `MTLTexture.usage` instead of trusting the mapping.
//!
//! Metal interop with wgpu 30: the raw `MTLDevice` comes from
//! `Device::as_hal::<Metal>()`, the raw `MTLTexture`s from `Texture::as_hal`, and the
//! scaler is encoded into **a separate `MTLCommandBuffer` on wgpu's own `MTLCommandQueue`**
//! (`Queue::as_hal::<Metal>()` → `as_raw()`), committed from inside
//! `VideoRenderer::prepare` *before* the caller's encoder is submitted.
//!
//! Why not the caller's command buffer: wgpu-core 30 refuses to mix the wgpu encoding API
//! with `CommandEncoder::as_hal_mut` on one encoder (it panics with "Mixing the wgpu
//! encoding API with the raw encoding API is not permitted"), and egui's encoder has
//! already been used by the time the paint callback runs. So, with MetalFX active, the
//! renderer encodes the NV12 → RGBA pass into an encoder of its own, submits it on the
//! wgpu queue, then commits the scaler's command buffer on the same `MTLCommandQueue`;
//! eframe submits egui's encoder (which samples the output) afterwards. One queue, commit
//! order convert → scale → egui, and Metal's hazard tracking on the (default, tracked)
//! textures orders the reads after the writes without fences. The cost is one extra
//! `Queue::submit` per frame on the render thread (see docs/perf.md).
//!
//! Timing: timestamp queries cannot wrap work wgpu did not encode, so the scaler's GPU
//! time is `GPUEndTime − GPUStartTime` of its own command buffer, read in an
//! `addCompletedHandler:` block — the same measurement the Qt item used.
//!
//! Without the `metalfx` cargo feature, or off macOS, this module compiles to stubs that
//! report [`MetalFxError::NotCompiled`], so `renderer.rs` has no `cfg` of its own.

/// `true` when MetalFX support is compiled into this build.
pub const COMPILED: bool = cfg!(all(target_os = "macos", feature = "metalfx"));

/// Why a MetalFX scaler could not be created or encoded; lives in [`crate::error`] and
/// wraps into [`crate::Error::MetalFx`]. The renderer never surfaces these: it logs them
/// and falls back to FSR.
pub use crate::error::MetalFxError;

#[cfg(all(target_os = "macos", feature = "metalfx"))]
mod imp {
    use std::ptr::NonNull;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use block2::RcBlock;
    use objc2::Message as _;
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_metal::{
        MTLCommandBuffer, MTLCommandQueue, MTLDevice, MTLPixelFormat, MTLTexture, MTLTextureUsage,
    };
    use objc2_metal_fx::{
        MTLFXSpatialScaler, MTLFXSpatialScalerBase, MTLFXSpatialScalerColorProcessingMode,
        MTLFXSpatialScalerDescriptor,
    };

    use super::MetalFxError;

    /// The pixel format the renderer's RGBA targets have on Metal.
    const FORMAT: MTLPixelFormat = MTLPixelFormat::RGBA8Unorm;

    /// Borrows wgpu's `MTLDevice`. The hal guard must not outlive the call: wgpu holds
    /// internal locks while it is alive.
    fn raw_device(
        device: &wgpu::Device,
    ) -> Result<Retained<ProtocolObject<dyn MTLDevice>>, MetalFxError> {
        // SAFETY: `as_hal` only asks that we do not break wgpu's state tracking through
        // the hal device; we clone the device handle and nothing else.
        let hal =
            unsafe { device.as_hal::<wgpu::hal::api::Metal>() }.ok_or(MetalFxError::NotMetal)?;
        Ok(hal.raw_device().clone())
    }

    /// Borrows the `MTLTexture` behind a wgpu texture.
    fn raw_texture(
        texture: &wgpu::Texture,
    ) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, MetalFxError> {
        // SAFETY: we only retain the handle; wgpu keeps owning the texture.
        let hal =
            unsafe { texture.as_hal::<wgpu::hal::api::Metal>() }.ok_or(MetalFxError::NotMetal)?;
        Ok(hal.raw_handle().retain())
    }

    fn check_texture(
        name: &'static str,
        texture: &ProtocolObject<dyn MTLTexture>,
        need: MTLTextureUsage,
    ) -> Result<(), MetalFxError> {
        let have = texture.usage();
        if !have.contains(need) {
            return Err(MetalFxError::Usage {
                texture: name,
                have: have.0,
                need: need.0,
            });
        }
        let format = texture.pixelFormat();
        if format != FORMAT {
            return Err(MetalFxError::Format {
                texture: name,
                have: format.0,
                need: FORMAT.0,
            });
        }
        Ok(())
    }

    /// `[MTLFXSpatialScalerDescriptor supportsDevice:]` on wgpu's device. `false` when
    /// wgpu is not on Metal.
    pub fn supported(device: &wgpu::Device) -> bool {
        match raw_device(device) {
            // SAFETY: plain class method on a valid device.
            Ok(raw) => unsafe { MTLFXSpatialScalerDescriptor::supportsDevice(&raw) },
            Err(_) => false,
        }
    }

    /// One `MTLFXSpatialScaler` bound to a colour (input) and an output texture.
    pub struct SpatialScaler {
        scaler: Retained<ProtocolObject<dyn MTLFXSpatialScaler>>,
        color: Retained<ProtocolObject<dyn MTLTexture>>,
        output: Retained<ProtocolObject<dyn MTLTexture>>,
        input_size: (u32, u32),
        output_size: (u32, u32),
        /// `f64::to_bits` of the last completed command buffer's GPU time in ms, or NaN.
        gpu_ms_bits: Arc<AtomicU64>,
        frames: u64,
    }

    // SAFETY: Metal objects are reference counted atomically and may be used from any
    // thread as long as calls are not concurrent (Apple's Metal threading model). The
    // scaler lives inside `VideoRenderer`, which egui's callback resources move to the
    // render thread once; every call happens there. The completion handler only touches an
    // atomic.
    unsafe impl Send for SpatialScaler {}
    // SAFETY: see above; there is no shared mutation.
    unsafe impl Sync for SpatialScaler {}

    impl std::fmt::Debug for SpatialScaler {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "SpatialScaler({}x{} → {}x{})",
                self.input_size.0, self.input_size.1, self.output_size.0, self.output_size.1
            )
        }
    }

    impl SpatialScaler {
        /// Creates the scaler for `color` (source size) → `output` (target size), as
        /// `VideoNode::render` does on a size change, and binds both textures.
        pub fn new(
            device: &wgpu::Device,
            color: &wgpu::Texture,
            output: &wgpu::Texture,
            input_size: (u32, u32),
            output_size: (u32, u32),
        ) -> Result<Self, MetalFxError> {
            let raw = raw_device(device)?;
            // SAFETY: class method on a valid device.
            if !unsafe { MTLFXSpatialScalerDescriptor::supportsDevice(&raw) } {
                return Err(MetalFxError::Unsupported);
            }
            // SAFETY: plain property setters on a freshly allocated descriptor; the sizes
            // are the texture sizes the renderer just created.
            let scaler = unsafe {
                let d = MTLFXSpatialScalerDescriptor::new();
                d.setInputWidth(input_size.0 as usize);
                d.setInputHeight(input_size.1 as usize);
                d.setOutputWidth(output_size.0 as usize);
                d.setOutputHeight(output_size.1 as usize);
                d.setColorTextureFormat(FORMAT);
                d.setOutputTextureFormat(FORMAT);
                d.setColorProcessingMode(MTLFXSpatialScalerColorProcessingMode::Perceptual);
                d.newSpatialScalerWithDevice(&raw)
            }
            .ok_or(MetalFxError::CreateFailed {
                input_width: input_size.0,
                input_height: input_size.1,
                output_width: output_size.0,
                output_height: output_size.1,
            })?;

            let color = raw_texture(color)?;
            let output = raw_texture(output)?;
            // SAFETY: property getters on the scaler we just created.
            let (color_usage, output_usage) =
                unsafe { (scaler.colorTextureUsage(), scaler.outputTextureUsage()) };
            check_texture("color", &color, color_usage)?;
            check_texture("output", &output, output_usage)?;

            let this = Self {
                scaler,
                color,
                output,
                input_size,
                output_size,
                gpu_ms_bits: Arc::new(AtomicU64::new(f64::NAN.to_bits())),
                frames: 0,
            };
            this.bind();
            Ok(this)
        }

        /// `scaler.colorTexture = rgb; scaler.outputTexture = output; inputContent* = source`.
        fn bind(&self) {
            // SAFETY: setters with textures that outlive the scaler (we retain both).
            unsafe {
                self.scaler.setColorTexture(Some(&self.color));
                self.scaler.setOutputTexture(Some(&self.output));
                self.scaler.setInputContentWidth(self.input_size.0 as usize);
                self.scaler
                    .setInputContentHeight(self.input_size.1 as usize);
            }
        }

        /// Source size the scaler was created for.
        pub fn input_size(&self) -> (u32, u32) {
            self.input_size
        }

        /// Output size the scaler was created for.
        pub fn output_size(&self) -> (u32, u32) {
            self.output_size
        }

        /// Frames encoded through this scaler.
        pub fn frames(&self) -> u64 {
            self.frames
        }

        /// The required usage of the input texture (`scaler.colorTextureUsage`).
        pub fn color_texture_usage(&self) -> usize {
            // SAFETY: property getter.
            unsafe { self.scaler.colorTextureUsage() }.0
        }

        /// The required usage of the output texture (`scaler.outputTextureUsage`).
        pub fn output_texture_usage(&self) -> usize {
            // SAFETY: property getter.
            unsafe { self.scaler.outputTextureUsage() }.0
        }

        /// `[scaler encodeToCommandBuffer:]` into a fresh command buffer on wgpu's
        /// `MTLCommandQueue`, with a completion handler that records the buffer's GPU
        /// time, and commits it.
        ///
        /// Call it after the work that writes the colour texture has been **submitted**
        /// on `queue` and before the work that samples the output is; Metal then runs the
        /// three in commit order.
        pub fn encode(&mut self, queue: &wgpu::Queue) -> Result<(), MetalFxError> {
            self.bind();
            let gpu_ms_bits = Arc::clone(&self.gpu_ms_bits);
            // SAFETY: `Queue::as_hal` asks that the raw queue is not destroyed; we only
            // create and commit a command buffer on it. The guard is dropped at the end of
            // the block, before wgpu needs the queue again.
            let hal =
                unsafe { queue.as_hal::<wgpu::hal::api::Metal>() }.ok_or(MetalFxError::NotMetal)?;
            let cmd = hal
                .as_raw()
                .commandBuffer()
                .ok_or(MetalFxError::NoCommandBuffer)?;
            // SAFETY: `cmd` is a fresh, uncommitted command buffer with no encoder open,
            // as `encodeToCommandBuffer:` requires; the handler block is copied by Metal.
            unsafe {
                self.scaler.encodeToCommandBuffer(&cmd);
                let handler = RcBlock::new(
                    move |buffer: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                        let buffer = buffer.as_ref();
                        let ms = (buffer.GPUEndTime() - buffer.GPUStartTime()) * 1e3;
                        gpu_ms_bits.store(ms.to_bits(), Ordering::Relaxed);
                    },
                );
                cmd.addCompletedHandler(RcBlock::as_ptr(&handler));
            }
            cmd.commit();
            drop(hal);
            self.frames += 1;
            Ok(())
        }

        /// GPU time (ms) of the last completed command buffer that carried the scaler,
        /// `GPUEndTime − GPUStartTime`. `None` before the first completion.
        pub fn last_gpu_ms(&self) -> Option<f64> {
            let ms = f64::from_bits(self.gpu_ms_bits.load(Ordering::Relaxed));
            (!ms.is_nan()).then_some(ms)
        }
    }
}

#[cfg(not(all(target_os = "macos", feature = "metalfx")))]
mod imp {
    use super::MetalFxError;

    /// Always `false`: MetalFX is not compiled in.
    pub fn supported(_device: &wgpu::Device) -> bool {
        false
    }

    /// Placeholder; [`SpatialScaler::new`] always fails with [`MetalFxError::NotCompiled`].
    #[derive(Debug)]
    pub struct SpatialScaler {
        never: std::convert::Infallible,
    }

    impl SpatialScaler {
        /// Always `Err(NotCompiled)`.
        pub fn new(
            _device: &wgpu::Device,
            _color: &wgpu::Texture,
            _output: &wgpu::Texture,
            _input_size: (u32, u32),
            _output_size: (u32, u32),
        ) -> Result<Self, MetalFxError> {
            Err(MetalFxError::NotCompiled)
        }

        /// Unreachable: no value of this type exists.
        pub fn input_size(&self) -> (u32, u32) {
            match self.never {}
        }

        /// Unreachable: no value of this type exists.
        pub fn output_size(&self) -> (u32, u32) {
            match self.never {}
        }

        /// Unreachable: no value of this type exists.
        pub fn frames(&self) -> u64 {
            match self.never {}
        }

        /// Unreachable: no value of this type exists.
        pub fn color_texture_usage(&self) -> usize {
            match self.never {}
        }

        /// Unreachable: no value of this type exists.
        pub fn output_texture_usage(&self) -> usize {
            match self.never {}
        }

        /// Unreachable: no value of this type exists.
        pub fn encode(&mut self, _queue: &wgpu::Queue) -> Result<(), MetalFxError> {
            match self.never {}
        }

        /// Unreachable: no value of this type exists.
        pub fn last_gpu_ms(&self) -> Option<f64> {
            match self.never {}
        }
    }
}

pub use imp::{SpatialScaler, supported};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_matches_cfg() {
        assert_eq!(
            COMPILED,
            cfg!(all(target_os = "macos", feature = "metalfx"))
        );
    }

    #[test]
    fn errors_describe_themselves() {
        let e = MetalFxError::CreateFailed {
            input_width: 1920,
            input_height: 1080,
            output_width: 3840,
            output_height: 2160,
        };
        assert_eq!(
            e.to_string(),
            "MTLFXSpatialScaler creation failed for 1920x1080 → 3840x2160"
        );
        let u = MetalFxError::Usage {
            texture: "output",
            have: 0x3,
            need: 0x6,
        };
        assert!(u.to_string().contains("output texture usage 0x3"));
        assert!(MetalFxError::NotCompiled.to_string().contains("metalfx"));
    }
}
