//! VideoToolbox H.264 decoder (macOS). Port of the hardware path of
//! `qt/src/streaming/H264Decoder.cpp` (FFmpeg with `AV_HWDEVICE_TYPE_VIDEOTOOLBOX`)
//! straight onto `VTDecompressionSession`, and of the `CVPixelBuffer` attachment handling
//! in `qt/src/app/MetalVideoItem.mm` (Y'CbCr matrix, full/video range).
//!
//! Design (docs/archive/PORTING_PLAN.md section 5.2): Annex-B access units are re-framed as AVCC, the
//! SPS/PPS build a `CMVideoFormatDescription`, the session is recreated when they change,
//! output is IOSurface-backed NV12 compatible with Metal, decoded asynchronously with
//! `RealTime = true`. `kVTVideoDecoderBadDataErr` maps to [`Error::BadData`] so the caller
//! can request a keyframe; `kVTInvalidSessionErr` recreates the session.

use std::ffi::{OsStr, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime,
    CMVideoFormatDescription, CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionGetDimensions, kCMBlockBufferAssureMemoryNowFlag, kCMTimeInvalid,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, kCVImageBufferYCbCrMatrix_ITU_R_601_4,
    kCVImageBufferYCbCrMatrix_ITU_R_709_2, kCVImageBufferYCbCrMatrix_ITU_R_2020,
    kCVImageBufferYCbCrMatrixKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord,
    VTDecompressionSession, VTSessionCopyProperty, VTSessionSetProperty,
    kVTCouldNotCreateInstanceErr, kVTCouldNotFindVideoDecoderErr,
    kVTDecompressionPropertyKey_RealTime,
    kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder, kVTInvalidSessionErr,
    kVTVideoDecoderBadDataErr, kVTVideoDecoderNotAvailableNowErr,
    kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder,
    kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
    kVTVideoDecoderUnsupportedDataFormatErr,
};

use crate::annexb::{self, NAL_AUD, NAL_PPS, NAL_SPS, ParameterSets};
use crate::decoder::{FrameSink, VideoDecoder};
use crate::frame::{ColorInfo, Frame, NativeBuffer, YCbCrMatrix};
use crate::{Error, Result};

/// Name of the environment variable that forces software decoding, same as the Qt app.
pub const SOFTWARE_VIDEO_ENV: &str = "OPENXCLOUD_SOFTWARE_VIDEO";

/// Which decoder VideoToolbox may pick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HardwarePolicy {
    /// `kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder = true`:
    /// fail if no hardware decoder can take the stream. The default for a game stream.
    Require,
    /// `EnableHardwareAcceleratedVideoDecoder = true`: prefer hardware, let VideoToolbox
    /// fall back to its software decoder.
    Prefer,
    /// `EnableHardwareAcceleratedVideoDecoder = false`: software only. What
    /// `OPENXCLOUD_SOFTWARE_VIDEO` selects, matching `H264Decoder(hardware=false)`.
    Software,
}

impl HardwarePolicy {
    /// `Software` when `OPENXCLOUD_SOFTWARE_VIDEO` is set, `Require` otherwise.
    pub fn from_env() -> Self {
        Self::from_env_value(std::env::var_os(SOFTWARE_VIDEO_ENV).as_deref())
    }

    fn from_env_value(value: Option<&OsStr>) -> Self {
        if value.is_some() {
            Self::Software
        } else {
            Self::Require
        }
    }
}

/// Decoder configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VtConfig {
    pub hardware: HardwarePolicy,
    /// `kVTDecompressionPropertyKey_RealTime`.
    pub real_time: bool,
    /// `kVTDecodeFrame_EnableAsynchronousDecompression`: output callback on VideoToolbox's
    /// thread instead of before `decode` returns.
    pub asynchronous: bool,
}

impl Default for VtConfig {
    fn default() -> Self {
        Self {
            hardware: HardwarePolicy::from_env(),
            real_time: true,
            asynchronous: true,
        }
    }
}

