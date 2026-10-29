//! Import of decoded frames into wgpu textures.
//!
//! Port of the `CVMetalTextureCache` handling of `qt/src/app/MetalVideoItem.mm`
//! (`VideoNode::initialize` creates the cache once, `VideoNode::render` wraps the two
//! NV12 planes of the `CVPixelBuffer` as `R8Unorm` / `RG8Unorm` Metal textures per frame)
//! onto wgpu's `Device::as_hal` / `create_texture_from_hal` escape hatch. No pixel is
//! copied: the IOSurface VideoToolbox decoded into is what the compute shaders read.
//!
//! The portable path (`NativeBuffer::Nv12Cpu`, used by the FFmpeg backend and by tests)
//! uploads the planes with `Queue::write_texture`. It is also the runtime fallback when
//! the zero-copy import fails, mirroring the Qt app's `nativeVideoEnabled = false`
//! fallback to CPU images.
//!
//! Lifetime rule (docs/archive/PORTING_PLAN.md section 4): an [`ImportedFrame`] keeps the
//! `CVMetalTextureRef`s and the `CVPixelBuffer` alive. The renderer holds it until
//! `Queue::on_submitted_work_done` fires for the submission that sampled it.

use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(target_os = "macos")]
use tracing::{info, warn};
use xcloud_media::{Frame, NativeBuffer};

use crate::{Error, Result};

/// Logging target, same as the Qt category.
pub(crate) const LOG_TARGET: &str = "MetalVideo";

/// Environment variable that forces the CPU upload path even where zero-copy would work
/// (for A/B measurements; the Qt app had no equivalent because its CPU path was the
/// `QImage` fallback).
pub const FORCE_CPU_UPLOAD_ENV: &str = "OPENXCLOUD_FORCE_CPU_UPLOAD";

/// The two NV12 planes of one frame as wgpu textures, plus whatever must stay alive while
/// the GPU may still read them.
pub struct ImportedFrame {
    /// Luma plane, `R8Unorm`.
    pub y_view: wgpu::TextureView,
    /// Interleaved chroma plane, `Rg8Unorm`, half resolution.
    pub uv_view: wgpu::TextureView,
    /// Display width in pixels.
    pub width: u32,
    /// Display height in pixels.
    pub height: u32,
    /// `true` when the textures alias the decoder's IOSurface (no copy happened).
    pub zero_copy: bool,
    // The texture handles behind the views; wgpu keeps the Metal objects alive through them.
    _y: wgpu::Texture,
    _uv: wgpu::Texture,
    #[cfg(target_os = "macos")]
    _keep_alive: Option<macos::KeepAlive>,
}

impl std::fmt::Debug for ImportedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ImportedFrame({}x{}, {})",
            self.width,
            self.height,
            if self.zero_copy {
                "zero-copy"
            } else {
                "cpu-upload"
            }
        )
    }
}

/// Turns [`Frame`]s into [`ImportedFrame`]s, zero-copy when the platform allows it.
pub struct FrameImporter {
    #[cfg(target_os = "macos")]
    metal: Option<macos::MetalImporter>,
    /// Why zero-copy is off, if it is (never tried, not Metal, or failed at runtime).
    zero_copy_disabled_reason: Option<String>,
    cpu: CpuUploader,
    imported: AtomicU64,
    zero_copy_frames: AtomicU64,
    fallbacks: AtomicU64,
}

impl FrameImporter {
    /// Creates an importer for `device`. On macOS with the Metal backend this creates the
    /// `CVMetalTextureCache`; anywhere else only the CPU upload path exists.
    pub fn new(device: &wgpu::Device) -> Self {
        #[cfg(target_os = "macos")]
        let (metal, reason) = match macos::MetalImporter::new(device) {
            Ok(_) if std::env::var_os(FORCE_CPU_UPLOAD_ENV).is_some() => {
                warn!(target: LOG_TARGET, "{FORCE_CPU_UPLOAD_ENV} set: zero-copy import disabled, using CPU upload");
                (None, Some(format!("{FORCE_CPU_UPLOAD_ENV} is set")))
            }
            Ok(importer) => {
                info!(target: LOG_TARGET, "zero-copy CVPixelBuffer → wgpu import enabled (CVMetalTextureCache on wgpu's MTLDevice)");
                (Some(importer), None)
            }
            Err(e) => {
                warn!(target: LOG_TARGET, "zero-copy import unavailable, using CPU upload: {e}");
                (None, Some(e.to_string()))
            }
        };
        #[cfg(not(target_os = "macos"))]
        let reason = {
            let _ = device;
            Some("zero-copy import is only implemented for macOS/Metal".to_string())
        };
        Self {
            #[cfg(target_os = "macos")]
            metal,
            zero_copy_disabled_reason: reason,
            cpu: CpuUploader::default(),
            imported: AtomicU64::new(0),
            zero_copy_frames: AtomicU64::new(0),
            fallbacks: AtomicU64::new(0),
        }
    }

