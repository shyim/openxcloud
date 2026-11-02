//! Driver video super resolution (NVIDIA RTX Video Super Resolution, Intel VSR) through the
//! D3D11 video processor, rendering into a texture wgpu's DX12 device samples directly.
//!
//! Neither vendor exposes VSR as a D3D12 or Vulkan API without its SDK. Both drivers do
//! hook it into `ID3D11VideoContext::VideoProcessorBlt` when a stream extension is set,
//! which is what Chromium and mpv (`vf_d3d11vpp.c`) use:
//!
//! * NVIDIA: `VideoProcessorSetStreamExtension(NVIDIA_PPE_INTERFACE_GUID,
//!   {version: 1, method: 2, enable: 1})`. The NVIDIA app / control panel's "RTX Video
//!   → Super resolution" toggle and quality level still apply; the call succeeds with the
//!   toggle off, the driver then scales like a plain video processor.
//! * Intel: three `INTEL_VPE_INTERFACE_GUID` calls (version, pre-processing mode, VSR
//!   scaling), same order and values as mpv.
//!
//! Driver check: no API reports whether the driver will actually apply super resolution
//! (the toggle above, the quality level, power policy, size limits). Plain video
//! processor scaling is worse than FSR, so [`VideoScaler::new`] blits a detailed test
//! pattern once without and then with the extension and compares a region of the output;
//! identical pixels mean the driver ignored the request and creation fails with
//! [`VsrError::DriverInactive`], which makes the renderer fall back to FSR.
//! `OPENXCLOUD_VSR_SKIP_CHECK` skips the check.
//!
//! Per frame ([`VideoScaler::process`]):
//!
//! 1. The CPU NV12 planes are written into one of two `STAGING` NV12 textures and copied
//!    into the `DEFAULT` texture the video processor reads. The blit converts NV12 to RGB
//!    and upscales in one go, so the WGSL convert pass and FSR do not run.
//! 2. The output is a `R8G8B8A8_UNORM` D3D12 resource created on **wgpu's** device with
//!    `HEAP_FLAG_SHARED` and `ALLOW_SIMULTANEOUS_ACCESS` (so it decays to `COMMON` after
//!    every D3D12 submit and needs no cross-API barriers), opened on the D3D11 device
//!    through an NT handle, and wrapped for wgpu with `texture_from_raw` /
//!    `create_texture_from_hal`.
//! 3. Two shared fences order the queues, one per direction, so each has a single writer
//!    and only ever grows. `blit`: D3D11 signals it after the blit and wgpu's queue waits
//!    for that value before its next submit (`dx12::Queue::add_wait_fence`). `read`:
//!    [`VideoScaler::after_submit`] signals it on wgpu's raw queue right behind the
//!    submit that sampled the output, and the next blit waits for it, so D3D11 never
//!    overwrites the output while egui samples it. D3D11 only waits for `read` values
//!    already enqueued on the D3D12 queue, so no ordering of `process` / submit /
//!    `after_submit` (two frames without a submit, a skipped paint) can form a cycle.
//!
//! No GPU time is reported: the drivers run super resolution outside the D3D11 timeline,
//! so timestamps around the blit miss it. The renderer's present latency (decoded →
//! the submit that waited for the blit finished) includes it.
//!
//! Without the `vsr` cargo feature, or off Windows, this module compiles to stubs that
//! report [`VsrError::NotCompiled`], so `renderer.rs` has no `cfg` of its own.

/// `true` when video super resolution support is compiled into this build.
pub const COMPILED: bool = cfg!(all(windows, feature = "vsr"));

/// Why the VSR path could not be created or run; lives in [`crate::error`] and wraps into
/// [`crate::Error::Vsr`]. The renderer logs these and falls back to FSR.
pub use crate::error::VsrError;

/// Environment variable that skips the driver check in [`VideoScaler::new`].
pub const SKIP_CHECK_ENV: &str = "OPENXCLOUD_VSR_SKIP_CHECK";

/// PCI vendor id of NVIDIA adapters.
pub const VENDOR_NVIDIA: u32 = 0x10de;
/// PCI vendor id of Intel adapters.
pub const VENDOR_INTEL: u32 = 0x8086;

#[cfg(all(windows, feature = "vsr"))]
mod imp {
    use std::ffi::c_void;
    use std::mem::ManuallyDrop;

