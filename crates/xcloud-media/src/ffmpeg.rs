//! FFmpeg H.264 decoder (feature `ffmpeg`). Port of `qt/src/streaming/H264Decoder.{h,cpp}`
//! through `ffmpeg-next`: the portable backend for Windows/Linux and the macOS fallback (`OPENXCLOUD_SOFTWARE_VIDEO=ffmpeg`).
//!
//! Like the C++: `avcodec` h264 with `thread_count = 1` (no frame-threading reorder delay),
//! `AV_CODEC_FLAG_LOW_DELAY`, `AV_CODEC_FLAG2_FAST`, `err_recognition = AV_EF_EXPLODE`, an
//! `av_parser` in front of the decoder, `avcodec_flush_buffers` + parser re-init on
//! [`FfmpegDecoder::reset`], and on macOS a VideoToolbox `hw_device_ctx` unless
//! `OPENXCLOUD_SOFTWARE_VIDEO` is set (the pipeline's `H264Decoder m_videoDecoder{true}` plus the
//! constructor's `qEnvironmentVariableIsSet("OPENXCLOUD_SOFTWARE_VIDEO")` check; the variable's
//! value is irrelevant, only its presence counts). Off macOS the decoder is software unless
//! [`Hwaccel::Vaapi`] is requested explicitly (Linux, opt-in).
//!
//! Differences, all on the output side (docs/archive/PORTING_PLAN.md section 5.3): frames are delivered
//! as [`NativeBuffer::Nv12Cpu`] instead of an RGB `QImage` (yuv420p planes are interleaved
//! to NV12 in Rust, other pixel formats go through `swscale` to NV12; the GPU does the RGB
//! conversion), the colour matrix and range come from the frame's `colorspace` /
//! `color_range`, a rejected access unit is reported as [`Error::BadData`] so the pipeline
//! can request a keyframe, and the parser runs in `PARSER_FLAG_COMPLETE_FRAMES` mode by
//! default because [`VideoDecoder::decode`] already receives complete access units (the
//! C++ fed arbitrary byte ranges and paid one access unit of parser latency for it; set
//! [`FfmpegConfig::complete_frames`] to `false` for that behaviour).

use std::ffi::{OsStr, c_int};
use std::ptr::{self, NonNull};
use std::sync::Once;
use std::time::Instant;

use ffmpeg_next as ffmpeg;
use ffmpeg_next::ffi;

use crate::decoder::{FrameSink, VideoDecoder};
use crate::frame::{ColorInfo, Frame, NativeBuffer, YCbCrMatrix};
use crate::{Error, Result};

const LOG: &str = "Video";

/// Environment variable that forces software decoding, same name and semantics as the Qt
/// app (`qEnvironmentVariableIsSet`: any value, even empty, counts as set). The same switch
/// as `vt::SOFTWARE_VIDEO_ENV`; duplicated here because `vt` only exists on macOS.
pub const SOFTWARE_VIDEO_ENV: &str = "OPENXCLOUD_SOFTWARE_VIDEO";

/// Which hardware acceleration the codec context is created with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hwaccel {
    /// Pure software decoding (`H264Decoder(false)`, `OPENXCLOUD_SOFTWARE_VIDEO` set, or any
    /// platform other than macOS).
    None,
    /// `AV_HWDEVICE_TYPE_VIDEOTOOLBOX` through `av_hwdevice_ctx_create`; falls back to
    /// software when the device cannot be created, like the C++ constructor.
    VideoToolbox,
    /// `AV_HWDEVICE_TYPE_VAAPI` on the default DRM render node (Linux, opt-in). Decoded
    /// surfaces are downloaded to CPU NV12 with `av_hwframe_transfer_data`; falls back to
    /// software when the device cannot be created.
    Vaapi,
}

impl Default for Hwaccel {
    /// The platform default without consulting the environment: `VideoToolbox` on macOS
    /// (the C++ `hardware = true`), `None` elsewhere (where the C++ ignores `hardware`).
    fn default() -> Self {
        if cfg!(target_os = "macos") {
            Self::VideoToolbox
        } else {
            Self::None
        }
    }
}

impl Hwaccel {
    /// The C++ constructor's choice for `hardware = true`: `None` when `OPENXCLOUD_SOFTWARE_VIDEO`
    /// is set (whatever its value) or off macOS, `VideoToolbox` otherwise.
    pub fn from_env() -> Self {
        Self::from_env_value(std::env::var_os(SOFTWARE_VIDEO_ENV).as_deref())
    }

    fn from_env_value(software_video: Option<&OsStr>) -> Self {
        if software_video.is_some() {
            Self::None
        } else {
            Self::default()
        }
    }
}

/// Decoder configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FfmpegConfig {
    pub hwaccel: Hwaccel,
    /// With a hardware frame (`AV_PIX_FMT_VIDEOTOOLBOX`), hand the retained `CVPixelBuffer`
    /// to the sink instead of copying it to CPU memory. The C++ `nativeVideoEnabled`
    /// switch. Only meaningful on macOS; ignored elsewhere.
    pub native_frames: bool,
    /// `PARSER_FLAG_COMPLETE_FRAMES`: every `decode` call carries one complete access
    /// unit, so the parser passes it straight through. `false` restores the C++ byte-stream
    /// behaviour (the parser buffers until it sees the next access unit start).
    pub complete_frames: bool,
}

impl Default for FfmpegConfig {
    fn default() -> Self {
        Self {
            hwaccel: Hwaccel::from_env(),
            native_frames: true,
            complete_frames: true,
        }
    }
}

/// Owned `AVCodecParserContext` (the C++ `Impl::parser`).
struct Parser(NonNull<ffi::AVCodecParserContext>);

// FFmpeg 9 changed av_parser_init's parameter from int to AVCodecID. Let the
// installed headers select the type without transmuting FFI values or assuming
// that every platform ships the same FFmpeg major version.
trait ParserCodecId {
    const H264: Self;
}
impl ParserCodecId for c_int {
    const H264: Self = ffi::AVCodecID::AV_CODEC_ID_H264 as c_int;
}
impl ParserCodecId for ffi::AVCodecID {
    const H264: Self = ffi::AVCodecID::AV_CODEC_ID_H264;
}