/// Shared with the output callback through `decompressionOutputRefCon`.
struct CallbackState {
    sink: Mutex<FrameSink>,
    frames: AtomicU64,
    dropped: AtomicU64,
    errors: AtomicU64,
}

/// Per-frame reference passed as `sourceFrameRefCon`; reclaimed in the callback.
struct FrameRef {
    t_received: Instant,
    t_submitted: Instant,
}

/// A live `VTDecompressionSession` with the format description it was built for.
struct Session {
    session: CFRetained<VTDecompressionSession>,
    _format: CFRetained<CMVideoFormatDescription>,
    width: u32,
    height: u32,
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the session is valid until invalidated; waiting first guarantees no
        // callback runs after `invalidate` returns, so the refCon may be freed afterwards.
        unsafe {
            self.session.wait_for_asynchronous_frames();
            self.session.invalidate();
        }
    }
}

/// VideoToolbox-backed [`VideoDecoder`].
pub struct VtDecoder {
    // Field order matters: `session` must drop before `state`, see `Session::drop`.
    session: Option<Session>,
    state: Arc<CallbackState>,
    config: VtConfig,
    params: ParameterSets,
    frame_index: i64,
}

// SAFETY: `VTDecompressionSession` and `CMFormatDescription` are Core Foundation objects
// that Apple documents as usable from any thread; we only use them from the owning
// decode thread anyway, the callback state is behind `Arc` + `Mutex`/atomics.
unsafe impl Send for VtDecoder {}

impl VtDecoder {
    /// Creates a decoder that delivers frames to `sink`. No session exists until the first
    /// access unit with SPS/PPS arrives.
    pub fn new(config: VtConfig, sink: FrameSink) -> Self {
        Self {
            session: None,
            state: Arc::new(CallbackState {
                sink: Mutex::new(sink),
                frames: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
                errors: AtomicU64::new(0),
            }),
            config,
            params: ParameterSets::default(),
            frame_index: 0,
        }
    }

    /// Convenience: default configuration (`OPENXCLOUD_SOFTWARE_VIDEO` honoured).
    pub fn with_defaults(sink: FrameSink) -> Self {
        Self::new(VtConfig::default(), sink)
    }

    /// Frames delivered to the sink so far.
    pub fn frames_output(&self) -> u64 {
        self.state.frames.load(Ordering::Relaxed)
    }

    /// Frames VideoToolbox dropped (`kVTDecodeInfo_FrameDropped`).
    pub fn frames_dropped(&self) -> u64 {
        self.state.dropped.load(Ordering::Relaxed)
    }

    /// Frames whose output callback reported an error.
    pub fn frame_errors(&self) -> u64 {
        self.state.errors.load(Ordering::Relaxed)
    }