    use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE, LUID, RECT};
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    };
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};
    use windows::core::{GUID, HRESULT, Interface, PCWSTR};
    use xcloud_media::{ColorInfo, Frame, YCbCrMatrix};

    use super::{VENDOR_INTEL, VENDOR_NVIDIA, VsrError};

    /// `NVIDIA_PPE_INTERFACE_GUID` (Chromium `swap_chain_presenter.cc`, mpv).
    const NVIDIA_PPE_INTERFACE_GUID: GUID = GUID::from_u128(0xd43ce1b3_1f4b_48ac_baee_c3c25375e6f7);
    /// `INTEL_VPE_INTERFACE_GUID` (same sources).
    const INTEL_VPE_INTERFACE_GUID: GUID = GUID::from_u128(0xedd1d4b9_8659_4cbc_a4d6_9831a2163ac3);

    const INTEL_VPE_FN_VERSION: u32 = 0x1;
    const INTEL_VPE_VERSION: u32 = 0x3;
    const INTEL_VPE_FN_MODE: u32 = 0x20;
    const INTEL_VPE_MODE_PREPROC: u32 = 0x1;
    const INTEL_VPE_FN_SCALING: u32 = 0x37;
    const INTEL_VPE_SCALING_VSR: u32 = 0x2;

    #[repr(C)]
    struct NvidiaExt {
        version: u32,
        method: u32,
        enable: u32,
    }

    #[repr(C)]
    struct IntelExt {
        function: u32,
        param: *const c_void,
    }

    /// Side of the centred output region the driver check reads back.
    const CHECK_REGION: u32 = 256;
    /// Blits with the extension on before the check gives up (a driver may need a frame
    /// or two to switch the scaler in).
    const CHECK_ATTEMPTS: usize = 4;
    /// Fraction of channel samples that must differ by more than [`CHECK_DELTA`] for the
    /// driver to count as upscaling. Plain scaling is bit-identical run to run; RTX VSR
    /// changes most edge pixels of the pattern.
    const CHECK_FRACTION: f64 = 0.005;
    const CHECK_DELTA: u8 = 8;

    fn os(what: &'static str) -> impl FnOnce(windows::core::Error) -> VsrError {
        move |e| VsrError::Os {
            what,
            hresult: e.code().0 as u32,
        }
    }

    /// wgpu's `ID3D12Device`. The hal guard is dropped before returning.
    fn raw_device(device: &wgpu::Device) -> Result<ID3D12Device, VsrError> {
        // SAFETY: we clone the COM pointer and do not touch wgpu's state through it.
        let hal = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }.ok_or(VsrError::NotDx12)?;
        Ok(hal.raw_device().clone())
    }

    /// The DXGI adapter wgpu's device runs on, and its PCI vendor id.
    fn adapter_for(device12: &ID3D12Device) -> Result<(IDXGIAdapter1, u32), VsrError> {
        // SAFETY: plain COM calls on valid objects.
        unsafe {
            let luid: LUID = device12.GetAdapterLuid();
            let factory: IDXGIFactory4 = CreateDXGIFactory1().map_err(os("CreateDXGIFactory1"))?;
            let adapter: IDXGIAdapter1 = factory
                .EnumAdapterByLuid(luid)
                .map_err(os("EnumAdapterByLuid"))?;
            let desc = adapter.GetDesc1().map_err(os("IDXGIAdapter1::GetDesc1"))?;
            Ok((adapter, desc.VendorId))
        }
    }

    /// A D3D11 device with video support on the same adapter as wgpu.
    fn create_d3d11(
        adapter: &IDXGIAdapter1,
    ) -> Result<(ID3D11Device5, ID3D11DeviceContext4), VsrError> {
        let mut device = None;
        let mut context = None;
        // SAFETY: out-pointers are valid for the call; the adapter outlives it.
        unsafe {
            D3D11CreateDevice(
                adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                Default::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(os("D3D11CreateDevice"))?;
        }
        let device = device.ok_or(VsrError::Os {
            what: "D3D11CreateDevice",
            hresult: 0,
        })?;
        let context = context.ok_or(VsrError::Os {
            what: "D3D11CreateDevice",
            hresult: 0,
        })?;
        Ok((
            device.cast().map_err(os("ID3D11Device5"))?,
            context.cast().map_err(os("ID3D11DeviceContext4"))?,
        ))
    }

    /// `true` when wgpu runs on DX12 on an NVIDIA or Intel adapter and a D3D11 video
    /// device can be created there. Does not prove the driver actually upscales with AI:
    /// that is the user's driver setting.
    pub fn supported(device: &wgpu::Device) -> bool {
        let Ok(device12) = raw_device(device) else {
            return false;
        };
        match adapter_for(&device12) {
            Ok((adapter, vendor)) if vendor == VENDOR_NVIDIA || vendor == VENDOR_INTEL => {
                create_d3d11(&adapter)
                    .and_then(|(d, _)| {
                        d.cast::<ID3D11VideoDevice>()
                            .map_err(os("ID3D11VideoDevice"))
                    })
                    .is_ok()
            }
            _ => false,
        }
    }

    fn color_space(color: &ColorInfo) -> DXGI_COLOR_SPACE_TYPE {
        match (color.matrix, color.full_range) {
            (YCbCrMatrix::Bt709, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            (YCbCrMatrix::Bt709, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
            (YCbCrMatrix::Bt601, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
            (YCbCrMatrix::Bt601, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601,
            (YCbCrMatrix::Bt2020, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P2020,
            (YCbCrMatrix::Bt2020, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P2020,
        }
    }

    fn nv12_texture(
        device: &ID3D11Device5,
        width: u32,
        height: u32,
        staging: bool,
    ) -> Result<ID3D11Texture2D, VsrError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: if staging {
                D3D11_USAGE_STAGING
            } else {
                D3D11_USAGE_DEFAULT
            },
            // The NVIDIA driver rejects `CreateVideoProcessorInputView` (E_INVALIDARG) on an
            // NV12 texture bound only as a shader resource; render target is accepted.
            BindFlags: if staging {
                0
            } else {
                D3D11_BIND_RENDER_TARGET.0 as u32
            },
            CPUAccessFlags: if staging {
                D3D11_CPU_ACCESS_WRITE.0 as u32
            } else {
                0
            },
            MiscFlags: 0,
        };
        let mut texture = None;
        // SAFETY: valid descriptor, no initial data.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
            .map_err(os("CreateTexture2D(NV12)"))?;
        texture.ok_or(VsrError::Os {
            what: "CreateTexture2D(NV12)",
            hresult: 0,
        })
    }

    /// The upscaled output: a D3D12 resource on wgpu's device, shared with D3D11.
    fn shared_output(
        device12: &ID3D12Device,
        width: u32,
        height: u32,
    ) -> Result<(ID3D12Resource, HANDLE), VsrError> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: u64::from(width),
            Height: height,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET
                | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
                | D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
        };
        let mut resource: Option<ID3D12Resource> = None;
        // SAFETY: valid descriptors; the handle is closed by the caller.
        unsafe {
            device12
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_SHARED,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut resource,
                )
                .map_err(os("CreateCommittedResource(shared output)"))?;
            let resource = resource.ok_or(VsrError::Os {
                what: "CreateCommittedResource(shared output)",
                hresult: 0,
            })?;
            let handle = device12
                .CreateSharedHandle(&resource, None, GENERIC_ALL.0, PCWSTR::null())
                .map_err(os("CreateSharedHandle(output)"))?;
            Ok((resource, handle))
        }
    }

    /// A fence created shared on wgpu's device and opened on ours.
    fn shared_fence(
        device12: &ID3D12Device,
        device11: &ID3D11Device5,
    ) -> Result<(ID3D12Fence, ID3D11Fence), VsrError> {
        // SAFETY: plain COM calls; the handle is closed after D3D11 opened its reference.
        unsafe {
            let fence12: ID3D12Fence = device12
                .CreateFence(0, D3D12_FENCE_FLAG_SHARED)
                .map_err(os("CreateFence(shared)"))?;
            let handle = device12
                .CreateSharedHandle(&fence12, None, GENERIC_ALL.0, PCWSTR::null())
                .map_err(os("CreateSharedHandle(fence)"))?;
            let mut fence11: Option<ID3D11Fence> = None;
            let opened = device11
                .OpenSharedFence(handle, &mut fence11)
                .map_err(os("OpenSharedFence"));
            let _ = CloseHandle(handle);
            opened?;
            let fence11 = fence11.ok_or(VsrError::Os {
                what: "OpenSharedFence",
                hresult: 0,
            })?;
            Ok((fence12, fence11))
        }
    }

    /// NV12 test pattern for the driver check: a 2-pixel checkerboard with diagonal
    /// stripes in luma and coloured 8-pixel blocks in chroma, i.e. the hard edges
    /// super resolution reshapes and plain scaling only interpolates.
    fn check_pattern(w: usize, h: usize) -> (Vec<u8>, Vec<u8>) {
        let mut y = vec![0u8; w * h];
        for (row, line) in y.chunks_exact_mut(w).enumerate() {
            for (col, px) in line.iter_mut().enumerate() {
                let checker = ((row / 2) + (col / 2)) % 2 == 0;
                let stripe = (row + col) % 11 < 2;
                *px = match (checker, stripe) {
                    (_, true) => 235,
                    (true, false) => 180,
                    (false, false) => 40,
                };
            }
        }
        let uv_w = w.next_multiple_of(2);
        let mut uv = vec![128u8; uv_w * h.div_ceil(2)];
        for (row, line) in uv.chunks_exact_mut(uv_w).enumerate() {
            for (pair, cbcr) in line.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                if ((row / 4) + (pair / 4)) % 2 == 0 {
                    *cbcr = [90, 170];
                }
            }
        }
        (y, uv)
    }

    /// One D3D11 video processor bound to an NV12 input (source size) and the shared
    /// RGBA output (target size).
    pub struct VideoScaler {
        context: ID3D11DeviceContext4,
        video_context: ID3D11VideoContext1,
        processor: ID3D11VideoProcessor,
        staging: [ID3D11Texture2D; 2],
        staging_index: usize,
        input: ID3D11Texture2D,
        input_view: ID3D11VideoProcessorInputView,
        output_view: ID3D11VideoProcessorOutputView,
        /// D3D11 → D3D12: "blit `n` finished".
        blit11: ID3D11Fence,
        blit12: ID3D12Fence,
        blit_value: u64,
        /// D3D12 → D3D11: "the submit that sampled blit `n` finished".
        read11: ID3D11Fence,
        read12: ID3D12Fence,
        /// Last `read` value enqueued on wgpu's queue.
        read_value: u64,
        /// A blit whose output has not been through a wgpu submit yet.
        awaiting_submit: bool,
        color: Option<ColorInfo>,
        vendor: u32,
        input_size: (u32, u32),
        output_size: (u32, u32),
        frames: u64,
    }

    /// How long dropping a scaler waits for its last blit.
    const DROP_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

    impl Drop for VideoScaler {
        /// Waits (bounded) for the last blit, so the driver has released this processor's
        /// super resolution slot before a replacement is created and checked. wgpu keeps
        /// its own references to the output and to the fences it still has to wait on.
        fn drop(&mut self) {
            // SAFETY: plain D3D11 calls on live objects from the render thread.
            unsafe { self.context.Flush() };
            let deadline = std::time::Instant::now() + DROP_WAIT;
            // SAFETY: as above.
            while unsafe { self.blit11.GetCompletedValue() } < self.blit_value
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }

    // SAFETY: the D3D11 immediate context is not thread-safe, but the scaler lives inside
    // `VideoRenderer`, which egui's callback resources move to the render thread once;
    // every call happens there. COM reference counting is atomic.
    unsafe impl Send for VideoScaler {}
    // SAFETY: see above; no `&self` method touches the context.
    unsafe impl Sync for VideoScaler {}

    impl std::fmt::Debug for VideoScaler {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "VideoScaler(vendor {:#06x}, {}x{} → {}x{})",
                self.vendor,
                self.input_size.0,
                self.input_size.1,
                self.output_size.0,
                self.output_size.1
            )
        }
    }

    impl VideoScaler {
        /// Creates the D3D11 device, video processor and textures for `input_size` →
        /// `output_size`, and returns the scaler with the output as a wgpu texture
        /// (`Rgba8Unorm`, usable as storage, sampled, copy source and render target).
        pub fn new(
            device: &wgpu::Device,
            input_size: (u32, u32),
            output_size: (u32, u32),
        ) -> Result<(Self, wgpu::Texture), VsrError> {
            let device12 = raw_device(device)?;
            let (adapter, vendor) = adapter_for(&device12)?;
            if vendor != VENDOR_NVIDIA && vendor != VENDOR_INTEL {
                return Err(VsrError::UnsupportedVendor(vendor));
            }
            let (device11, context) = create_d3d11(&adapter)?;
            let video_device: ID3D11VideoDevice =
                device11.cast().map_err(os("ID3D11VideoDevice"))?;
            let video_context: ID3D11VideoContext1 =
                context.cast().map_err(os("ID3D11VideoContext1"))?;

            // NV12 textures need even dimensions; the source rect crops the padding.
            let (in_w, in_h) = (
                input_size.0.next_multiple_of(2),
                input_size.1.next_multiple_of(2),
            );
            let (out_w, out_h) = output_size;

            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                InputWidth: in_w,
                InputHeight: in_h,
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                OutputWidth: out_w,
                OutputHeight: out_h,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            // SAFETY: plain COM calls with valid descriptors on the objects created above.
            let (enumerator, processor) = unsafe {
                let enumerator = video_device
                    .CreateVideoProcessorEnumerator(&content)
                    .map_err(os("CreateVideoProcessorEnumerator"))?;
                let input_support = enumerator
                    .CheckVideoProcessorFormat(DXGI_FORMAT_NV12)
                    .map_err(os("CheckVideoProcessorFormat(NV12)"))?;
                if input_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0 {
                    return Err(VsrError::Format("NV12 input"));
                }
                let output_support = enumerator
                    .CheckVideoProcessorFormat(DXGI_FORMAT_R8G8B8A8_UNORM)
                    .map_err(os("CheckVideoProcessorFormat(RGBA8)"))?;
                if output_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0 {
                    return Err(VsrError::Format("R8G8B8A8_UNORM output"));
                }
                let processor = video_device
                    .CreateVideoProcessor(&enumerator, 0)
                    .map_err(os("CreateVideoProcessor"))?;
                (enumerator, processor)
            };

            let staging = [
                nv12_texture(&device11, in_w, in_h, true)?,
                nv12_texture(&device11, in_w, in_h, true)?,
            ];
            let input = nv12_texture(&device11, in_w, in_h, false)?;

            let (resource, handle) = shared_output(&device12, out_w, out_h)?;
            // SAFETY: the handle came from `CreateSharedHandle` just now and is closed
            // right after D3D11 has opened its own reference.
            let output11: Result<ID3D11Texture2D, VsrError> =
                unsafe { device11.OpenSharedResource1(handle) }
                    .map_err(os("OpenSharedResource1(output)"));
            // SAFETY: we own the handle.
            unsafe {
                let _ = CloseHandle(handle);
            }
            let output11 = output11?;

            // SAFETY: plain COM calls; the out-pointers are valid for each call.
            let (input_view, output_view) = unsafe {
                let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV {
                            MipSlice: 0,
                            ArraySlice: 0,
                        },
                    },
                };
                let mut input_view = None;
                video_device
                    .CreateVideoProcessorInputView(
                        &input,
                        &enumerator,
                        &in_desc,
                        Some(&mut input_view),
                    )
                    .map_err(os("CreateVideoProcessorInputView"))?;
                let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                };
                let mut output_view = None;
                video_device
                    .CreateVideoProcessorOutputView(
                        &output11,
                        &enumerator,
                        &out_desc,
                        Some(&mut output_view),
                    )
                    .map_err(os("CreateVideoProcessorOutputView"))?;
                (
                    input_view.ok_or(VsrError::Os {
                        what: "CreateVideoProcessorInputView",
                        hresult: 0,
                    })?,
                    output_view.ok_or(VsrError::Os {
                        what: "CreateVideoProcessorOutputView",
                        hresult: 0,
                    })?,
                )
            };

            let (blit12, blit11) = shared_fence(&device12, &device11)?;
            let (read12, read11) = shared_fence(&device12, &device11)?;

            let mut this = Self {
                context,
                video_context,
                processor,
                staging,
                staging_index: 0,
                input,
                input_view,
                output_view,
                blit11,
                blit12,
                blit_value: 0,
                read11,
                read12,
                read_value: 0,
                awaiting_submit: false,
                color: None,
                vendor,
                input_size,
                output_size,
                frames: 0,
            };
            this.configure();
            if std::env::var_os(super::SKIP_CHECK_ENV).is_some() {
                this.enable_extension()?;
            } else {
                this.check_driver_upscales(&device11, &output11)?;
            }

            let size = wgpu::Extent3d {
                width: out_w,
                height: out_h,
                depth_or_array_layers: 1,
            };
            // SAFETY: `resource` is a live `R8G8B8A8_UNORM` 2D texture of `size` on wgpu's
            // device with one mip and one sample, in `COMMON` (wgpu's `UNINITIALIZED`), with
            // the RTV/UAV/SRV capabilities the usages below need.
            let texture = unsafe {
                let hal = wgpu::hal::dx12::Device::texture_from_raw(
                    resource,
                    wgpu::TextureFormat::Rgba8Unorm,
                    wgpu::TextureDimension::D2,
                    size,
                    1,
                    1,
                );
                device.create_texture_from_hal::<wgpu::hal::api::Dx12>(
                    hal,
                    &wgpu::TextureDescriptor {
                        label: Some("video.output.vsr"),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        usage: wgpu::TextureUsages::STORAGE_BINDING
                            | wgpu::TextureUsages::TEXTURE_BINDING
                            | wgpu::TextureUsages::COPY_SRC
                            | wgpu::TextureUsages::RENDER_ATTACHMENT,
                        view_formats: &[],
                    },
                    wgpu::TextureUses::UNINITIALIZED,
                )
            };
            Ok((this, texture))
        }

        /// Rects, frame format and colour spaces (everything but the vendor extension).
        fn configure(&self) {
            let (w, h) = self.input_size;
            let (ow, oh) = self.output_size;
            let src = RECT {
                left: 0,
                top: 0,
                right: w as i32,
                bottom: h as i32,
            };
            let dst = RECT {
                left: 0,
                top: 0,
                right: ow as i32,
                bottom: oh as i32,
            };
            let vc = &self.video_context;
            let vp = &self.processor;
            // SAFETY: setters on a live processor with valid rects.
            unsafe {
                vc.VideoProcessorSetStreamFrameFormat(vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
                vc.VideoProcessorSetStreamSourceRect(vp, 0, true, Some(&src));
                vc.VideoProcessorSetStreamDestRect(vp, 0, true, Some(&dst));
                vc.VideoProcessorSetOutputTargetRect(vp, true, Some(&dst));
                // mpv: "supposed to stop drivers from [degrading] the video quality".
                vc.VideoProcessorSetStreamAutoProcessingMode(vp, 0, false);
                vc.VideoProcessorSetOutputColorSpace1(vp, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                vc.VideoProcessorSetStreamColorSpace1(vp, 0, color_space(&ColorInfo::default()));
            }
        }

        /// Turns the vendor's super resolution extension on for stream 0.
        fn enable_extension(&self) -> Result<(), VsrError> {
            let vc = &self.video_context;
            let vp = &self.processor;
            // SAFETY: setters on a live processor; extension buffers outlive each call.
            unsafe {
                if self.vendor == VENDOR_NVIDIA {
                    let ext = NvidiaExt {
                        version: 1,
                        method: 2,
                        enable: 1,
                    };
                    HRESULT(vc.VideoProcessorSetStreamExtension(
                        vp,
                        0,
                        &NVIDIA_PPE_INTERFACE_GUID,
                        size_of::<NvidiaExt>() as u32,
                        std::ptr::from_ref(&ext).cast(),
                    ))
                    .ok()
                    .map_err(os("VideoProcessorSetStreamExtension(NVIDIA VSR)"))?;
                } else {
                    let set_output = |function, param: &u32| {
                        let ext = IntelExt {
                            function,
                            param: std::ptr::from_ref(param).cast(),
                        };
                        HRESULT(vc.VideoProcessorSetOutputExtension(
                            vp,
                            &INTEL_VPE_INTERFACE_GUID,
                            size_of::<IntelExt>() as u32,
                            std::ptr::from_ref(&ext).cast(),
                        ))
                        .ok()
                    };
                    set_output(INTEL_VPE_FN_VERSION, &INTEL_VPE_VERSION)
                        .map_err(os("VideoProcessorSetOutputExtension(Intel version)"))?;
                    set_output(INTEL_VPE_FN_MODE, &INTEL_VPE_MODE_PREPROC)
                        .map_err(os("VideoProcessorSetOutputExtension(Intel mode)"))?;
                    let ext = IntelExt {
                        function: INTEL_VPE_FN_SCALING,
                        param: std::ptr::from_ref(&INTEL_VPE_SCALING_VSR).cast(),
                    };
                    HRESULT(vc.VideoProcessorSetStreamExtension(
                        vp,
                        0,
                        &INTEL_VPE_INTERFACE_GUID,
                        size_of::<IntelExt>() as u32,
                        std::ptr::from_ref(&ext).cast(),
                    ))
                    .ok()
                    .map_err(os("VideoProcessorSetStreamExtension(Intel VSR)"))?;
                }
            }
            Ok(())
        }

        /// Blits a detailed pattern without and then with the extension and fails with
        /// [`VsrError::DriverInactive`] when the driver leaves the output unchanged. Runs
        /// before wgpu has seen the output, so no fence is involved; the readback map
        /// waits for the D3D11 work.
        fn check_driver_upscales(
            &mut self,
            device11: &ID3D11Device5,
            output11: &ID3D11Texture2D,
        ) -> Result<(), VsrError> {
            let (w, h) = (self.input_size.0 as usize, self.input_size.1 as usize);
            let (y, uv) = check_pattern(w, h);
            let uv_stride = w.next_multiple_of(2);
            self.upload(&y, &uv, w, uv_stride, w, h)?;
            self.blit()?;
            let plain = self.read_region(device11, output11)?;
            self.enable_extension()?;
            let mut best = 0.0;
            for attempt in 1..=CHECK_ATTEMPTS {
                self.blit()?;
                let upscaled = self.read_region(device11, output11)?;
                let changed = plain
                    .iter()
                    .zip(&upscaled)
                    .filter(|(a, b)| a.abs_diff(**b) > CHECK_DELTA)
                    .count() as f64
                    / plain.len().max(1) as f64;
                best = f64::max(best, changed);
                if changed > CHECK_FRACTION {
                    tracing::debug!(
                        target: crate::import::LOG_TARGET,
                        "video super resolution active after {attempt} blit(s): {:.1}% of samples changed",
                        changed * 100.0
                    );
                    return Ok(());
                }
            }
            Err(VsrError::DriverInactive {
                vendor: self.vendor_name(),
                changed_permille: (best * 1000.0) as u32,
            })
        }

        /// Copies the centred [`CHECK_REGION`]² of the output to the CPU (RGBA bytes).
        fn read_region(
            &self,
            device11: &ID3D11Device5,
            output11: &ID3D11Texture2D,
        ) -> Result<Vec<u8>, VsrError> {
            let (ow, oh) = self.output_size;
            let (rw, rh) = (CHECK_REGION.min(ow), CHECK_REGION.min(oh));
            let (left, top) = ((ow - rw) / 2, (oh - rh) / 2);
            let desc = D3D11_TEXTURE2D_DESC {
                Width: rw,
                Height: rh,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let ctx = &self.context;
            // SAFETY: valid descriptors; the box lies inside the output; the mapped pointer
            // is valid for `RowPitch * rh` bytes and each row read is `rw * 4` bytes.
            unsafe {
                let mut readback = None;
                device11
                    .CreateTexture2D(&desc, None, Some(&mut readback))
                    .map_err(os("CreateTexture2D(check readback)"))?;
                let readback = readback.ok_or(VsrError::Os {
                    what: "CreateTexture2D(check readback)",
                    hresult: 0,
                })?;
                let region = D3D11_BOX {
                    left,
                    top,
                    front: 0,
                    right: left + rw,
                    bottom: top + rh,
                    back: 1,
                };
                ctx.CopySubresourceRegion(&readback, 0, 0, 0, 0, output11, 0, Some(&region));
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                ctx.Map(&readback, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .map_err(os("Map(check readback)"))?;
                let row_bytes = rw as usize * 4;
                let mut pixels = Vec::with_capacity(row_bytes * rh as usize);
                for row in 0..rh as usize {
                    let src = mapped
                        .pData
                        .cast::<u8>()
                        .add(row * mapped.RowPitch as usize);
                    pixels.extend_from_slice(std::slice::from_raw_parts(src, row_bytes));
                }
                ctx.Unmap(&readback, 0);
                Ok(pixels)
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

        /// Frames processed by this scaler.
        pub fn frames(&self) -> u64 {
            self.frames
        }

        /// `"NVIDIA"` or `"Intel"`.
        pub fn vendor_name(&self) -> &'static str {
            if self.vendor == VENDOR_NVIDIA {
                "NVIDIA"
            } else {
                "Intel"
            }
        }

        /// Writes NV12 planes into the next staging texture and copies it to the input.
        fn upload(
            &mut self,
            y: &[u8],
            uv: &[u8],
            y_stride: usize,
            uv_stride: usize,
            w: usize,
            h: usize,
        ) -> Result<(), VsrError> {
            let tex_h = h.next_multiple_of(2);
            let uv_rows = h.div_ceil(2);
            let row_bytes = w.next_multiple_of(2);
            if w == 0
                || h == 0
                || y.len() < y_stride * (h - 1) + w
                || uv.len() < uv_stride * (uv_rows - 1) + row_bytes
            {
                return Err(VsrError::NotCpuFrame);
            }
            let ctx = &self.context;
            let staging = &self.staging[self.staging_index];
            self.staging_index ^= 1;
            // SAFETY: D3D11 calls on live objects from the render thread. The mapped
            // pointer is valid for `RowPitch * tex_h * 3 / 2` bytes (NV12 staging layout:
            // luma rows, then the interleaved chroma rows at `RowPitch * Height`); every
            // copy stays inside that and inside the source slices checked above.
            unsafe {
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                ctx.Map(staging, 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
                    .map_err(os("Map(NV12 staging)"))?;
                let pitch = mapped.RowPitch as usize;
                let base = mapped.pData.cast::<u8>();
                for row in 0..h {
                    std::ptr::copy_nonoverlapping(
                        y.as_ptr().add(row * y_stride),
                        base.add(row * pitch),
                        w,
                    );
                }
                let uv_base = base.add(pitch * tex_h);
                for row in 0..uv_rows {
                    std::ptr::copy_nonoverlapping(
                        uv.as_ptr().add(row * uv_stride),
                        uv_base.add(row * pitch),
                        row_bytes,
                    );
                }
                ctx.Unmap(staging, 0);
                ctx.CopyResource(&self.input, staging);
            }
            Ok(())
        }

        /// `VideoProcessorBlt` of the input into the shared output.
        fn blit(&self) -> Result<(), VsrError> {
            // SAFETY: live processor and views; the stream's interface reference is
            // released right after the call.
            unsafe {
                let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                    Enable: true.into(),
                    pInputSurface: ManuallyDrop::new(Some(self.input_view.clone())),
                    ..Default::default()
                };
                let blit = self.video_context.VideoProcessorBlt(
                    &self.processor,
                    &self.output_view,
                    0,
                    std::slice::from_ref(&stream),
                );
                ManuallyDrop::drop(&mut stream.pInputSurface);
                blit.map_err(os("VideoProcessorBlt"))
            }
        }

        /// Uploads `frame`, runs the super resolution blit into the shared output and
        /// stages the wait for it on wgpu's queue. The caller must submit on `queue`
        /// afterwards and then call [`VideoScaler::after_submit`].
        pub fn process(&mut self, queue: &wgpu::Queue, frame: &Frame) -> Result<(), VsrError> {
            let (y, uv, y_stride, uv_stride) = frame.nv12_planes().ok_or(VsrError::NotCpuFrame)?;
            if (frame.width, frame.height) != self.input_size {
                return Err(VsrError::NotCpuFrame);
            }
            // SAFETY: GPU-side wait on a fence we own; the value is already enqueued on
            // wgpu's queue (or 0), so it always completes.
            unsafe { self.context.Wait(&self.read11, self.read_value) }
                .map_err(os("ID3D11DeviceContext4::Wait"))?;
            self.upload(
                y,
                uv,
                y_stride,
                uv_stride,
                frame.width as usize,
                frame.height as usize,
            )?;
            if self.color != Some(frame.color) {
                // SAFETY: setter on a live processor.
                unsafe {
                    self.video_context.VideoProcessorSetStreamColorSpace1(
                        &self.processor,
                        0,
                        color_space(&frame.color),
                    );
                }
                self.color = Some(frame.color);
            }
            self.blit()?;
            self.blit_value += 1;
            // SAFETY: plain D3D11 calls on live objects.
            unsafe {
                self.context
                    .Signal(&self.blit11, self.blit_value)
                    .map_err(os("ID3D11DeviceContext4::Signal"))?;
                self.context.Flush();
            }

            // SAFETY: we only stage a wait on wgpu's queue; the guard is dropped at once.
            let hal = unsafe { queue.as_hal::<wgpu::hal::api::Dx12>() }.ok_or(VsrError::NotDx12)?;
            hal.add_wait_fence(self.blit12.clone(), self.blit_value);
            drop(hal);
            self.awaiting_submit = true;
            self.frames += 1;
            Ok(())
        }

        /// Call after the submit that samples the output: signals the `read` fence on
        /// wgpu's raw queue behind that submit, which the next blit waits for.
        pub fn after_submit(&mut self, queue: &wgpu::Queue) {
            if !self.awaiting_submit {
                return;
            }
            // SAFETY: `Signal` on wgpu's queue only appends a fence signal behind the
            // work wgpu already submitted; the guard is dropped at once.
            let Some(hal) = (unsafe { queue.as_hal::<wgpu::hal::api::Dx12>() }) else {
                return;
            };
            let value = self.read_value + 1;
            // SAFETY: see above.
            if unsafe { hal.as_raw().Signal(&self.read12, value) }.is_ok() {
                self.read_value = value;
                self.awaiting_submit = false;
            }
        }
    }
}

#[cfg(not(all(windows, feature = "vsr")))]
mod imp {
    use xcloud_media::Frame;

    use super::VsrError;

    /// Always `false`: video super resolution is not compiled in.
    pub fn supported(_device: &wgpu::Device) -> bool {
        false
    }

    /// Placeholder; [`VideoScaler::new`] always fails with [`VsrError::NotCompiled`].
    #[derive(Debug)]
    pub struct VideoScaler {
        never: std::convert::Infallible,
    }

    impl VideoScaler {
        /// Always `Err(NotCompiled)`.
        pub fn new(
            _device: &wgpu::Device,
            _input_size: (u32, u32),
            _output_size: (u32, u32),
        ) -> Result<(Self, wgpu::Texture), VsrError> {
            Err(VsrError::NotCompiled)
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
        pub fn vendor_name(&self) -> &'static str {
            match self.never {}
        }

        /// Unreachable: no value of this type exists.
        pub fn process(&mut self, _queue: &wgpu::Queue, _frame: &Frame) -> Result<(), VsrError> {
            match self.never {}
        }

        /// Unreachable: no value of this type exists.
        pub fn after_submit(&mut self, _queue: &wgpu::Queue) {
            match self.never {}
        }
    }
}

pub use imp::{VideoScaler, supported};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_matches_cfg() {
        assert_eq!(COMPILED, cfg!(all(windows, feature = "vsr")));
    }

    #[test]
    fn errors_describe_themselves() {
        assert!(VsrError::NotCompiled.to_string().contains("--features vsr"));
        assert_eq!(
            VsrError::UnsupportedVendor(0x1002).to_string(),
            "adapter vendor 0x1002 has no video super resolution extension"
        );
        let e = VsrError::Os {
            what: "VideoProcessorBlt",
            hresult: 0x8007_0057,
        };
        assert_eq!(
            e.to_string(),
            "VideoProcessorBlt failed with HRESULT 0x80070057"
        );
    }
}