impl Parser {
    fn new(complete_frames: bool) -> Result<Self> {
        // SAFETY: plain allocation; a null result is handled.
        let raw = unsafe { ffi::av_parser_init(ParserCodecId::H264) };
        let ptr = NonNull::new(raw)
            .ok_or_else(|| Error::Unavailable("av_parser_init(AV_CODEC_ID_H264) failed".into()))?;
        if complete_frames {
            // SAFETY: `ptr` is a live parser context we own.
            unsafe { (*ptr.as_ptr()).flags |= ffi::PARSER_FLAG_COMPLETE_FRAMES };
        }
        Ok(Self(ptr))
    }

    /// One `av_parser_parse2` step. Returns the number of input bytes consumed and the
    /// complete access unit the parser emitted, if any. An empty `input` flushes the
    /// parser's pending data.
    ///
    /// # Safety
    /// `codec` must be a live codec context; the returned slice is only valid until the
    /// next call on this parser or until `input` is dropped.
    unsafe fn parse<'a>(
        &mut self,
        codec: *mut ffi::AVCodecContext,
        input: &'a [u8],
    ) -> std::result::Result<(usize, Option<&'a [u8]>), c_int> {
        let mut out: *mut u8 = ptr::null_mut();
        let mut out_size: c_int = 0;
        let len = c_int::try_from(input.len()).map_err(|_| ffi::AVERROR(ffmpeg::error::EINVAL))?;
        // SAFETY: valid parser/codec contexts, `input` outlives the call, out-pointers are
        // valid for writes.
        let used = unsafe {
            ffi::av_parser_parse2(
                self.0.as_ptr(),
                codec,
                &mut out,
                &mut out_size,
                if input.is_empty() {
                    ptr::null()
                } else {
                    input.as_ptr()
                },
                len,
                ffi::AV_NOPTS_VALUE,
                ffi::AV_NOPTS_VALUE,
                0,
            )
        };
        if used < 0 {
            return Err(used);
        }
        let parsed = if out.is_null() || out_size <= 0 {
            None
        } else {
            // SAFETY: the parser guarantees `out_size` readable bytes at `out` until its
            // next call; `out` is either a window into `input` or the parser's own buffer.
            Some(unsafe { std::slice::from_raw_parts(out.cast_const(), out_size as usize) })
        };
        // `used` is in 0..=len, so the cast is lossless.
        Ok((used as usize, parsed))
    }
}

impl Drop for Parser {
    fn drop(&mut self) {
        // SAFETY: created by `av_parser_init`, closed exactly once.
        unsafe { ffi::av_parser_close(self.0.as_ptr()) };
    }
}

/// The open codec and the scratch buffers (the C++ `Impl`).
struct OpenCodec {
    decoder: ffmpeg::decoder::Video,
    parser: Parser,
    frame: ffmpeg::frame::Video,
    /// `swsWidth`/`swsHeight`/`swsFormat` live inside the scaling context.
    sws: Option<ffmpeg::software::scaling::Context>,
    sws_out: ffmpeg::frame::Video,
    hwaccel_active: bool,
    /// [`FfmpegConfig::native_frames`].
    native_frames: bool,
}

/// FFmpeg-backed [`VideoDecoder`].
pub struct FfmpegDecoder {
    codec: Option<OpenCodec>,
    config: FfmpegConfig,
    sink: FrameSink,
    last_error: Option<String>,
    frames: u64,
    errors: u64,
    dimensions: Option<(u32, u32)>,
}

// SAFETY: every FFmpeg object here (codec context, parser, frames, swscale context) is
// owned by this struct and only touched through `&mut self` from the thread that owns the
// decoder; FFmpeg contexts are not bound to the thread that created them. `FrameSink` is
// `Send` by its own bound.
unsafe impl Send for FfmpegDecoder {}

static INIT: Once = Once::new();

impl FfmpegDecoder {
    /// Creates a decoder that delivers frames to `sink`. Mirrors the C++ constructor: a
    /// missing h264 decoder or a failed `avcodec_open2` leaves the decoder invalid
    /// ([`is_valid`](Self::is_valid) is `false`, [`last_error`](Self::last_error) says
    /// why) and every `decode` call returns [`Error::Unavailable`].
    pub fn new(config: FfmpegConfig, sink: FrameSink) -> Self {
        INIT.call_once(|| {
            if let Err(err) = ffmpeg::init() {
                tracing::warn!(target: LOG, error = %err, "ffmpeg::init failed");
            }
        });
        let mut this = Self {
            codec: None,
            config,
            sink,
            last_error: None,
            frames: 0,
            errors: 0,
            dimensions: None,
        };
        match open_codec(&config) {
            Ok(codec) => {
                tracing::info!(
                    target: LOG,
                    hwaccel = codec.hwaccel_active,
                    complete_frames = config.complete_frames,
                    "FFmpeg h264 decoder opened"
                );
                this.codec = Some(codec);
            }
            Err(err) => {
                tracing::error!(target: LOG, error = %err, "FFmpeg h264 decoder unavailable");
                this.last_error = Some(err.to_string());
            }
        }
        this
    }

    /// Convenience: default configuration (`OPENXCLOUD_SOFTWARE_VIDEO` honoured).
    pub fn with_defaults(sink: FrameSink) -> Self {
        Self::new(FfmpegConfig::default(), sink)
    }

    /// `H264Decoder::isValid`.
    pub fn is_valid(&self) -> bool {
        self.codec.is_some()
    }

    /// `H264Decoder::lastError`: why the decoder is invalid, if it is.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Frames delivered to the sink so far.
    pub fn frames_output(&self) -> u64 {
        self.frames
    }

    /// Access units (or hardware transfers) the decoder rejected.
    pub fn frame_errors(&self) -> u64 {
        self.errors
    }