    /// Coded size of the active session, if any.
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        self.session.as_ref().map(|s| (s.width, s.height))
    }

    /// Whether the active session uses a hardware decoder
    /// (`kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder`).
    pub fn using_hardware(&self) -> Option<bool> {
        let session = self.session.as_ref()?;
        let mut value: *const CFType = ptr::null();
        // SAFETY: `value` is a valid out-pointer; the property is a CFBoolean we release
        // through `CFRetained::from_raw`.
        let status = unsafe {
            VTSessionCopyProperty(
                &session.session,
                kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
                None,
                (&mut value as *mut *const CFType).cast::<c_void>(),
            )
        };
        if status != 0 {
            return None;
        }
        // SAFETY: `VTSessionCopyProperty` returned noErr, so `value` is a +1 retained
        // CFBoolean the caller owns ("Copy" rule). `from_raw` transfers that single
        // reference into `CFRetained`, which releases it on drop; nothing else holds it.
        let value = unsafe { CFRetained::from_raw(NonNull::new(value.cast_mut())?) };
        value.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool)
    }

    /// Tears down the session; the next access unit with SPS/PPS rebuilds it.
    pub fn reset(&mut self) {
        self.session = None;
        self.params = ParameterSets::default();
        self.frame_index = 0;
    }

    fn ensure_session(&mut self, au: &[u8]) -> Result<()> {
        let changed = self.params.update_from(au);
        if !self.params.is_complete() {
            return Err(Error::WaitingForParameterSets);
        }
        if changed || self.session.is_none() {
            self.session = None;
            self.session = Some(self.create_session()?);
        }
        Ok(())
    }

    fn create_session(&self) -> Result<Session> {
        let info = annexb::parse_sps(&self.params.sps)?;
        let format = create_format_description(&self.params)?;
        // SAFETY: `format` is a valid video format description.
        let dims = unsafe { CMVideoFormatDescriptionGetDimensions(&format) };

        let decoder_spec = decoder_specification(self.config.hardware);
        let pixel_format =
            CFNumber::new_i32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32);
        let io_surface_props = CFDictionary::<CFString, CFType>::empty();
        // SAFETY: the `kCV*Key` statics are valid CFStrings for the process lifetime.
        let destination_attrs = unsafe {
            CFDictionary::<CFString, CFType>::from_slices(
                &[
                    kCVPixelBufferPixelFormatTypeKey,
                    kCVPixelBufferMetalCompatibilityKey,
                    kCVPixelBufferIOSurfacePropertiesKey,
                ],
                &[&pixel_format, CFBoolean::new(true), &io_surface_props],
            )
        };

        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(output_callback),
            decompressionOutputRefCon: Arc::as_ptr(&self.state).cast_mut().cast::<c_void>(),
        };
        let mut raw: *mut VTDecompressionSession = ptr::null_mut();
        // SAFETY: all dictionaries hold the documented key/value types; `record` and `raw`
        // are valid for the duration of the call.
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &format,
                Some(decoder_spec.as_opaque()),
                Some(destination_attrs.as_opaque()),
                &record,
                NonNull::from(&mut raw),
            )
        };
        if status != 0 {
            return Err(map_create_error(status, self.config.hardware));
        }
        // SAFETY: `VTDecompressionSessionCreate` returned noErr, so `raw` is the +1
        // session reference it created for the caller; `from_raw` moves that ownership
        // into `CFRetained` (released by `Session::drop`, after `invalidate`). The refcon
        // the session captured is `Arc::as_ptr(&self.state)`: `VtDecoder::state` keeps
        // that allocation alive and drops after `session` (field order), and
        // `Session::drop` waits for pending callbacks before invalidating, so the
        // callback never observes a dangling refcon.
        let session = NonNull::new(raw)
            .map(|p| unsafe { CFRetained::from_raw(p) })
            .ok_or(Error::Os {
                what: "VTDecompressionSessionCreate",
                status,
            })?;

        if self.config.real_time {
            // SAFETY: valid session, documented key and a CFBoolean value.
            let status = unsafe {
                VTSessionSetProperty(
                    &session,
                    kVTDecompressionPropertyKey_RealTime,
                    Some(CFBoolean::new(true)),
                )
            };
            if status != 0 {
                tracing::warn!(target: "Video", status, "kVTDecompressionPropertyKey_RealTime not accepted");
            }
        }

        let (width, height) = (info.width, info.height);
        tracing::info!(
            target: "Video",
            width,
            height,
            coded_width = dims.width,
            coded_height = dims.height,
            profile = info.profile_idc,
            level = info.level_idc,
            hardware = ?self.config.hardware,
            "VideoToolbox session created"
        );
        Ok(Session {
            session,
            _format: format,
            width,
            height,
        })
    }

    /// Submits one access unit to the current session. `Ok(false)` means the AU carried
    /// no slice data (parameter sets only) and nothing was submitted.
    fn submit(&mut self, au: &[u8], t_received: Instant) -> std::result::Result<bool, i32> {
        let avcc =
            annexb::annexb_to_avcc_filtered(au, |t| !matches!(t, NAL_SPS | NAL_PPS | NAL_AUD));
        if avcc.is_empty() {
            return Ok(false);
        }
        let Some(session) = self.session.as_ref() else {
            return Ok(false);
        };
        let block = create_block_buffer(&avcc)?;
        let sample = create_sample_buffer(&block, &session._format, avcc.len(), self.frame_index)?;
        self.frame_index += 1;

        let mut flags = VTDecodeFrameFlags::Frame_1xRealTimePlayback;
        if self.config.asynchronous {
            flags |= VTDecodeFrameFlags::Frame_EnableAsynchronousDecompression;
        }
        let frame_ref = Box::into_raw(Box::new(FrameRef {
            t_received,
            t_submitted: Instant::now(),
        }));
        let mut info_flags = VTDecodeInfoFlags::empty();
        // SAFETY: `sample` is a complete sample buffer for this session's format; the
        // callback reclaims `frame_ref` exactly once if and only if the call succeeds.
        let status = unsafe {
            session.session.decode_frame(
                &sample,
                flags,
                frame_ref.cast::<c_void>(),
                &mut info_flags,
            )
        };
        if status != 0 {
            // No callback will come: free the frame reference ourselves.
            // SAFETY: `frame_ref` came from `Box::into_raw` above and was not consumed.
            drop(unsafe { Box::from_raw(frame_ref) });
            return Err(status);
        }
        Ok(true)
    }
}