    /// `true` while the zero-copy path is usable.
    pub fn zero_copy_available(&self) -> bool {
        self.zero_copy_disabled_reason.is_none()
    }

    /// Why zero-copy is disabled, if it is.
    pub fn zero_copy_disabled_reason(&self) -> Option<&str> {
        self.zero_copy_disabled_reason.as_deref()
    }

    /// Frames imported so far.
    pub fn frames_imported(&self) -> u64 {
        self.imported.load(Ordering::Relaxed)
    }

    /// Frames that went through the zero-copy path.
    pub fn frames_zero_copy(&self) -> u64 {
        self.zero_copy_frames.load(Ordering::Relaxed)
    }

    /// Frames that were meant to be zero-copy but had to be read back and uploaded.
    pub fn fallbacks(&self) -> u64 {
        self.fallbacks.load(Ordering::Relaxed)
    }

    /// Imports one frame. Never fails because of the zero-copy path alone: a runtime
    /// failure there disables it (like the Qt app's `nativeVideoEnabled = false`) and the
    /// frame is read back and uploaded instead.
    pub fn import(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &Frame,
    ) -> Result<ImportedFrame> {
        if frame.width == 0 || frame.height == 0 {
            return Err(Error::EmptyFrame {
                width: frame.width,
                height: frame.height,
            });
        }
        self.imported.fetch_add(1, Ordering::Relaxed);
        match &frame.buf {
            #[cfg(target_os = "macos")]
            NativeBuffer::CvPixelBuffer(buffer) => {
                if let Some(metal) = &mut self.metal {
                    match metal.import(device, buffer, frame.width, frame.height) {
                        Ok(imported) => {
                            self.zero_copy_frames.fetch_add(1, Ordering::Relaxed);
                            return Ok(imported);
                        }
                        Err(e) => {
                            warn!(target: LOG_TARGET, "zero-copy import failed, reverting to CPU upload for the rest of the session: {e}");
                            self.metal = None;
                            self.zero_copy_disabled_reason = Some(e.to_string());
                        }
                    }
                }
                self.fallbacks.fetch_add(1, Ordering::Relaxed);
                let cpu = frame.to_nv12_cpu()?;
                match &cpu {
                    NativeBuffer::Nv12Cpu {
                        y,
                        uv,
                        y_stride,
                        uv_stride,
                    } => self.cpu.upload(
                        device,
                        queue,
                        frame.width,
                        frame.height,
                        y,
                        uv,
                        *y_stride,
                        *uv_stride,
                    ),
                    NativeBuffer::CvPixelBuffer(_) => Err(Error::UnsupportedBuffer(
                        "to_nv12_cpu returned a CVPixelBuffer",
                    )),
                }
            }
            NativeBuffer::Nv12Cpu {
                y,
                uv,
                y_stride,
                uv_stride,
            } => self.cpu.upload(
                device,
                queue,
                frame.width,
                frame.height,
                y,
                uv,
                *y_stride,
                *uv_stride,
            ),
        }
    }
}

/// `Queue::write_texture` path. Reuses one pair of textures per size: writes are ordered
/// on the queue, so a frame still being sampled by the previous submission is safe.
#[derive(Default)]
struct CpuUploader {
    cached: Option<CachedPlanes>,
}

struct CachedPlanes {
    width: u32,
    height: u32,
    y: wgpu::Texture,
    uv: wgpu::Texture,
    y_view: wgpu::TextureView,
    uv_view: wgpu::TextureView,
}