    /// Size of the most recent decoded frame (the C++ `outWidth`/`outHeight`).
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        self.dimensions
    }

    /// Whether a hardware device context is attached to the codec.
    pub fn hwaccel_active(&self) -> bool {
        self.codec.as_ref().is_some_and(|c| c.hwaccel_active)
    }

    /// [`FfmpegConfig::native_frames`] for the frames decoded from now on: the C++
    /// `nativeVideoEnabled` atomic, which the pipeline flips at run time on the same
    /// decoder. `false` downloads every hardware frame to CPU NV12 instead of handing out
    /// the retained `CVPixelBuffer`.
    pub fn set_native_frames(&mut self, native_frames: bool) {
        self.config.native_frames = native_frames;
        if let Some(codec) = self.codec.as_mut() {
            codec.native_frames = native_frames;
        }
    }

    /// Whether hardware frames are currently handed out as retained `CVPixelBuffer`s.
    pub fn native_frames(&self) -> bool {
        self.config.native_frames
    }

    /// `H264Decoder::reset`: `avcodec_flush_buffers` and a fresh parser. Pending output
    /// is discarded; use [`VideoDecoder::flush`] to deliver it first.
    pub fn reset(&mut self) {
        let Some(codec) = self.codec.as_mut() else {
            return;
        };
        codec.decoder.flush();
        match Parser::new(self.config.complete_frames) {
            Ok(parser) => codec.parser = parser,
            Err(err) => {
                // Extremely unlikely (allocation failure); keep the old parser rather than
                // losing the decoder.
                tracing::error!(target: LOG, error = %err, "av_parser_init failed on reset; keeping the old parser");
            }
        }
    }

    /// Sends one parsed packet and delivers every frame it yields.
    fn decode_packet(
        codec: &mut OpenCodec,
        packet: &[u8],
        t_received: Instant,
        t_submitted: Instant,
        emit: &mut dyn FnMut(Frame),
    ) -> std::result::Result<(), ffmpeg::Error> {
        let borrowed = ffmpeg::Packet::borrow(packet);
        codec.decoder.send_packet(&borrowed)?;
        Self::receive_all(codec, t_received, t_submitted, emit)
    }

    /// `while (avcodec_receive_frame(...) == 0)`.
    fn receive_all(
        codec: &mut OpenCodec,
        t_received: Instant,
        t_submitted: Instant,
        emit: &mut dyn FnMut(Frame),
    ) -> std::result::Result<(), ffmpeg::Error> {
        loop {
            match codec.decoder.receive_frame(&mut codec.frame) {
                Ok(()) => {}
                Err(ffmpeg::Error::Other {
                    errno: ffmpeg::error::EAGAIN,
                })
                | Err(ffmpeg::Error::Eof) => return Ok(()),
                Err(err) => return Err(err),
            }
            let t_decoded = Instant::now();
            let frame = convert_frame(codec, t_received, t_submitted, t_decoded);
            // SAFETY: `frame` is our own allocated AVFrame; unref releases the decoder's
            // buffers so the next receive starts clean.
            unsafe { ffi::av_frame_unref(codec.frame.as_mut_ptr()) };
            match frame {
                Ok(Some(frame)) => emit(frame),
                Ok(None) => {}
                Err(err) => return Err(err),
            }
        }
    }
}

impl VideoDecoder for FfmpegDecoder {
    fn decode(&mut self, au: &[u8], t_received: Instant) -> Result<()> {
        let t_submitted = Instant::now();
        let Some(codec) = self.codec.as_mut() else {
            return Err(Error::Unavailable(
                self.last_error
                    .clone()
                    .unwrap_or_else(|| "FFmpeg decoder is not open".into()),
            ));
        };

        let mut frames = 0u64;
        let mut dimensions = None;
        let mut bad_data = false;
        let mut fatal: Option<Error> = None;
        let sink = &mut self.sink;
        let mut emit = |frame: Frame| {
            frames += 1;
            dimensions = Some((frame.width, frame.height));
            sink(frame);
        };

        // The parser turns an arbitrary Annex-B byte stream into complete access units
        // (or, in `complete_frames` mode, passes our already complete one straight through).
        let mut remaining = au;
        while !remaining.is_empty() {
            // SAFETY: the codec context outlives the parse; the parsed slice is consumed
            // before the next parser call.
            let step = unsafe { codec.parser.parse(codec.decoder.as_mut_ptr(), remaining) };
            let (used, parsed) = match step {
                Ok(step) => step,
                Err(status) => {
                    fatal = Some(Error::Os {
                        what: "av_parser_parse2",
                        status,
                    });
                    break;
                }
            };
            if used == 0 && parsed.is_none() {
                // Defensive: a parser that neither consumes nor emits would loop forever.
                break;
            }
            let Some(packet) = parsed else {
                remaining = &remaining[used..];
                continue;
            };
            match Self::decode_packet(codec, packet, t_received, t_submitted, &mut emit) {
                Ok(()) => {}
                Err(ffmpeg::Error::InvalidData) => bad_data = true,
                Err(ffmpeg::Error::Other {
                    errno: ffmpeg::error::EAGAIN,
                }) => {
                    // Cannot happen: every packet is drained before the next one is sent.
                    bad_data = true;
                }
                Err(err) => {
                    fatal = Some(map_ffmpeg_error("avcodec_send_packet/receive_frame", err));
                    break;
                }
            }
            remaining = &remaining[used..];
        }

        self.frames += frames;
        if let Some(dims) = dimensions {
            self.dimensions = Some(dims);
        }
        if let Some(err) = fatal {
            self.errors += 1;
            return Err(err);
        }
        if bad_data {
            self.errors += 1;
            tracing::warn!(target: LOG, "FFmpeg rejected access unit (bad data)");
            return Err(Error::BadData);
        }
        Ok(())
    }

    fn flush(&mut self) {
        let Some(codec) = self.codec.as_mut() else {
            return;
        };
        let now = Instant::now();
        let mut frames = 0u64;
        let sink = &mut self.sink;
        let mut emit = |frame: Frame| {
            frames += 1;
            sink(frame);
        };
        // 1. Anything the parser still holds (only in byte-stream mode).
        // SAFETY: live contexts; an empty input is the documented parser flush.
        let pending = unsafe { codec.parser.parse(codec.decoder.as_mut_ptr(), &[]) };
        if let Ok((_, Some(packet))) = pending
            && let Err(err) = Self::decode_packet(codec, packet, now, now, &mut emit)
        {
            tracing::debug!(target: LOG, error = %err, "flush: pending packet rejected");
        }
        // 2. Drain the decoder (enters draining mode; `reset` leaves it again).
        if codec.decoder.send_eof().is_ok()
            && let Err(err) = Self::receive_all(codec, now, now, &mut emit)
        {
            tracing::debug!(target: LOG, error = %err, "flush: drain failed");
        }
        self.frames += frames;
        // 3. The Qt `reset()`.
        self.reset();
    }
}

