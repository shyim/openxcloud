//! Port of `qt/src/streaming/NativeVideoFrame.h` (`NativeVideoFrame`: a retained,
//! IOSurface-backed `CVPixelBuffer` plus its size) extended with the colour metadata that
//! `qt/src/app/MetalVideoItem.mm` reads from the buffer attachments, and with the
//! per-stage timestamps the latency HUD needs (docs/archive/PORTING_PLAN.md section 4).

use std::fmt;
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use objc2_core_foundation::CFRetained;
#[cfg(target_os = "macos")]
use objc2_core_video::CVPixelBuffer;

/// Y'CbCr → RGB matrix signalled by the decoder (`kCVImageBufferYCbCrMatrixKey`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum YCbCrMatrix {
    /// ITU-R BT.601-4 (SD content).
    Bt601,
    /// ITU-R BT.709-2 (HD content; what xCloud sends).
    Bt709,
    /// ITU-R BT.2020.
    Bt2020,
}

/// Colour information the NV12→RGB convert shader needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ColorInfo {
    /// Which Y'CbCr matrix to apply.
    pub matrix: YCbCrMatrix,
    /// `true` for full-range (0–255) samples, `false` for video range (16–235 / 16–240).
    pub full_range: bool,
}

impl Default for ColorInfo {
    fn default() -> Self {
        Self {
            matrix: YCbCrMatrix::Bt709,
            full_range: false,
        }
    }
}

impl ColorInfo {
    /// The eight shader constants `MetalVideoItem.mm` uploads as two `float4`s:
    /// `[y_offset, y_scale, c_scale, 0, r_cr, g_cb, g_cr, b_cb]`.
    ///
    /// Kept bit-identical to the Qt kernel so the WGSL port can be validated against it.
    pub fn shader_coefficients(&self) -> [f32; 8] {
        let (y_offset, y_scale, c_scale) = if self.full_range {
            (0.0, 1.0, 1.0)
        } else {
            (16.0 / 255.0, 255.0 / 219.0, 255.0 / 224.0)
        };
        let (r_cr, g_cb, g_cr, b_cb) = match self.matrix {
            YCbCrMatrix::Bt709 => (1.5748, -0.187_324, -0.468_124, 1.8556),
            YCbCrMatrix::Bt601 => (1.402, -0.344_136, -0.714_136, 1.772),
            YCbCrMatrix::Bt2020 => (1.4746, -0.164_553, -0.571_353, 1.8814),
        };
        [y_offset, y_scale, c_scale, 0.0, r_cr, g_cb, g_cr, b_cb]
    }
}

/// The decoded pixels, in whatever form the backend produced them.
///
/// `CvPixelBuffer` is the zero-copy path: the IOSurface stays on the GPU and
/// `xcloud-gpu` wraps its planes as Metal textures. `Nv12Cpu` is the portable path
/// (FFmpeg backend, CPU readback, tests).
pub enum NativeBuffer {
    /// A retained, IOSurface-backed `CVPixelBuffer` from VideoToolbox
    /// (`kCVPixelFormatType_420YpCbCr8BiPlanar{Video,Full}Range`).
    #[cfg(target_os = "macos")]
    CvPixelBuffer(CFRetained<CVPixelBuffer>),
    /// NV12 planes in CPU memory. `y` is `y_stride * height` bytes, `uv` is
    /// `uv_stride * ceil(height / 2)` bytes of interleaved Cb/Cr.
    Nv12Cpu {
        y: Vec<u8>,
        uv: Vec<u8>,
        y_stride: usize,
        uv_stride: usize,
    },
}

// SAFETY: `CVPixelBuffer` is a Core Foundation object; CFRetain/CFRelease are atomic and
// Apple documents CoreVideo buffers as safe to pass between threads (the Qt app already
// hands them from the pump thread to the render thread). We only ever read the buffer
// through `&self`, never mutate pixels, and the `Nv12Cpu` variant holds plain `Vec<u8>`.
unsafe impl Send for NativeBuffer {}
// SAFETY: see above; all accessors take `&self` and are read-only.
unsafe impl Sync for NativeBuffer {}