impl CpuUploader {
    #[allow(clippy::too_many_arguments)]
    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        y: &[u8],
        uv: &[u8],
        y_stride: usize,
        uv_stride: usize,
    ) -> Result<ImportedFrame> {
        let uv_width = width.div_ceil(2);
        let uv_height = height.div_ceil(2);
        let needed_y = y_stride
            .checked_mul(height as usize - 1)
            .map(|n| n + width as usize);
        let needed_uv = uv_stride
            .checked_mul(uv_height as usize - 1)
            .map(|n| n + 2 * uv_width as usize);
        if y_stride < width as usize || needed_y.is_none_or(|n| y.len() < n) {
            return Err(Error::UnsupportedBuffer(
                "Y plane shorter than stride × height",
            ));
        }
        if uv_stride < 2 * uv_width as usize || needed_uv.is_none_or(|n| uv.len() < n) {
            return Err(Error::UnsupportedBuffer(
                "UV plane shorter than stride × height",
            ));
        }
        let y_stride = u32::try_from(y_stride)
            .map_err(|_| Error::UnsupportedBuffer("Y stride does not fit in u32"))?;
        let uv_stride = u32::try_from(uv_stride)
            .map_err(|_| Error::UnsupportedBuffer("UV stride does not fit in u32"))?;

        let reusable = matches!(&self.cached, Some(p) if p.width == width && p.height == height);
        if !reusable {
            let make = |label, w, h, format| {
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: w,
                        height: h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                (texture, view)
            };
            let (y, y_view) = make("video.y.cpu", width, height, wgpu::TextureFormat::R8Unorm);
            let (uv, uv_view) = make(
                "video.uv.cpu",
                uv_width,
                uv_height,
                wgpu::TextureFormat::Rg8Unorm,
            );
            self.cached = Some(CachedPlanes {
                width,
                height,
                y,
                uv,
                y_view,
                uv_view,
            });
        }
        let planes = self
            .cached
            .as_ref()
            .ok_or(Error::UnsupportedBuffer("plane textures missing"))?;

        queue.write_texture(
            planes.y.as_image_copy(),
            y,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(y_stride),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        queue.write_texture(
            planes.uv.as_image_copy(),
            uv,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(uv_stride),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: uv_width,
                height: uv_height,
                depth_or_array_layers: 1,
            },
        );

        Ok(ImportedFrame {
            y_view: planes.y_view.clone(),
            uv_view: planes.uv_view.clone(),
            width,
            height,
            zero_copy: false,
            _y: planes.y.clone(),
            _uv: planes.uv.clone(),
            #[cfg(target_os = "macos")]
            _keep_alive: None,
        })
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ptr::{self, NonNull};

    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_core_foundation::{CFRetained, Type as _};
    use objc2_core_video::{
        CVMetalTexture, CVMetalTextureCache, CVMetalTextureGetTexture, CVPixelBuffer,
        CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount,
        CVPixelBufferGetWidthOfPlane, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    };
    use objc2_metal::{MTLDevice, MTLPixelFormat, MTLTextureType};

    use super::ImportedFrame;
    use crate::{Error, Result};

    /// What must outlive the GPU's use of the planes: the texture-cache entries and the
    /// pixel buffer they alias (`CVMetalTexture.h`: "Clients should retain CVMetalTexture
    /// objects until they are done using the images in them").
    pub(super) struct KeepAlive {
        _y: CFRetained<CVMetalTexture>,
        _uv: CFRetained<CVMetalTexture>,
        _buffer: CFRetained<CVPixelBuffer>,
    }

    // SAFETY: Core Foundation objects are reference counted atomically and the Qt app
    // already moves CVMetalTextureRefs between its GUI and render threads. We never call
    // into these objects; they are only released, possibly from wgpu's callback thread.
    unsafe impl Send for KeepAlive {}
    // SAFETY: see above; there is no shared access, only a drop.
    unsafe impl Sync for KeepAlive {}

    pub(super) struct MetalImporter {
        cache: CFRetained<CVMetalTextureCache>,
        frames: u64,
    }

    // SAFETY: `CVMetalTextureCache` is documented as thread-safe; the importer lives on the
    // render thread anyway and only moves there once (eframe creates it on the main thread).
    unsafe impl Send for MetalImporter {}
    // SAFETY: see above.
    unsafe impl Sync for MetalImporter {}

    impl MetalImporter {
        pub(super) fn new(device: &wgpu::Device) -> Result<Self> {
            // Borrow wgpu's MTLDevice. The hal guard must not outlive this block: wgpu
            // holds internal locks while it is alive.
            let raw_device: Retained<ProtocolObject<dyn MTLDevice>> = {
                // SAFETY: `as_hal` only requires that we do not use the hal device in ways
                // that break wgpu's own state tracking; we read the raw device handle.
                let hal = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }.ok_or(
                    Error::ZeroCopyUnavailable("wgpu is not running on the Metal backend"),
                )?;
                hal.raw_device().clone()
            };
            let mut out: *mut CVMetalTextureCache = ptr::null_mut();
            // SAFETY: all pointers are valid for the call; `out` receives a +1 reference.
            let status = unsafe {
                CVMetalTextureCache::create(None, None, &raw_device, None, NonNull::from(&mut out))
            };
            if status != 0 {
                return Err(Error::CoreVideo {
                    what: "CVMetalTextureCacheCreate",
                    status,
                });
            }
            let cache = NonNull::new(out)
                // SAFETY: CoreVideo returned an owned (+1) reference we are responsible for.
                .map(|p| unsafe { CFRetained::from_raw(p) })
                .ok_or(Error::ZeroCopyUnavailable(
                    "CVMetalTextureCacheCreate returned success but no cache",
                ))?;
            Ok(Self { cache, frames: 0 })
        }

        pub(super) fn import(
            &mut self,
            device: &wgpu::Device,
            buffer: &CVPixelBuffer,
            width: u32,
            height: u32,
        ) -> Result<ImportedFrame> {
            let format = CVPixelBufferGetPixelFormatType(buffer);
            if format != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
                && format != kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
            {
                return Err(Error::UnsupportedBuffer(
                    "CVPixelBuffer is not 420YpCbCr8BiPlanar (NV12)",
                ));
            }
            if CVPixelBufferGetPlaneCount(buffer) < 2 {
                return Err(Error::UnsupportedBuffer(
                    "CVPixelBuffer has fewer than 2 planes",
                ));
            }
            let (y, y_ref) = self.wrap_plane(
                device,
                buffer,
                0,
                MTLPixelFormat::R8Unorm,
                wgpu::TextureFormat::R8Unorm,
                "video.y.iosurface",
            )?;
            let (uv, uv_ref) = self.wrap_plane(
                device,
                buffer,
                1,
                MTLPixelFormat::RG8Unorm,
                wgpu::TextureFormat::Rg8Unorm,
                "video.uv.iosurface",
            )?;
            // CVMetalTextureCache.h asks for periodic housekeeping so released entries are
            // recycled; once a second at 60 fps is plenty.
            self.frames = self.frames.wrapping_add(1);
            if self.frames.is_multiple_of(60) {
                self.cache.flush(0);
            }
            let y_view = y.create_view(&wgpu::TextureViewDescriptor::default());
            let uv_view = uv.create_view(&wgpu::TextureViewDescriptor::default());
            Ok(ImportedFrame {
                y_view,
                uv_view,
                width,
                height,
                zero_copy: true,
                _y: y,
                _uv: uv,
                _keep_alive: Some(KeepAlive {
                    _y: y_ref,
                    _uv: uv_ref,
                    _buffer: buffer.retain(),
                }),
            })
        }

        fn wrap_plane(
            &self,
            device: &wgpu::Device,
            buffer: &CVPixelBuffer,
            plane: usize,
            mtl_format: MTLPixelFormat,
            wgpu_format: wgpu::TextureFormat,
            label: &'static str,
        ) -> Result<(wgpu::Texture, CFRetained<CVMetalTexture>)> {
            let width = CVPixelBufferGetWidthOfPlane(buffer, plane);
            let height = CVPixelBufferGetHeightOfPlane(buffer, plane);
            if width == 0 || height == 0 {
                return Err(Error::UnsupportedBuffer(
                    "CVPixelBuffer plane has zero size",
                ));
            }
            let mut out: *mut CVMetalTexture = ptr::null_mut();
            // SAFETY: the cache and buffer are live retained objects, `out` is a valid
            // pointer that receives a +1 reference.
            let status = unsafe {
                CVMetalTextureCache::create_texture_from_image(
                    None,
                    &self.cache,
                    buffer,
                    None,
                    mtl_format,
                    width,
                    height,
                    plane,
                    NonNull::from(&mut out),
                )
            };
            if status != 0 {
                return Err(Error::CoreVideo {
                    what: "CVMetalTextureCacheCreateTextureFromImage",
                    status,
                });
            }
            let cv_texture = NonNull::new(out)
                // SAFETY: owned (+1) reference from CoreVideo.
                .map(|p| unsafe { CFRetained::from_raw(p) })
                .ok_or(Error::ZeroCopyUnavailable(
                    "CVMetalTextureCacheCreateTextureFromImage returned no texture",
                ))?;
            let mtl_texture = CVMetalTextureGetTexture(&cv_texture).ok_or(
                Error::ZeroCopyUnavailable("CVMetalTextureGetTexture returned nil"),
            )?;
            let width = u32::try_from(width)
                .map_err(|_| Error::UnsupportedBuffer("plane width does not fit in u32"))?;
            let height = u32::try_from(height)
                .map_err(|_| Error::UnsupportedBuffer("plane height does not fit in u32"))?;
            // SAFETY: the descriptor describes exactly the Metal texture CoreVideo made:
            // 2D, one mip, one layer, the given format and size. wgpu takes ownership of the
            // `Retained<MTLTexture>`; the CVMetalTexture/CVPixelBuffer stay alive through
            // `KeepAlive` until the renderer retires the frame.
            let hal_texture = unsafe {
                wgpu::hal::metal::Device::texture_from_raw(
                    mtl_texture,
                    wgpu_format,
                    MTLTextureType::Type2D,
                    1,
                    1,
                    wgpu::hal::CopyExtent {
                        width,
                        height,
                        depth: 1,
                    },
                    None,
                )
            };
            let descriptor = wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu_format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            };
            // SAFETY: `hal_texture` belongs to this device's backend and matches `descriptor`.
            let texture = unsafe {
                device.create_texture_from_hal::<wgpu::hal::api::Metal>(
                    hal_texture,
                    &descriptor,
                    wgpu::TextureUses::RESOURCE,
                )
            };
            Ok((texture, cv_texture))
        }
    }
}