/// The C++ constructor body: find h264, configure, optionally attach VideoToolbox, open.
fn open_codec(config: &FfmpegConfig) -> Result<OpenCodec> {
    let h264 = ffmpeg::decoder::find(ffmpeg::codec::Id::H264)
        .ok_or_else(|| Error::Unavailable("no H.264 decoder in this FFmpeg build".into()))?;
    let mut context = ffmpeg::codec::Context::new_with_codec(h264);
    // SAFETY: `context` owns a freshly allocated AVCodecContext; these are plain field
    // writes before `avcodec_open2`, exactly as the C++ does them.
    let hwaccel_active = unsafe {
        let ctx = context.as_mut_ptr();
        if ctx.is_null() {
            return Err(Error::Unavailable("avcodec_alloc_context3 failed".into()));
        }
        let hwaccel_active = attach_hwaccel(ctx, config.hwaccel);
        // Low latency: a single thread avoids the frame reordering delay an auto-threaded
        // decoder introduces, which matters for a game stream.
        (*ctx).thread_count = 1;
        (*ctx).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as c_int;
        (*ctx).flags2 |= ffi::AV_CODEC_FLAG2_FAST;
        (*ctx).err_recognition = ffi::AV_EF_EXPLODE;
        hwaccel_active
    };
    let decoder = context
        .decoder()
        .open_as(h264)
        .and_then(ffmpeg::decoder::Opened::video)
        .map_err(|err| Error::Unavailable(format!("avcodec_open2 failed: {err}")))?;
    let parser = Parser::new(config.complete_frames)?;
    Ok(OpenCodec {
        decoder,
        parser,
        frame: ffmpeg::frame::Video::empty(),
        sws: None,
        sws_out: ffmpeg::frame::Video::empty(),
        hwaccel_active,
        native_frames: config.native_frames,
    })
}

/// `av_hwdevice_ctx_create(AV_HWDEVICE_TYPE_VIDEOTOOLBOX)` + the `get_format` callback.
/// Returns whether a device context is attached.
///
/// # Safety
/// `ctx` must be a valid, not yet opened codec context.
unsafe fn attach_hwaccel(ctx: *mut ffi::AVCodecContext, hwaccel: Hwaccel) -> bool {
    match hwaccel {
        Hwaccel::None => false,
        Hwaccel::VideoToolbox => {
            if !cfg!(target_os = "macos") {
                tracing::warn!(target: LOG, "Hwaccel::VideoToolbox is macOS-only; decoding in software");
                return false;
            }
            // SAFETY: forwarded from the caller.
            unsafe {
                attach_device(
                    ctx,
                    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
                    "VIDEOTOOLBOX",
                    pick_videotoolbox_format,
                )
            }
        }
        Hwaccel::Vaapi => {
            if !cfg!(target_os = "linux") {
                tracing::warn!(target: LOG, "Hwaccel::Vaapi is Linux-only; decoding in software");
                return false;
            }
            // SAFETY: forwarded from the caller.
            unsafe {
                attach_device(
                    ctx,
                    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    "VAAPI",
                    pick_vaapi_format,
                )
            }
        }
    }
}

/// `av_hwdevice_ctx_create(kind)` on the default device and the matching `get_format`
/// callback. Returns whether a device context is attached.
///
/// # Safety
/// `ctx` must be a valid, not yet opened codec context.
unsafe fn attach_device(
    ctx: *mut ffi::AVCodecContext,
    kind: ffi::AVHWDeviceType,
    name: &str,
    get_format: unsafe extern "C" fn(
        *mut ffi::AVCodecContext,
        *const ffi::AVPixelFormat,
    ) -> ffi::AVPixelFormat,
) -> bool {
    let mut device: *mut ffi::AVBufferRef = ptr::null_mut();
    // SAFETY: valid out-pointer; a non-zero return leaves `device` null.
    let status =
        unsafe { ffi::av_hwdevice_ctx_create(&mut device, kind, ptr::null(), ptr::null_mut(), 0) };
    if status != 0 || device.is_null() {
        tracing::warn!(target: LOG, status, "av_hwdevice_ctx_create({name}) failed; decoding in software");
        return false;
    }
    // SAFETY: the codec context takes ownership of `device` and frees it in
    // `avcodec_free_context`.
    unsafe {
        (*ctx).hw_device_ctx = device;
        (*ctx).get_format = Some(get_format);
    }
    true
}

/// The C++ `get_format` lambda: prefer `AV_PIX_FMT_VIDEOTOOLBOX`, else the first offer.
unsafe extern "C" fn pick_videotoolbox_format(
    _ctx: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    // SAFETY: FFmpeg passes its AV_PIX_FMT_NONE-terminated offer list.
    unsafe { pick_format(formats, ffi::AVPixelFormat::AV_PIX_FMT_VIDEOTOOLBOX) }
}

/// Prefer `AV_PIX_FMT_VAAPI`, else the first (software) offer, e.g. for a profile the
/// driver cannot decode.
unsafe extern "C" fn pick_vaapi_format(
    _ctx: *mut ffi::AVCodecContext,
    formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    // SAFETY: FFmpeg passes its AV_PIX_FMT_NONE-terminated offer list.
    unsafe { pick_format(formats, ffi::AVPixelFormat::AV_PIX_FMT_VAAPI) }
}

/// `wanted` when FFmpeg offers it, else the first offer.
///
/// # Safety
/// `formats` must be null or an `AV_PIX_FMT_NONE`-terminated list.
unsafe fn pick_format(
    formats: *const ffi::AVPixelFormat,
    wanted: ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    if formats.is_null() {
        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    }
    let mut p = formats;
    // SAFETY: FFmpeg terminates the list with AV_PIX_FMT_NONE.
    unsafe {
        while *p != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p == wanted {
                return *p;
            }
            p = p.add(1);
        }
        *formats
    }
}

fn map_ffmpeg_error(what: &'static str, err: ffmpeg::Error) -> Error {
    match err {
        ffmpeg::Error::InvalidData => Error::BadData,
        ffmpeg::Error::DecoderNotFound => Error::Unavailable(err.to_string()),
        other => Error::Os {
            what,
            status: c_int::from(other),
        },
    }
}

/// Matrix and range from the FFmpeg tags, like `MetalVideoItem.mm` reads them off the
/// `CVPixelBuffer`. Unspecified tags fall back to BT.709 limited range (what xCloud sends).
fn color_info(
    space: ffmpeg::color::Space,
    range: ffmpeg::color::Range,
    full_format: bool,
) -> ColorInfo {
    use ffmpeg::color::{Range, Space};
    let matrix = match space {
        Space::BT470BG | Space::SMPTE170M | Space::SMPTE240M => YCbCrMatrix::Bt601,
        Space::BT2020NCL | Space::BT2020CL => YCbCrMatrix::Bt2020,
        _ => YCbCrMatrix::Bt709,
    };
    let full_range = match range {
        Range::JPEG => true,
        Range::MPEG => false,
        _ => full_format,
    };
    ColorInfo { matrix, full_range }
}