impl fmt::Debug for NativeBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(target_os = "macos")]
            Self::CvPixelBuffer(buf) => {
                let w = objc2_core_video::CVPixelBufferGetWidth(buf);
                let h = objc2_core_video::CVPixelBufferGetHeight(buf);
                let fourcc = objc2_core_video::CVPixelBufferGetPixelFormatType(buf);
                write!(f, "CvPixelBuffer({w}x{h}, '{}')", fourcc_to_string(fourcc))
            }
            Self::Nv12Cpu {
                y,
                uv,
                y_stride,
                uv_stride,
            } => write!(
                f,
                "Nv12Cpu(y={} bytes @{y_stride}, uv={} bytes @{uv_stride})",
                y.len(),
                uv.len()
            ),
        }
    }
}

#[cfg(target_os = "macos")]
fn fourcc_to_string(code: u32) -> String {
    code.to_be_bytes()
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                char::from(b)
            } else {
                '?'
            }
        })
        .collect()
}

/// One decoded video frame with the timestamps of every stage it went through.
///
/// `t_received` is when the complete access unit left the jitter buffer, `t_submitted`
/// when it was handed to the decoder, `t_decoded` when the output callback fired.
#[derive(Debug)]
pub struct Frame {
    /// The pixels.
    pub buf: NativeBuffer,
    /// Display width in pixels (after cropping).
    pub width: u32,
    /// Display height in pixels (after cropping).
    pub height: u32,
    /// Matrix and range for the convert shader.
    pub color: ColorInfo,
    /// Access unit complete in the jitter buffer.
    pub t_received: Instant,
    /// Access unit handed to the decoder.
    pub t_submitted: Instant,
    /// Decoder output callback fired.
    pub t_decoded: Instant,
}

impl Frame {
    /// Time the frame spent inside the decoder (submit → output callback).
    pub fn decode_latency(&self) -> Duration {
        self.t_decoded.saturating_duration_since(self.t_submitted)
    }

    /// Time from the access unit being complete to the decoded frame being available.
    pub fn total_latency(&self) -> Duration {
        self.t_decoded.saturating_duration_since(self.t_received)
    }

    /// The underlying `CVPixelBuffer`, if this frame came from VideoToolbox.
    #[cfg(target_os = "macos")]
    pub fn pixel_buffer(&self) -> Option<&CVPixelBuffer> {
        match &self.buf {
            NativeBuffer::CvPixelBuffer(buf) => Some(buf),
            NativeBuffer::Nv12Cpu { .. } => None,
        }
    }

    /// The NV12 planes as `(y, uv, y_stride, uv_stride)` if this frame is in CPU memory.
    pub fn nv12_planes(&self) -> Option<(&[u8], &[u8], usize, usize)> {
        match &self.buf {
            NativeBuffer::Nv12Cpu {
                y,
                uv,
                y_stride,
                uv_stride,
            } => Some((y, uv, *y_stride, *uv_stride)),
            #[cfg(target_os = "macos")]
            NativeBuffer::CvPixelBuffer(_) => None,
        }
    }

    /// Copies the pixels into CPU memory as NV12. This is the equivalent of the Qt
    /// `av_hwframe_transfer_data` fallback (`nativeVideoEnabled == false`), used when a
    /// zero-copy GPU import is not available and by tests that inspect pixels.
    ///
    /// Returns the frame unchanged when it already is `Nv12Cpu`.
    #[cfg(target_os = "macos")]
    pub fn to_nv12_cpu(&self) -> crate::Result<NativeBuffer> {
        use objc2_core_video::{
            CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
            CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType,
            CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
            kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        };

        let buf = match &self.buf {
            NativeBuffer::CvPixelBuffer(buf) => buf,
            NativeBuffer::Nv12Cpu {
                y,
                uv,
                y_stride,
                uv_stride,
            } => {
                return Ok(NativeBuffer::Nv12Cpu {
                    y: y.clone(),
                    uv: uv.clone(),
                    y_stride: *y_stride,
                    uv_stride: *uv_stride,
                });
            }
        };

        let format = CVPixelBufferGetPixelFormatType(buf);
        if format != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
            && format != kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
        {
            return Err(crate::Error::Unsupported(
                "CVPixelBuffer is not 420YpCbCr8BiPlanar; cannot read back as NV12",
            ));
        }

        // SAFETY: `buf` is a live, retained CVPixelBuffer; lock/unlock are paired and the
        // plane pointers are only read between them.
        let status = unsafe { CVPixelBufferLockBaseAddress(buf, CVPixelBufferLockFlags::ReadOnly) };
        if status != 0 {
            return Err(crate::Error::Os {
                what: "CVPixelBufferLockBaseAddress",
                status,
            });
        }
        let copy_plane = |plane: usize| -> (Vec<u8>, usize) {
            let base = CVPixelBufferGetBaseAddressOfPlane(buf, plane);
            let stride = CVPixelBufferGetBytesPerRowOfPlane(buf, plane);
            let rows = CVPixelBufferGetHeightOfPlane(buf, plane);
            if base.is_null() || stride == 0 || rows == 0 {
                return (Vec::new(), stride);
            }
            // SAFETY: CoreVideo guarantees `stride * rows` readable bytes at `base` while
            // the buffer is locked.
            let bytes = unsafe { std::slice::from_raw_parts(base.cast::<u8>(), stride * rows) };
            (bytes.to_vec(), stride)
        };
        let (y, y_stride) = copy_plane(0);
        let (uv, uv_stride) = copy_plane(1);
        // SAFETY: paired with the lock above.
        unsafe { CVPixelBufferUnlockBaseAddress(buf, CVPixelBufferLockFlags::ReadOnly) };

        Ok(NativeBuffer::Nv12Cpu {
            y,
            uv,
            y_stride,
            uv_stride,
        })
    }
}