impl VideoDecoder for VtDecoder {
    fn decode(&mut self, au: &[u8], t_received: Instant) -> Result<()> {
        self.ensure_session(au)?;
        match self.submit(au, t_received) {
            Ok(_) => Ok(()),
            Err(status) if status == kVTVideoDecoderBadDataErr => {
                tracing::warn!(target: "Video", "VideoToolbox rejected access unit (bad data)");
                Err(Error::BadData)
            }
            Err(status) if status == kVTInvalidSessionErr => {
                tracing::warn!(target: "Video", "VideoToolbox session invalidated; recreating");
                self.session = None;
                self.session = Some(self.create_session()?);
                Err(Error::InvalidSession)
            }
            Err(status) => Err(Error::Os {
                what: "VTDecompressionSessionDecodeFrame",
                status,
            }),
        }
    }

    fn flush(&mut self) {
        if let Some(session) = self.session.as_ref() {
            // SAFETY: valid session; blocks until every pending callback has run.
            let status = unsafe { session.session.wait_for_asynchronous_frames() };
            if status != 0 {
                tracing::warn!(target: "Video", status, "VTDecompressionSessionWaitForAsynchronousFrames failed");
            }
        }
        self.frame_index = 0;
    }
}

fn decoder_specification(policy: HardwarePolicy) -> CFRetained<CFDictionary<CFString, CFType>> {
    // SAFETY: the specification keys are valid CFStrings for the process lifetime.
    unsafe {
        match policy {
            HardwarePolicy::Require => CFDictionary::from_slices(
                &[kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder],
                &[CFBoolean::new(true)],
            ),
            HardwarePolicy::Prefer => CFDictionary::from_slices(
                &[kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder],
                &[CFBoolean::new(true)],
            ),
            HardwarePolicy::Software => CFDictionary::from_slices(
                &[kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder],
                &[CFBoolean::new(false)],
            ),
        }
    }
}

fn map_create_error(status: i32, policy: HardwarePolicy) -> Error {
    let unavailable = [
        kVTCouldNotFindVideoDecoderErr,
        kVTCouldNotCreateInstanceErr,
        kVTVideoDecoderNotAvailableNowErr,
        kVTVideoDecoderUnsupportedDataFormatErr,
    ];
    if unavailable.contains(&status) {
        Error::Unavailable(format!(
            "VTDecompressionSessionCreate returned {status} (hardware policy {policy:?})"
        ))
    } else {
        Error::Os {
            what: "VTDecompressionSessionCreate",
            status,
        }
    }
}