/// Turns the decoder's `AVFrame` into a [`Frame`]. `Ok(None)` when the frame had to be
/// skipped (the C++ `continue` after a failed `av_hwframe_transfer_data`).
fn convert_frame(
    codec: &mut OpenCodec,
    t_received: Instant,
    t_submitted: Instant,
    t_decoded: Instant,
) -> std::result::Result<Option<Frame>, ffmpeg::Error> {
    use ffmpeg::format::Pixel;

    let width = codec.frame.width();
    let height = codec.frame.height();
    if width == 0 || height == 0 {
        return Ok(None);
    }
    let format = codec.frame.format();
    let color = color_info(
        codec.frame.color_space(),
        codec.frame.color_range(),
        matches!(format, Pixel::YUVJ420P | Pixel::YUVJ422P | Pixel::YUVJ444P),
    );

    if matches!(format, Pixel::VIDEOTOOLBOX | Pixel::VAAPI) {
        #[cfg(target_os = "macos")]
        {
            if format == Pixel::VIDEOTOOLBOX
                && codec.native_frames
                && let Some(buf) = native_pixel_buffer(&codec.frame)
            {
                return Ok(Some(Frame {
                    buf,
                    width,
                    height,
                    color,
                    t_received,
                    t_submitted,
                    t_decoded,
                }));
            }
        }
        // The CPU fallback (`nativeVideoEnabled == false`) and every VAAPI frame: download
        // the hardware frame.
        let mut software = ffmpeg::frame::Video::empty();
        // SAFETY: `software` is an empty frame FFmpeg fills; `frame` is a valid hw frame.
        let status = unsafe {
            ffi::av_hwframe_transfer_data(software.as_mut_ptr(), codec.frame.as_ptr(), 0)
        };
        if status < 0 {
            tracing::warn!(target: LOG, status, "av_hwframe_transfer_data failed; frame skipped");
            return Ok(None);
        }
        let buf = nv12_from_software_frame(&software, &mut codec.sws, &mut codec.sws_out)?;
        return Ok(Some(Frame {
            buf,
            width,
            height,
            color,
            t_received,
            t_submitted,
            t_decoded,
        }));
    }

    let buf = nv12_from_software_frame(&codec.frame, &mut codec.sws, &mut codec.sws_out)?;
    Ok(Some(Frame {
        buf,
        width,
        height,
        color,
        t_received,
        t_submitted,
        t_decoded,
    }))
}

/// Retains the `CVPixelBuffer` behind `AVFrame.data[3]` (the C++ native path). `None` when
/// the native path is disabled or the frame carries no buffer.
#[cfg(target_os = "macos")]
fn native_pixel_buffer(frame: &ffmpeg::frame::Video) -> Option<NativeBuffer> {
    use objc2_core_foundation::CFRetained;
    use objc2_core_video::CVPixelBuffer;

    // SAFETY: for AV_PIX_FMT_VIDEOTOOLBOX frames FFmpeg documents `data[3]` as a
    // `CVPixelBufferRef` owned by the frame; we take our own reference.
    let raw = unsafe { (*frame.as_ptr()).data[3] }.cast::<CVPixelBuffer>();
    let ptr = NonNull::new(raw)?;
    // SAFETY: `ptr` is a live CVPixelBuffer for the lifetime of `frame`; retain bumps the
    // CF refcount so the frame may be unref'd afterwards.
    let retained = unsafe { CFRetained::<CVPixelBuffer>::retain(ptr) };
    Some(NativeBuffer::CvPixelBuffer(retained))
}

/// NV12 planes in CPU memory from a software frame of any pixel format: yuv420p (what the
/// h264 decoder produces) and nv12 (what `av_hwframe_transfer_data` produces) are copied
/// directly, anything else goes through `swscale` to NV12 (`SWS_BILINEAR`, same as the
/// C++ RGB conversion).
fn nv12_from_software_frame(
    frame: &ffmpeg::frame::Video,
    sws: &mut Option<ffmpeg::software::scaling::Context>,
    sws_out: &mut ffmpeg::frame::Video,
) -> std::result::Result<NativeBuffer, ffmpeg::Error> {
    use ffmpeg::format::Pixel;
    use ffmpeg::software::scaling::{Context, Flags};

    match frame.format() {
        Pixel::YUV420P | Pixel::YUVJ420P if frame.planes() >= 3 => Ok(nv12_from_yuv420p(frame)),
        Pixel::NV12 if frame.planes() >= 2 => Ok(nv12_from_nv12(frame)),
        format => {
            let (w, h) = (frame.width(), frame.height());
            let stale = sws.as_ref().is_none_or(|s| {
                let i = s.input();
                i.format != format || i.width != w || i.height != h
            });
            if stale {
                *sws = Some(Context::get(
                    format,
                    w,
                    h,
                    Pixel::NV12,
                    w,
                    h,
                    Flags::BILINEAR,
                )?);
                *sws_out = ffmpeg::frame::Video::empty();
            }
            let Some(ctx) = sws.as_mut() else {
                return Err(ffmpeg::Error::Bug);
            };
            ctx.run(frame, sws_out)?;
            if sws_out.planes() < 2 {
                return Err(ffmpeg::Error::Bug);
            }
            Ok(nv12_from_nv12(sws_out))
        }
    }
}

/// Copies `rows` rows of `row_bytes` out of a plane with `src_stride`, dropping padding.
fn compact_plane(src: &[u8], src_stride: usize, row_bytes: usize, rows: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(row_bytes * rows);
    for row in src.chunks(src_stride).take(rows) {
        let take = row.len().min(row_bytes);
        out.extend_from_slice(&row[..take]);
        out.resize(out.len() + (row_bytes - take), 0);
    }
    out
}

fn nv12_from_nv12(frame: &ffmpeg::frame::Video) -> NativeBuffer {
    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let chroma_rows = height.div_ceil(2);
    let uv_row_bytes = width.div_ceil(2) * 2;
    NativeBuffer::Nv12Cpu {
        y: compact_plane(frame.data(0), frame.stride(0), width, height),
        uv: compact_plane(frame.data(1), frame.stride(1), uv_row_bytes, chroma_rows),
        y_stride: width,
        uv_stride: uv_row_bytes,
    }
}