// The frame travels decode thread → triple buffer → render thread, so it must be
// `Send + Sync`. Fail at compile time if a future change breaks that.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Frame>();
    assert_send_sync::<NativeBuffer>();
};

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu_frame(w: u32, h: u32) -> Frame {
        let now = Instant::now();
        Frame {
            buf: NativeBuffer::Nv12Cpu {
                y: vec![0; (w * h) as usize],
                uv: vec![128; (w * h / 2) as usize],
                y_stride: w as usize,
                uv_stride: w as usize,
            },
            width: w,
            height: h,
            color: ColorInfo::default(),
            t_received: now,
            t_submitted: now + Duration::from_millis(1),
            t_decoded: now + Duration::from_millis(4),
        }
    }

    #[test]
    fn shader_coefficients_match_metal_video_item() {
        // Values copied from MetalVideoItem.mm `coefficients[8]`.
        let limited_709 = ColorInfo::default().shader_coefficients();
        assert_eq!(limited_709[0], 16.0 / 255.0);
        assert_eq!(limited_709[1], 255.0 / 219.0);
        assert_eq!(limited_709[2], 255.0 / 224.0);
        assert_eq!(&limited_709[4..], &[1.5748, -0.187_324, -0.468_124, 1.8556]);

        let full_601 = ColorInfo {
            matrix: YCbCrMatrix::Bt601,
            full_range: true,
        }
        .shader_coefficients();
        assert_eq!(&full_601[..4], &[0.0, 1.0, 1.0, 0.0]);
        assert_eq!(&full_601[4..], &[1.402, -0.344_136, -0.714_136, 1.772]);

        let bt2020 = ColorInfo {
            matrix: YCbCrMatrix::Bt2020,
            full_range: false,
        }
        .shader_coefficients();
        assert_eq!(&bt2020[4..], &[1.4746, -0.164_553, -0.571_353, 1.8814]);
    }

    #[test]
    fn latencies_are_derived_from_stage_timestamps() {
        let frame = cpu_frame(16, 16);
        assert_eq!(frame.decode_latency(), Duration::from_millis(3));
        assert_eq!(frame.total_latency(), Duration::from_millis(4));
    }

    #[test]
    fn nv12_planes_exposes_cpu_buffers() {
        let frame = cpu_frame(16, 8);
        let (y, uv, ys, uvs) = frame.nv12_planes().expect("cpu frame");
        assert_eq!(y.len(), 16 * 8);
        assert_eq!(uv.len(), 16 * 4);
        assert_eq!((ys, uvs), (16, 16));
        #[cfg(target_os = "macos")]
        assert!(frame.pixel_buffer().is_none());
    }

    #[test]
    fn debug_format_is_compact() {
        let frame = cpu_frame(4, 2);
        let text = format!("{:?}", frame.buf);
        assert_eq!(text, "Nv12Cpu(y=8 bytes @4, uv=4 bytes @4)");
    }
}