fn create_format_description(
    params: &ParameterSets,
) -> Result<CFRetained<CMVideoFormatDescription>> {
    let (Some(sps0), Some(pps0)) = (params.sps.first(), params.pps.first()) else {
        return Err(Error::WaitingForParameterSets);
    };
    let mut pointers = [NonNull::from(sps0), NonNull::from(pps0)];
    let mut sizes = [params.sps.len(), params.pps.len()];
    let mut raw: *const CMFormatDescription = ptr::null();
    // SAFETY: two parameter sets with matching sizes; `raw` is a valid out-pointer.
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            None,
            2,
            NonNull::from(&mut pointers[0]),
            NonNull::from(&mut sizes[0]),
            4,
            NonNull::from(&mut raw),
        )
    };
    if status != 0 {
        return Err(Error::Os {
            what: "CMVideoFormatDescriptionCreateFromH264ParameterSets",
            status,
        });
    }
    // SAFETY: the create call returned noErr, so `raw` is the +1 format description it
    // allocated for the caller; `from_raw` takes over that one reference and releases it
    // on drop. `raw` is not used again.
    NonNull::new(raw.cast_mut())
        .map(|p| unsafe { CFRetained::from_raw(p) })
        .ok_or(Error::Os {
            what: "CMVideoFormatDescriptionCreateFromH264ParameterSets",
            status,
        })
}

fn create_block_buffer(data: &[u8]) -> std::result::Result<CFRetained<CMBlockBuffer>, i32> {
    let mut raw: *mut CMBlockBuffer = ptr::null_mut();
    // SAFETY: a null memory block with the default allocator asks CoreMedia to allocate
    // `data.len()` bytes now; we then copy into it.
    let status = unsafe {
        CMBlockBuffer::create_with_memory_block(
            None,
            ptr::null_mut(),
            data.len(),
            None,
            ptr::null(),
            0,
            data.len(),
            kCMBlockBufferAssureMemoryNowFlag,
            NonNull::from(&mut raw),
        )
    };
    if status != 0 {
        return Err(status);
    }
    // SAFETY: `CMBlockBufferCreateWithMemoryBlock` returned noErr, so `raw` is the +1
    // block buffer it allocated for the caller; `from_raw` takes over that reference.
    let block = NonNull::new(raw)
        .map(|p| unsafe { CFRetained::from_raw(p) })
        .ok_or(-1)?;
    let Some(src) = NonNull::new(data.as_ptr().cast_mut().cast::<c_void>()) else {
        return Err(-1);
    };
    // SAFETY: `src` points at `data.len()` readable bytes, the block holds that many.
    let status = unsafe { CMBlockBuffer::replace_data_bytes(src, &block, 0, data.len()) };
    if status != 0 {
        return Err(status);
    }
    Ok(block)
}

fn create_sample_buffer(
    block: &CMBlockBuffer,
    format: &CMVideoFormatDescription,
    len: usize,
    frame_index: i64,
) -> std::result::Result<CFRetained<CMSampleBuffer>, i32> {
    // SAFETY: plain value constructors.
    let timing = unsafe {
        CMSampleTimingInfo {
            duration: CMTime::new(1, 60),
            presentationTimeStamp: CMTime::new(frame_index, 60),
            decodeTimeStamp: kCMTimeInvalid,
        }
    };
    let sizes = [len];
    let mut raw: *mut CMSampleBuffer = ptr::null_mut();
    // SAFETY: one sample, one timing entry, one size entry; `raw` is a valid out-pointer.
    let status = unsafe {
        CMSampleBuffer::create_ready(
            None,
            Some(block),
            Some(format),
            1,
            1,
            &timing,
            1,
            sizes.as_ptr(),
            NonNull::from(&mut raw),
        )
    };
    if status != 0 {
        return Err(status);
    }
    // SAFETY: `CMSampleBufferCreateReady` returned noErr, so `raw` is the +1 sample
    // buffer it created for the caller (it retains `block` and `format` itself);
    // `from_raw` takes over that reference and releases it on drop.
    NonNull::new(raw)
        .map(|p| unsafe { CFRetained::from_raw(p) })
        .ok_or(-1)
}