/// Interleaves the decoder's planar Cb and Cr planes into NV12's single UV plane.
fn nv12_from_yuv420p(frame: &ffmpeg::frame::Video) -> NativeBuffer {
    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let chroma_width = width.div_ceil(2);
    let chroma_rows = height.div_ceil(2);
    let uv_stride = chroma_width * 2;

    let y = compact_plane(frame.data(0), frame.stride(0), width, height);
    let mut uv = vec![0u8; uv_stride * chroma_rows];
    let u_rows = frame.data(1).chunks(frame.stride(1));
    let v_rows = frame.data(2).chunks(frame.stride(2));
    for ((u_row, v_row), out_row) in u_rows.zip(v_rows).zip(uv.chunks_exact_mut(uv_stride)) {
        let u_row = &u_row[..chroma_width.min(u_row.len())];
        let v_row = &v_row[..chroma_width.min(v_row.len())];
        let (pairs, _) = out_row.as_chunks_mut::<2>();
        for ((u, v), out) in u_row.iter().zip(v_row).zip(pairs) {
            *out = [*u, *v];
        }
    }
    NativeBuffer::Nv12Cpu {
        y,
        uv,
        y_stride: width,
        uv_stride,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annexb::split_access_units;
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[cfg(target_os = "macos")]
    use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth};

    #[cfg(target_os = "macos")]
    unsafe extern "C-unwind" {
        /// Declared by hand like in `vt.rs`: `objc2-core-video` only exposes it with the
        /// `objc2-io-surface` feature, and the test only needs to know the surface exists.
        fn CVPixelBufferGetIOSurface(pixel_buffer: &CVPixelBuffer) -> *const std::ffi::c_void;
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

    fn software_config() -> FfmpegConfig {
        FfmpegConfig {
            hwaccel: Hwaccel::None,
            native_frames: true,
            complete_frames: true,
        }
    }

    fn decode_aus(decoder: &mut FfmpegDecoder, aus: &[&[u8]]) -> Vec<Result<()>> {
        aus.iter()
            .map(|au| decoder.decode(au, Instant::now()))
            .collect()
    }

    /// Generates a short stream with ffmpeg like `tst_decoders.cpp` does. `None` when
    /// ffmpeg is not installed (the Qt test QSKIPs in that case).
    fn ffmpeg_stream(args: &[&str]) -> Option<Vec<u8>> {
        let dir = std::env::temp_dir().join(format!("xcloud-media-ffmpeg-{}", std::process::id()));
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

    fn assert_nv12_1080p(frame: &Frame) {
        assert_eq!((frame.width, frame.height), (1920, 1080));
        let (y, uv, y_stride, uv_stride) = frame.nv12_planes().expect("CPU NV12 planes");
        assert_eq!(y_stride, 1920);
        assert_eq!(uv_stride, 1920);
        assert_eq!(y.len(), 1920 * 1080);
        assert_eq!(uv.len(), 1920 * 540);
    }

    /// `H264Decoder(true)` + `qEnvironmentVariableIsSet("OPENXCLOUD_SOFTWARE_VIDEO")`: hardware
    /// on macOS unless the variable exists, software for any value of it (even `0` or
    /// empty, like `qEnvironmentVariableIsSet`), and never hardware off macOS.
    #[test]
    fn hwaccel_from_env_value() {
        let platform_default = if cfg!(target_os = "macos") {
            Hwaccel::VideoToolbox
        } else {
            Hwaccel::None
        };
        assert_eq!(Hwaccel::default(), platform_default);
        assert_eq!(Hwaccel::from_env_value(None), platform_default);
        for value in ["", "0", "1", "true", "false", "ffmpeg"] {
            assert_eq!(
                Hwaccel::from_env_value(Some(OsStr::new(value))),
                Hwaccel::None,
                "OPENXCLOUD_SOFTWARE_VIDEO={value:?} must force software"
            );
        }
        assert_eq!(SOFTWARE_VIDEO_ENV, "OPENXCLOUD_SOFTWARE_VIDEO");
        #[cfg(target_os = "macos")]
        assert_eq!(SOFTWARE_VIDEO_ENV, crate::vt::SOFTWARE_VIDEO_ENV);
    }

    /// The default configuration is what `NativeMediaPipeline` built the C++ decoder with:
    /// `hardware = true`, native frames on.
    #[test]
    fn default_config_matches_pipeline_decoder() {
        let config = FfmpegConfig::default();
        assert!(config.native_frames);
        assert!(config.complete_frames);
        let expected = if std::env::var_os(SOFTWARE_VIDEO_ENV).is_some() {
            Hwaccel::None
        } else {
            Hwaccel::default()
        };
        assert_eq!(config.hwaccel, expected);
    }

    #[test]
    fn color_info_maps_ffmpeg_tags() {
        use ffmpeg::color::{Range, Space};
        assert_eq!(
            color_info(Space::BT709, Range::MPEG, false),
            ColorInfo::default()
        );
        assert_eq!(
            color_info(Space::Unspecified, Range::Unspecified, false),
            ColorInfo::default(),
            "unspecified tags default to BT.709 limited"
        );
        assert_eq!(
            color_info(Space::SMPTE170M, Range::JPEG, false),
            ColorInfo {
                matrix: YCbCrMatrix::Bt601,
                full_range: true
            }
        );
        assert_eq!(
            color_info(Space::BT2020NCL, Range::Unspecified, true),
            ColorInfo {
                matrix: YCbCrMatrix::Bt2020,
                full_range: true
            },
            "a yuvj format implies full range when the tag is missing"
        );
    }

    #[test]
    fn compact_plane_drops_padding_and_pads_short_rows() {
        let src = [1, 2, 3, 9, 9, 4, 5, 6, 9, 9];
        assert_eq!(compact_plane(&src, 5, 3, 2), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(compact_plane(&src[..7], 5, 3, 2), vec![1, 2, 3, 4, 5, 0]);
    }

    #[test]
    fn decoder_opens_in_software() {
        let (sink, _frames) = collecting_sink();
        let decoder = FfmpegDecoder::new(software_config(), sink);
        assert!(decoder.is_valid(), "{:?}", decoder.last_error());
        assert!(!decoder.hwaccel_active());
        assert!(decoder.dimensions().is_none());
    }

    #[test]
    fn slice_without_parameter_sets_yields_no_frame() {
        let (sink, frames) = collecting_sink();
        let mut decoder = FfmpegDecoder::new(software_config(), sink);
        let p_frame: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x41, 0x9a, 0x00, 0x11, 0x22];
        let result = decoder.decode(p_frame, Instant::now());
        match result {
            Ok(()) | Err(Error::BadData) => {}
            other => panic!("unexpected: {other:?}"),
        }
        assert!(frames.lock().unwrap().is_empty());
        assert_eq!(decoder.frames_output(), 0);
    }

    /// The headline test of the task: every access unit of the fixture decodes to one
    /// 1920x1080 NV12 frame, without any parser delay, in software.
    #[test]
    fn decodes_fixture_120_frames_1080p() {
        let (sink, frames) = collecting_sink();
        let mut decoder = FfmpegDecoder::new(software_config(), sink);
        assert!(decoder.is_valid(), "{:?}", decoder.last_error());
        let aus = split_access_units(FIXTURE);
        assert_eq!(aus.len(), 120);
        let results = decode_aus(&mut decoder, &aus);
        for (i, r) in results.iter().enumerate() {
            assert!(r.is_ok(), "AU {i}: {r:?}");
        }
        // No flush needed: complete_frames mode delivers each frame from its own decode().
        assert_eq!(decoder.frames_output(), 120);
        assert_eq!(decoder.frame_errors(), 0);
        assert_eq!(decoder.dimensions(), Some((1920, 1080)));

        let frames = frames.lock().unwrap();
        assert_eq!(frames.len(), 120);
        for frame in frames.iter() {
            assert_nv12_1080p(frame);
            assert!(frame.decode_latency() < Duration::from_secs(1));
            assert!(frame.t_submitted >= frame.t_received);
            assert!(frame.t_decoded >= frame.t_submitted);
        }
        // x264 signals BT.709 limited range for HD content.
        assert_eq!(frames[0].color, ColorInfo::default());
        // Pixels are real: the test pattern is neither black nor grey chroma everywhere.
        let (y, uv, ..) = frames[0].nv12_planes().unwrap();
        assert!(y.iter().any(|&v| v > 64));
        assert!(uv.iter().any(|&v| v != 128));
    }

    // Port of TestDecoders::h264DecodesRealStream, literally: the whole byte stream in one
    // `decode()` call with the C++ parser behaviour (`complete_frames = false`), expecting
    // at least one 64x64 frame. x264 picks yuv444p for `testsrc` here, which exercises the
    // swscale path to NV12.
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
            "-f",
            "h264",
        ]) else {
            eprintln!("SKIP: ffmpeg not available to generate a test stream");
            return;
        };
        assert!(!stream.is_empty());
        let (sink, frames) = collecting_sink();
        let config = FfmpegConfig {
            complete_frames: false,
            ..software_config()
        };
        let mut decoder = FfmpegDecoder::new(config, sink);
        assert!(decoder.is_valid(), "{:?}", decoder.last_error());
        decoder.decode(&stream, Instant::now()).unwrap();
        decoder.flush();
        assert_eq!(decoder.dimensions(), Some((64, 64)));
        let frames = frames.lock().unwrap();
        assert!(!frames.is_empty(), "expected at least one decoded frame");
        assert_eq!(frames.len(), 3, "flush delivers the parser's pending frame");
        for frame in frames.iter() {
            assert_eq!((frame.width, frame.height), (64, 64));
            let (y, uv, y_stride, uv_stride) = frame.nv12_planes().expect("NV12 planes");
            assert_eq!((y_stride, uv_stride), (64, 64));
            assert_eq!(y.len(), 64 * 64);
            assert_eq!(uv.len(), 64 * 32);
        }
    }

    /// Byte-stream mode on the fixture: the parser holds back one access unit until the
    /// next one starts (the C++ latency), `flush` delivers it.
    #[test]
    fn byte_stream_mode_needs_flush_for_the_last_frame() {
        let (sink, frames) = collecting_sink();
        let config = FfmpegConfig {
            complete_frames: false,
            ..software_config()
        };
        let mut decoder = FfmpegDecoder::new(config, sink);
        decoder.decode(FIXTURE, Instant::now()).unwrap();
        let before = frames.lock().unwrap().len();
        assert!(before == 119 || before == 120, "got {before}");
        decoder.flush();
        assert_eq!(frames.lock().unwrap().len(), 120);
        assert_nv12_1080p(&frames.lock().unwrap()[119]);
    }

    // Port of the fallback half of TestDecoders::hardwareFramesSurviveResetAndCanFallBack:
    // `reset()` mid-stream keeps the decoder usable, decoding continues from the next IDR
    // with full-size CPU planes, and a frame handed out earlier stays valid.
    #[test]
    fn reset_mid_stream_then_continue() {
        let (sink, frames) = collecting_sink();
        let mut decoder = FfmpegDecoder::new(software_config(), sink);
        let aus = split_access_units(FIXTURE);

        let results = decode_aus(&mut decoder, &aus[..30]);
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        let retained = frames.lock().unwrap().remove(0);
        assert_nv12_1080p(&retained);

        decoder.reset();
        frames.lock().unwrap().clear();
        // The GOP is 60: AU 60 is the second IDR, so decoding resumes cleanly there.
        let results = decode_aus(&mut decoder, &aus[60..]);
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        assert_eq!(frames.lock().unwrap().len(), 60);
        for frame in frames.lock().unwrap().iter() {
            assert_nv12_1080p(frame);
        }

        // Like the C++ test: reset, then the whole stream again from the start.
        decoder.reset();
        frames.lock().unwrap().clear();
        let results = decode_aus(&mut decoder, &aus);
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        assert_eq!(frames.lock().unwrap().len(), 120);
        assert_eq!(decoder.frame_errors(), 0);

        // The frame taken out before the reset is untouched by it.
        assert_nv12_1080p(&retained);
    }

    #[test]
    fn garbage_after_keyframe_is_bad_data() {
        let (sink, _frames) = collecting_sink();
        let mut decoder = FfmpegDecoder::new(software_config(), sink);
        let aus = split_access_units(FIXTURE);
        decoder.decode(aus[0], Instant::now()).unwrap();
        // A P-slice NAL whose payload is garbage: AV_EF_EXPLODE turns it into an error.
        let garbage: &[u8] = &[
            0x00, 0x00, 0x00, 0x01, 0x41, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ];
        match decoder.decode(garbage, Instant::now()) {
            Err(Error::BadData) => assert_eq!(decoder.frame_errors(), 1),
            Ok(()) => {} // concealed; acceptable, the stream stays decodable
            other => panic!("unexpected: {other:?}"),
        }
        // The decoder is still usable afterwards.
        decoder.decode(aus[1], Instant::now()).unwrap();
    }

    /// The hardware path on macOS on the 1080p fixture: with the VideoToolbox device
    /// attached, frames come back either as retained, IOSurface-backed `CVPixelBuffer`s or,
    /// with the native path disabled, as CPU NV12 after `av_hwframe_transfer_data`. The
    /// device context must exist on macOS (the Qt test fails without a hardware frame).
    #[cfg(target_os = "macos")]
    #[test]
    fn videotoolbox_hwaccel_native_and_cpu_fallback() {
        let aus = split_access_units(FIXTURE);
        for native in [true, false] {
            let (sink, frames) = collecting_sink();
            let config = FfmpegConfig {
                hwaccel: Hwaccel::VideoToolbox,
                native_frames: native,
                complete_frames: true,
            };
            let mut decoder = FfmpegDecoder::new(config, sink);
            assert!(decoder.is_valid(), "{:?}", decoder.last_error());
            assert!(
                decoder.hwaccel_active(),
                "av_hwdevice_ctx_create(VIDEOTOOLBOX) must succeed on macOS"
            );
            assert_eq!(decoder.native_frames(), native);
            let results = decode_aus(&mut decoder, &aus[..30]);
            assert!(results.iter().all(Result::is_ok), "{results:?}");
            decoder.flush();
            let frames = frames.lock().unwrap();
            assert!(
                frames.len() >= 28,
                "native={native}: {} frames (hardware decoders may hold a few back)",
                frames.len()
            );
            for frame in frames.iter() {
                assert_eq!((frame.width, frame.height), (1920, 1080));
                if native {
                    let buffer = frame
                        .pixel_buffer()
                        .expect("expected a VideoToolbox pixel buffer");
                    assert_eq!(CVPixelBufferGetWidth(buffer), 1920);
                    // SAFETY: `buffer` is a live pixel buffer borrowed from the frame; the
                    // call only reads it and returns a pointer we do not dereference.
                    assert!(
                        !unsafe { CVPixelBufferGetIOSurface(buffer) }.is_null(),
                        "the zero-copy GPU import needs an IOSurface-backed buffer"
                    );
                    let NativeBuffer::Nv12Cpu { y, .. } = frame.to_nv12_cpu().unwrap() else {
                        panic!("readback produces CPU planes");
                    };
                    assert!(y.iter().any(|&v| v > 64));
                } else {
                    assert_nv12_1080p(frame);
                }
            }
        }
    }

    /// The VAAPI path on Linux on the 1080p fixture: frames come back as CPU NV12 after
    /// `av_hwframe_transfer_data`. Skips when the machine has no usable VAAPI device.
    #[cfg(target_os = "linux")]
    #[test]
    fn vaapi_hwaccel_downloads_nv12() {
        let aus = split_access_units(FIXTURE);
        let (sink, frames) = collecting_sink();
        let config = FfmpegConfig {
            hwaccel: Hwaccel::Vaapi,
            native_frames: true,
            complete_frames: true,
        };
        let mut decoder = FfmpegDecoder::new(config, sink);
        assert!(decoder.is_valid(), "{:?}", decoder.last_error());
        if !decoder.hwaccel_active() {
            eprintln!("skipping: no VAAPI device");
            return;
        }
        let results = decode_aus(&mut decoder, &aus[..30]);
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        decoder.flush();
        let frames = frames.lock().unwrap();
        assert!(
            frames.len() >= 28,
            "{} frames (hardware decoders may hold a few back)",
            frames.len()
        );
        for frame in frames.iter() {
            assert_nv12_1080p(frame);
        }
    }

    /// Port of TestDecoders::hardwareFramesSurviveResetAndCanFallBack, literally: a 4-frame
    /// 2560x1440 `testsrc2` stream from the ffmpeg CLI (QSKIP without it), `H264Decoder(true)`
    /// fed the whole byte stream in one `decode()` (`complete_frames = false`), a retained
    /// VideoToolbox pixel buffer of the right size, then on the *same* decoder: `reset()`,
    /// `nativeVideoEnabled = false` ([`FfmpegDecoder::set_native_frames`]), a CPU image of
    /// the right size and no native frame. The retained buffer outlives the decoder and is
    /// still 2560 wide and IOSurface-backed after the decoder has been destroyed.
    #[cfg(target_os = "macos")]
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
            // `H264Decoder decoder(true)`: hardware, native frames, byte-stream parser.
            let config = FfmpegConfig {
                hwaccel: Hwaccel::VideoToolbox,
                native_frames: true,
                complete_frames: false,
            };
            let mut decoder = FfmpegDecoder::new(config, sink);
            assert!(decoder.is_valid(), "{:?}", decoder.last_error());
            assert!(
                decoder.hwaccel_active(),
                "Expected a VideoToolbox pixel buffer: av_hwdevice_ctx_create failed"
            );
            decoder.decode(&bytes, Instant::now()).unwrap();
            assert_eq!(decoder.dimensions(), Some((2560, 1440)));
            // `takeNativeFrame()` returns the most recent frame of the call.
            retained = frames
                .lock()
                .unwrap()
                .pop()
                .expect("Expected a VideoToolbox pixel buffer");
            assert!(
                retained.pixel_buffer().is_some(),
                "Expected a VideoToolbox pixel buffer"
            );
            assert_eq!((retained.width, retained.height), (2560, 1440));

            decoder.reset();
            decoder.set_native_frames(false);
            frames.lock().unwrap().clear();
            decoder.decode(&bytes, Instant::now()).unwrap();
            decoder.set_native_frames(true);
            let fallback = frames.lock().unwrap();
            assert!(!fallback.is_empty(), "fallback must produce a CPU image");
            for frame in fallback.iter() {
                assert_eq!((frame.width, frame.height), (2560, 1440));
                // `QVERIFY(decoder.takeNativeFrame().isNull())`.
                assert!(
                    frame.pixel_buffer().is_none(),
                    "no native frame in fallback mode"
                );
                let (y, uv, y_stride, uv_stride) = frame.nv12_planes().expect("CPU NV12 planes");
                assert_eq!((y_stride, uv_stride), (2560, 2560));
                assert_eq!(y.len(), 2560 * 1440);
                assert_eq!(uv.len(), 2560 * 720);
            }
        }
        // The decoder (and its hw_frames_ctx pool) is gone; the retained buffer is not.
        let buffer = retained.pixel_buffer().unwrap();
        assert_eq!(CVPixelBufferGetWidth(buffer), 2560);
        assert_eq!(CVPixelBufferGetHeight(buffer), 1440);
        // SAFETY: `buffer` is a live pixel buffer borrowed from the frame; the call only
        // reads it and returns a pointer we do not dereference.
        assert!(!unsafe { CVPixelBufferGetIOSurface(buffer) }.is_null());
    }
}