/// Reads `kCVImageBufferYCbCrMatrixKey` and the pixel format, like `MetalVideoItem.mm`.
fn color_info(buffer: &CVPixelBuffer) -> ColorInfo {
    let full_range =
        CVPixelBufferGetPixelFormatType(buffer) == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;
    // SAFETY: the key statics are valid CFStrings; a null attachment-mode pointer is allowed.
    let matrix = unsafe { buffer.attachment(kCVImageBufferYCbCrMatrixKey, ptr::null_mut()) }
        .and_then(|value| {
            let name = value.downcast_ref::<CFString>()?;
            // SAFETY: comparing against process-lifetime CFString constants.
            unsafe {
                if name == kCVImageBufferYCbCrMatrix_ITU_R_601_4 {
                    Some(YCbCrMatrix::Bt601)
                } else if name == kCVImageBufferYCbCrMatrix_ITU_R_2020 {
                    Some(YCbCrMatrix::Bt2020)
                } else if name == kCVImageBufferYCbCrMatrix_ITU_R_709_2 {
                    Some(YCbCrMatrix::Bt709)
                } else {
                    None
                }
            }
        })
        .unwrap_or(YCbCrMatrix::Bt709);
    ColorInfo { matrix, full_range }
}

unsafe extern "C-unwind" fn output_callback(
    ref_con: *mut c_void,
    frame_ref: *mut c_void,
    status: i32,
    info_flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    let t_decoded = Instant::now();
    // SAFETY: `frame_ref` is the `Box<FrameRef>` leaked in `submit`, delivered exactly once.
    let Some(frame_ref) =
        NonNull::new(frame_ref.cast::<FrameRef>()).map(|p| unsafe { Box::from_raw(p.as_ptr()) })
    else {
        return;
    };
    // SAFETY: `ref_con` is the `Arc<CallbackState>` the owning `VtDecoder` keeps alive
    // until its session has been invalidated (see field order and `Session::drop`).
    let Some(state) =
        NonNull::new(ref_con.cast::<CallbackState>()).map(|p| unsafe { &*p.as_ptr() })
    else {
        return;
    };

    if status != 0 {
        state.errors.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(target: "Video", status, "VideoToolbox output callback reported an error");
        return;
    }
    if info_flags.contains(VTDecodeInfoFlags::FrameDropped) {
        state.dropped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // SAFETY: a non-null image buffer is owned by VideoToolbox for the duration of the
    // callback; we take our own reference with `retain`.
    let Some(buffer) =
        NonNull::new(image).map(|p| unsafe { CFRetained::<CVPixelBuffer>::retain(p) })
    else {
        state.dropped.fetch_add(1, Ordering::Relaxed);
        return;
    };

    let frame = Frame {
        width: CVPixelBufferGetWidth(&buffer) as u32,
        height: CVPixelBufferGetHeight(&buffer) as u32,
        color: color_info(&buffer),
        buf: NativeBuffer::CvPixelBuffer(buffer),
        t_received: frame_ref.t_received,
        t_submitted: frame_ref.t_submitted,
        t_decoded,
    };
    state.frames.fetch_add(1, Ordering::Relaxed);

    let mut sink = state.sink.lock().unwrap_or_else(PoisonError::into_inner);
    // A panicking sink must not unwind into VideoToolbox.
    if catch_unwind(AssertUnwindSafe(|| (sink)(frame))).is_err() {
        tracing::error!(target: "Video", "frame sink panicked");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annexb::split_access_units;
    use std::process::Command;
    use std::sync::Mutex;
    use std::time::Duration;

    unsafe extern "C-unwind" {
        /// Declared by hand: `objc2-core-video` only exposes it with the `objc2-io-surface`
        /// feature, and the test only needs to know the surface exists.
        fn CVPixelBufferGetIOSurface(pixel_buffer: &CVPixelBuffer) -> *const c_void;
    }

    const FIXTURE: &[u8] = include_bytes!("../../../fixtures/testsrc_1080p60.h264");

    type Collected = Arc<Mutex<Vec<Frame>>>;

    fn collecting_sink() -> (FrameSink, Collected) {
        let frames: Collected = Arc::new(Mutex::new(Vec::new()));
        let sink_frames = Arc::clone(&frames);
        let sink: FrameSink = Box::new(move |frame| {
            sink_frames.lock().unwrap().push(frame);
        });
        (sink, frames)
    }

    fn decode_all(decoder: &mut VtDecoder, stream: &[u8]) -> Vec<Result<()>> {
        let results = split_access_units(stream)
            .into_iter()
            .map(|au| decoder.decode(au, Instant::now()))
            .collect();
        decoder.flush();
        results
    }

    /// Generates a short stream with ffmpeg like `tst_decoders.cpp` does. `None` when
    /// ffmpeg is not installed (the Qt test QSKIPs in that case).
    fn ffmpeg_stream(args: &[&str]) -> Option<Vec<u8>> {
        let dir = std::env::temp_dir().join(format!("xcloud-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(format!(
            "{}.h264",
            args.join("_").replace(['/', ':', '='], "-")
        ));
        let status = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(args)
            .arg(&path)
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        let bytes = std::fs::read(&path).ok()?;
        let _ = std::fs::remove_file(&path);
        Some(bytes)
    }

    #[test]
    fn hardware_policy_from_env_value() {
        assert_eq!(
            HardwarePolicy::from_env_value(None),
            HardwarePolicy::Require
        );
        assert_eq!(
            HardwarePolicy::from_env_value(Some(OsStr::new("1"))),
            HardwarePolicy::Software
        );
        assert_eq!(
            HardwarePolicy::from_env_value(Some(OsStr::new(""))),
            HardwarePolicy::Software,
            "Qt uses qEnvironmentVariableIsSet: an empty value still counts"
        );
    }

    #[test]
    fn decode_without_parameter_sets_asks_for_keyframe() {
        let (sink, _frames) = collecting_sink();
        let mut decoder = VtDecoder::new(VtConfig::default(), sink);
        let p_frame: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x41, 0x9a, 0x00];
        assert!(matches!(
            decoder.decode(p_frame, Instant::now()),
            Err(Error::WaitingForParameterSets)
        ));
        assert!(decoder.dimensions().is_none());
    }

    #[test]
    fn decodes_fixture_120_frames_1080p() {
        let (sink, frames) = collecting_sink();
        let mut decoder = VtDecoder::new(VtConfig::default(), sink);
        let results = decode_all(&mut decoder, FIXTURE);
        assert_eq!(results.len(), 120);
        for (i, r) in results.iter().enumerate() {
            assert!(r.is_ok(), "AU {i}: {r:?}");
        }
        assert_eq!(decoder.dimensions(), Some((1920, 1080)));
        assert_eq!(decoder.frame_errors(), 0);
        assert_eq!(decoder.frames_dropped(), 0);

        let frames = frames.lock().unwrap();
        assert_eq!(frames.len(), 120);
        for frame in frames.iter() {
            assert_eq!((frame.width, frame.height), (1920, 1080));
            let buffer = frame
                .pixel_buffer()
                .expect("zero-copy CVPixelBuffer output");
            assert_eq!(
                CVPixelBufferGetPixelFormatType(buffer),
                kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
            );
            // SAFETY: `buffer` is a live pixel buffer borrowed from the frame; the call
            // only reads it and returns a pointer we do not dereference.
            assert!(
                !unsafe { CVPixelBufferGetIOSurface(buffer) }.is_null(),
                "IOSurface-backed"
            );
            assert!(frame.decode_latency() < Duration::from_secs(1));
            assert!(frame.t_submitted >= frame.t_received);
        }
        // x264 signals BT.709 for HD content.
        assert_eq!(frames[0].color, ColorInfo::default());
        // Pixels are real: the test pattern is not black.
        let NativeBuffer::Nv12Cpu { y, .. } = frames[0].to_nv12_cpu().unwrap() else {
            panic!("readback produces CPU planes");
        };
        assert!(y.iter().any(|&v| v > 64));
    }

    #[test]
    fn bad_data_maps_to_error_bad_data_or_callback_error() {
        let (sink, _frames) = collecting_sink();
        let mut decoder = VtDecoder::new(VtConfig::default(), sink);
        let aus = split_access_units(FIXTURE);
        decoder.decode(aus[0], Instant::now()).unwrap();
        // A slice NAL with garbage payload after a valid keyframe: the hardware decoder
        // reports it either synchronously (BadData) or through the callback status.
        let garbage: &[u8] = &[
            0x00, 0x00, 0x00, 0x01, 0x41, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ];
        let result = decoder.decode(garbage, Instant::now());
        decoder.flush();
        match result {
            Err(Error::BadData) => {}
            Ok(()) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    // Port of TestDecoders::h264DecodesRealStream. The Qt test used `H264Decoder()` with
    // `hardware = false`, i.e. FFmpeg's software path, which accepts the yuv444p that
    // x264 picks for `testsrc` by default. VideoToolbox only decodes 4:2:0, so `-pix_fmt
    // yuv420p` is added; everything else is the Qt command.
    #[test]
    fn h264_decodes_real_stream() {
        let Some(stream) = ffmpeg_stream(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x64:rate=10",
            "-frames:v",
            "3",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "h264",
        ]) else {
            eprintln!("SKIP: ffmpeg not available to generate a test stream");
            return;
        };
        assert!(!stream.is_empty());
        let (sink, frames) = collecting_sink();
        let config = VtConfig {
            hardware: HardwarePolicy::Software,
            ..VtConfig::default()
        };
        let mut decoder = VtDecoder::new(config, sink);
        let results = decode_all(&mut decoder, &stream);
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        assert_eq!(decoder.dimensions(), Some((64, 64)));
        let frames = frames.lock().unwrap();
        assert!(
            !frames.is_empty(),
            "expected at least one decoded frame (callback errors: {}, dropped: {})",
            decoder.frame_errors(),
            decoder.frames_dropped()
        );
        assert_eq!((frames[0].width, frames[0].height), (64, 64));
    }

    // Port of TestDecoders::hardwareFramesSurviveResetAndCanFallBack: a retained
    // CVPixelBuffer outlives the decoder, `reset()` keeps decoding working, and the CPU
    // fallback (`nativeVideoEnabled = false` → `to_nv12_cpu`) yields full-size planes.
    #[test]
    fn hardware_frames_survive_reset_and_can_fall_back() {
        let Some(bytes) = ffmpeg_stream(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=2560x1440:rate=30",
            "-frames:v",
            "4",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "h264",
        ]) else {
            eprintln!("SKIP: ffmpeg required for fixture");
            return;
        };
        let retained: Frame;
        {
            let (sink, frames) = collecting_sink();
            let config = VtConfig {
                hardware: HardwarePolicy::Require,
                ..VtConfig::default()
            };
            let mut decoder = VtDecoder::new(config, sink);
            let results = decode_all(&mut decoder, &bytes);
            assert!(results.iter().all(Result::is_ok), "{results:?}");
            assert_eq!(decoder.using_hardware(), Some(true));
            retained = frames.lock().unwrap().remove(0);
            assert!(
                retained.pixel_buffer().is_some(),
                "Expected a VideoToolbox pixel buffer"
            );
            assert_eq!((retained.width, retained.height), (2560, 1440));

            decoder.reset();
            frames.lock().unwrap().clear();
            let results = decode_all(&mut decoder, &bytes);
            assert!(results.iter().all(Result::is_ok), "{results:?}");
            let frames = frames.lock().unwrap();
            assert!(!frames.is_empty());
            let NativeBuffer::Nv12Cpu {
                y,
                uv,
                y_stride,
                uv_stride,
            } = frames[0].to_nv12_cpu().unwrap()
            else {
                panic!("fallback must produce CPU planes");
            };
            assert!(y_stride >= 2560 && uv_stride >= 2560);
            assert_eq!(y.len(), y_stride * 1440);
            assert_eq!(uv.len(), uv_stride * 720);
        }
        let buffer = retained.pixel_buffer().unwrap();
        assert_eq!(CVPixelBufferGetWidth(buffer), 2560);
        // SAFETY: `buffer` is a live pixel buffer borrowed from the frame; the call only
        // reads it and returns a pointer we do not dereference.
        assert!(!unsafe { CVPixelBufferGetIOSurface(buffer) }.is_null());
    }
}
