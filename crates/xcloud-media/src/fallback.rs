//! Runtime fallback from one [`VideoDecoder`] to another: the last bullet of
//! docs/archive/PORTING_PLAN.md section 5.2, "FFmpeg as the runtime fallback when VideoToolbox cannot
//! decode".
//!
//! The Qt client had two such safety nets. `qt/src/streaming/H264Decoder.cpp` let FFmpeg
//! fall back from the VideoToolbox hwaccel to software decoding inside one codec context,
//! and `qt/src/app/MetalVideoItem.mm` dropped from the native Metal node to CPU image
//! output ("Native rendering unavailable; reverting to CPU image output"). The Rust
//! VideoToolbox backend talks to `VTDecompressionSession` directly, so the FFmpeg fallback
//! has to happen one level up: [`FallbackDecoder`] owns the primary backend and a factory
//! for the secondary one, forwards `decode`/`flush`, and switches **once**, for the rest
//! of its life, when the primary
//!
//! * returns [`Error::Unavailable`] (`VTDecompressionSessionCreate` said there is no
//!   decoder for the stream: `kVTCouldNotFindVideoDecoderErr`,
//!   `kVTVideoDecoderUnsupportedDataFormatErr`, ...),
//! * returns [`Error::Os`] from *session creation* (`VTDecompressionSessionCreate` or
//!   `CMVideoFormatDescriptionCreateFromH264ParameterSets` failed with a status the
//!   backend does not map: there is no session for this stream, so nothing will ever
//!   decode; see [`SESSION_CREATION_CALLS`]),
//! * returns [`Error::InvalidSession`] for [`FallbackConfig::invalid_session_limit`]
//!   consecutive access units (the backend keeps recreating a session the system keeps
//!   invalidating), or
//! * accepts [`FallbackConfig::frameless_idr_limit`] consecutive IDR access units
//!   without delivering a single frame to the sink (a decoder that takes the data and
//!   silently produces nothing).
//!
//! [`Error::BadData`] and [`Error::WaitingForParameterSets`] never trigger a switch: they
//! are the keyframe-request path and the secondary would reject the same data. The same
//! goes for an [`Error::Os`] from a per-frame call (`VTDecompressionSessionDecodeFrame`,
//! `CMBlockBufferCreateWithMemoryBlock`, ...): a transient
//! `kVTVideoDecoderMalfunctionErr` or `kVTVideoDecoderNotAvailableNowErr` on one access
//! unit is reported as-is and the caller requests a keyframe, exactly as the Qt client
//! skipped a packet `avcodec_send_packet` rejected without ever touching the hardware /
//! software choice it made once at context creation (`H264Decoder.cpp`).
//!
//! On a switch the access unit that exposed the problem is resubmitted to the secondary,
//! so no data is lost beyond what the primary already swallowed. The frameless-IDR check
//! runs when the *next* IDR arrives, because the secondary needs a keyframe to start from
//! and because the primary's output is asynchronous: frames of IDR *n* may still be in
//! flight when `decode` returns. Only IDRs the primary *accepted* count: an IDR it
//! rejected (`BadData`, ...) is skipped like the Qt `continue`, and neither adds to nor
//! resets the frameless run.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::Mutex;

use crate::annexb;
use crate::decoder::{FrameSink, VideoDecoder};
use crate::{Error, Result};

const LOG: &str = "Video";

/// The `what` of an [`Error::Os`] that means "no decoder session could be created for
/// this stream": the only `Os` errors that switch backends. Every other `Os` error comes
/// from a per-access-unit call and is reported to the caller unchanged. The names are the
/// ones `crate::vt` puts into [`Error::Os::what`] for its session-creation calls.
pub const SESSION_CREATION_CALLS: &[&str] = &[
    "VTDecompressionSessionCreate",
    "CMVideoFormatDescriptionCreateFromH264ParameterSets",
];

/// Builds a backend on demand, given the sink its frames must go to.
pub type BackendFactory = Box<dyn FnOnce(FrameSink) -> Box<dyn VideoDecoder> + Send>;

/// Which backend a [`FallbackDecoder`] currently forwards to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// The decoder the stream started with (VideoToolbox in the app).
    Primary,
    /// The fallback (FFmpeg in the app); active until the decoder is dropped.
    Secondary,
}

impl Backend {
    const fn from_u8(value: u8) -> Self {
        if value == 0 {
            Self::Primary
        } else {
            Self::Secondary
        }
    }
}

/// Why a [`FallbackDecoder`] switched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwitchReason {
    /// The primary returned [`Error::Unavailable`].
    Unavailable(String),
    /// The primary returned [`Error::Os`] from session creation (one of
    /// [`SESSION_CREATION_CALLS`]).
    Os { what: &'static str, status: i32 },
    /// The primary returned [`Error::InvalidSession`] this many times in a row.
    InvalidSessions(u32),
    /// The primary accepted this many consecutive IDR access units without a frame.
    FramelessIdrs(u32),
}

impl fmt::Display for SwitchReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(what) => write!(f, "no decoder available: {what}"),
            Self::Os { what, status } => write!(f, "{what} failed with OSStatus {status}"),
            Self::InvalidSessions(n) => write!(f, "session invalidated {n} times in a row"),
            Self::FramelessIdrs(n) => write!(f, "{n} consecutive keyframes produced no frame"),
        }
    }
}

/// Tuning of the switch heuristics and the names used in the log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FallbackConfig {
    /// Switch after this many consecutive accepted IDR access units without any frame
    /// reaching the sink (`N` in PORTING_PLAN 5.2). `0` disables the check.
    pub frameless_idr_limit: u32,
    /// Switch after this many consecutive [`Error::InvalidSession`] results. `0` disables
    /// the check.
    pub invalid_session_limit: u32,
    /// Name of the primary backend in the log line.
    pub primary_name: &'static str,
    /// Name of the secondary backend in the log line.
    pub secondary_name: &'static str,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self {
            frameless_idr_limit: 3,
            invalid_session_limit: 3,
            primary_name: "primary video decoder",
            secondary_name: "fallback video decoder",
        }
    }
}

impl FallbackConfig {
    /// Names the backends for the log line.
    pub fn with_names(mut self, primary: &'static str, secondary: &'static str) -> Self {
        self.primary_name = primary;
        self.secondary_name = secondary;
        self
    }
}

/// Shared, thread-safe view of a [`FallbackDecoder`]'s state for the HUD: the decoder
/// itself moves to the decode thread, this handle stays with the UI.
#[derive(Clone, Debug)]
pub struct FallbackStatus {
    backend: Arc<AtomicU8>,
}

impl FallbackStatus {
    /// The backend currently decoding.
    pub fn active(&self) -> Backend {
        Backend::from_u8(self.backend.load(Ordering::Acquire))
    }

    /// `true` once the secondary took over.
    pub fn switched(&self) -> bool {
        self.active() == Backend::Secondary
    }
}

/// A [`VideoDecoder`] that forwards to a primary backend and switches to a lazily built
/// secondary one when the primary cannot decode. See the module documentation for the
/// switch conditions.
pub struct FallbackDecoder {
    active: Box<dyn VideoDecoder>,
    /// `None` once consumed (after the switch).
    secondary: Option<BackendFactory>,
    sink: Arc<Mutex<FrameSink>>,
    /// Frames delivered to the sink by either backend.
    frames: Arc<AtomicU64>,
    backend: Arc<AtomicU8>,
    config: FallbackConfig,
    reason: Option<SwitchReason>,
    /// `frames` right after the last IDR the primary accepted; `None` before the first.
    last_idr_frames: Option<u64>,
    /// Consecutive accepted IDRs whose interval since the previous accepted IDR delivered
    /// no frame. Updated only when the primary accepts an IDR.
    frameless_idrs: u32,
    invalid_sessions: u32,
}

impl FallbackDecoder {
    /// Builds the primary with `primary` now and keeps `secondary` for the switch. Both
    /// receive a sink that forwards to `sink` and counts frames for the IDR check.
    pub fn new(
        sink: FrameSink,
        primary: impl FnOnce(FrameSink) -> Box<dyn VideoDecoder>,
        secondary: impl FnOnce(FrameSink) -> Box<dyn VideoDecoder> + Send + 'static,
    ) -> Self {
        Self::with_config(FallbackConfig::default(), sink, primary, secondary)
    }

    /// [`new`](Self::new) with explicit thresholds and names.
    pub fn with_config(
        config: FallbackConfig,
        sink: FrameSink,
        primary: impl FnOnce(FrameSink) -> Box<dyn VideoDecoder>,
        secondary: impl FnOnce(FrameSink) -> Box<dyn VideoDecoder> + Send + 'static,
    ) -> Self {
        let sink = Arc::new(Mutex::new(sink));
        let frames = Arc::new(AtomicU64::new(0));
        let active = primary(counting_sink(&sink, &frames));
        Self {
            active,
            secondary: Some(Box::new(secondary)),
            sink,
            frames,
            backend: Arc::new(AtomicU8::new(0)),
            config,
            reason: None,
            last_idr_frames: None,
            frameless_idrs: 0,
            invalid_sessions: 0,
        }
    }

    /// The backend currently decoding.
    pub fn active(&self) -> Backend {
        Backend::from_u8(self.backend.load(Ordering::Acquire))
    }

    /// `true` once the secondary took over. There is no way back.
    pub fn switched(&self) -> bool {
        self.active() == Backend::Secondary
    }

    /// Why the secondary took over, if it did.
    pub fn switch_reason(&self) -> Option<&SwitchReason> {
        self.reason.as_ref()
    }

    /// Frames delivered to the sink so far, by either backend.
    pub fn frames_output(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// A handle for the HUD that stays valid after the decoder moved to its thread.
    pub fn status(&self) -> FallbackStatus {
        FallbackStatus {
            backend: Arc::clone(&self.backend),
        }
    }

    /// `true` when an IDR arriving now closes a frameless interval: the primary accepted
    /// an IDR before and has delivered nothing since. `false` before the first accepted
    /// IDR.
    fn idr_interval_is_frameless(&self) -> bool {
        self.last_idr_frames
            .is_some_and(|previous| self.frames.load(Ordering::Acquire) == previous)
    }

    /// Runs on every IDR before it is submitted to the primary: returns the switch reason
    /// when accepting this IDR would be the `limit`-th consecutive frameless one. Nothing
    /// is counted here; the run is committed in [`Self::idr_accepted`] once the primary
    /// took the IDR, so an IDR it rejects neither adds to nor resets the run.
    fn frameless_idr_check(&self, frameless: bool) -> Option<SwitchReason> {
        let limit = self.config.frameless_idr_limit;
        (limit > 0 && frameless && self.frameless_idrs + 1 >= limit)
            .then_some(SwitchReason::FramelessIdrs(limit))
    }

    /// The primary accepted an IDR: commits the frameless run and starts a new interval.
    fn idr_accepted(&mut self, frameless: bool) {
        self.frameless_idrs = if frameless {
            self.frameless_idrs + 1
        } else {
            0
        };
        self.last_idr_frames = Some(self.frames.load(Ordering::Acquire));
    }

    /// Replaces the primary by the secondary and resubmits `au` to it.
    fn switch(&mut self, reason: SwitchReason, au: &[u8], t_received: Instant) -> Result<()> {
        let Some(factory) = self.secondary.take() else {
            // Cannot happen: the secondary is only taken here, and afterwards `decode`
            // never calls `switch` again. Report rather than panic.
            return Err(Error::Unavailable(reason.to_string()));
        };
        tracing::warn!(
            target: LOG,
            reason = %reason,
            "{} decoding unavailable; reverting to {} for the rest of the stream",
            self.config.primary_name,
            self.config.secondary_name
        );
        // Drain what the primary still owes the sink (the Qt `reset()`), then let it go
        // before the secondary is built: VideoToolbox sessions are not free.
        self.active.flush();
        self.active = factory(counting_sink(&self.sink, &self.frames));
        self.backend.store(1, Ordering::Release);
        self.reason = Some(reason);
        self.active.decode(au, t_received)
    }
}

impl VideoDecoder for FallbackDecoder {
    fn decode(&mut self, au: &[u8], t_received: Instant) -> Result<()> {
        if self.switched() {
            return self.active.decode(au, t_received);
        }
        let idr = annexb::is_idr(au);
        let frameless = idr && self.idr_interval_is_frameless();
        if idr && let Some(reason) = self.frameless_idr_check(frameless) {
            return self.switch(reason, au, t_received);
        }
        match self.active.decode(au, t_received) {
            Ok(()) => {
                self.invalid_sessions = 0;
                if idr {
                    self.idr_accepted(frameless);
                }
                Ok(())
            }
            Err(Error::InvalidSession) => {
                self.invalid_sessions += 1;
                let limit = self.config.invalid_session_limit;
                if limit > 0 && self.invalid_sessions >= limit {
                    self.switch(SwitchReason::InvalidSessions(limit), au, t_received)
                } else {
                    Err(Error::InvalidSession)
                }
            }
            Err(Error::Unavailable(what)) => {
                self.switch(SwitchReason::Unavailable(what), au, t_received)
            }
            // No session for this stream: nothing will ever decode on the primary.
            Err(Error::Os { what, status }) if SESSION_CREATION_CALLS.contains(&what) => {
                self.switch(SwitchReason::Os { what, status }, au, t_received)
            }
            // BadData / WaitingForParameterSets / InvalidSps / Unsupported, and an `Os`
            // error from a per-frame call (`VTDecompressionSessionDecodeFrame`, buffer
            // creation): the caller requests a keyframe; the secondary would reject the
            // same data, and a transient decoder malfunction is no reason to give up
            // hardware decoding for the rest of the stream.
            Err(other) => {
                self.invalid_sessions = 0;
                Err(other)
            }
        }
    }

    fn flush(&mut self) {
        self.active.flush();
        // A flush is the between-streams reset: the IDR bookkeeping starts over, but a
        // completed switch stays.
        self.last_idr_frames = None;
        self.frameless_idrs = 0;
        self.invalid_sessions = 0;
    }
}

/// A sink for one backend: counts the frame, then hands it to the shared sink.
fn counting_sink(sink: &Arc<Mutex<FrameSink>>, frames: &Arc<AtomicU64>) -> FrameSink {
    let sink = Arc::clone(sink);
    let frames = Arc::clone(frames);
    Box::new(move |frame| {
        frames.fetch_add(1, Ordering::AcqRel);
        (sink.lock())(frame);
    })
}

// The decoder moves to the decode thread; the status handle stays with the UI.
const _: () = {
    const fn assert_send<T: Send>() {}
    const fn assert_sync<T: Sync>() {}
    assert_send::<FallbackDecoder>();
    assert_send::<FallbackStatus>();
    assert_sync::<FallbackStatus>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{ColorInfo, Frame, NativeBuffer};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU32};

    /// An IDR access unit: SPS, PPS and an IDR slice (NAL types 7, 8, 5).
    const IDR: &[u8] = &[
        0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88, 0x84,
    ];
    /// A non-IDR slice (NAL type 1).
    const P: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a, 0x02];

    fn frame() -> Frame {
        let now = Instant::now();
        Frame {
            buf: NativeBuffer::Nv12Cpu {
                y: vec![0; 4],
                uv: vec![0; 2],
                y_stride: 2,
                uv_stride: 2,
            },
            width: 2,
            height: 2,
            color: ColorInfo::default(),
            t_received: now,
            t_submitted: now,
            t_decoded: now,
        }
    }

    /// What a scripted decode call does.
    enum Step {
        /// Accept the access unit and emit this many frames.
        Frames(usize),
        /// Reject it with this error.
        Fail(Error),
    }

    /// Shared view of a fake backend, kept by the test after the backend moved into the
    /// [`FallbackDecoder`].
    #[derive(Default)]
    struct Probe {
        aus: Mutex<Vec<Vec<u8>>>,
        flushes: AtomicU32,
        built: AtomicBool,
    }

    impl Probe {
        fn aus(&self) -> Vec<Vec<u8>> {
            self.aus.lock().clone()
        }
        fn decodes(&self) -> usize {
            self.aus.lock().len()
        }
        fn flushes(&self) -> u32 {
            self.flushes.load(Ordering::SeqCst)
        }
        fn built(&self) -> bool {
            self.built.load(Ordering::SeqCst)
        }
    }

    /// Fails like VideoToolbox would: plays `script` step by step, then behaves like
    /// `fallthrough` for every further access unit.
    struct VtLike {
        sink: FrameSink,
        probe: Arc<Probe>,
        script: VecDeque<Step>,
        fallthrough: fn() -> Step,
    }

    impl VideoDecoder for VtLike {
        fn decode(&mut self, au: &[u8], _t_received: Instant) -> Result<()> {
            self.probe.aus.lock().push(au.to_vec());
            let step = self
                .script
                .pop_front()
                .unwrap_or_else(|| (self.fallthrough)());
            match step {
                Step::Frames(n) => {
                    for _ in 0..n {
                        (self.sink)(frame());
                    }
                    Ok(())
                }
                Step::Fail(err) => Err(err),
            }
        }

        fn flush(&mut self) {
            self.probe.flushes.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Counts: records every access unit, emits one frame each, counts flushes.
    struct Counting {
        sink: FrameSink,
        probe: Arc<Probe>,
    }

    impl VideoDecoder for Counting {
        fn decode(&mut self, au: &[u8], _t_received: Instant) -> Result<()> {
            self.probe.aus.lock().push(au.to_vec());
            (self.sink)(frame());
            Ok(())
        }

        fn flush(&mut self) {
            self.probe.flushes.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct Harness {
        decoder: FallbackDecoder,
        primary: Arc<Probe>,
        secondary: Arc<Probe>,
        /// Frames the outer sink received.
        frames: Arc<AtomicU32>,
    }

    fn harness(config: FallbackConfig, script: Vec<Step>, fallthrough: fn() -> Step) -> Harness {
        let primary = Arc::new(Probe::default());
        let secondary = Arc::new(Probe::default());
        let frames = Arc::new(AtomicU32::new(0));
        let sink_frames = Arc::clone(&frames);
        let sink: FrameSink = Box::new(move |_frame| {
            sink_frames.fetch_add(1, Ordering::SeqCst);
        });
        let primary_probe = Arc::clone(&primary);
        let secondary_probe = Arc::clone(&secondary);
        let decoder = FallbackDecoder::with_config(
            config,
            sink,
            move |sink| {
                primary_probe.built.store(true, Ordering::SeqCst);
                Box::new(VtLike {
                    sink,
                    probe: primary_probe,
                    script: script.into(),
                    fallthrough,
                })
            },
            move |sink| {
                secondary_probe.built.store(true, Ordering::SeqCst);
                Box::new(Counting {
                    sink,
                    probe: secondary_probe,
                })
            },
        );
        Harness {
            decoder,
            primary,
            secondary,
            frames,
        }
    }

    fn decode(h: &mut Harness, au: &[u8]) -> Result<()> {
        h.decoder.decode(au, Instant::now())
    }

    fn one_frame() -> Step {
        Step::Frames(1)
    }
    fn no_frame() -> Step {
        Step::Frames(0)
    }
    fn bad_data() -> Step {
        Step::Fail(Error::BadData)
    }

    #[test]
    fn starts_on_primary_and_builds_secondary_lazily() {
        let h = harness(FallbackConfig::default(), vec![], one_frame);
        assert!(h.primary.built());
        assert!(
            !h.secondary.built(),
            "the secondary is built on the switch only"
        );
        assert_eq!(h.decoder.active(), Backend::Primary);
        assert!(!h.decoder.switched());
        assert!(h.decoder.switch_reason().is_none());
        assert_eq!(h.decoder.status().active(), Backend::Primary);
    }

    #[test]
    fn forwards_to_primary_and_counts_its_frames() {
        let mut h = harness(FallbackConfig::default(), vec![], one_frame);
        for _ in 0..5 {
            decode(&mut h, IDR).expect("accepted");
            decode(&mut h, P).expect("accepted");
        }
        assert_eq!(h.primary.decodes(), 10);
        assert_eq!(
            h.frames.load(Ordering::SeqCst),
            10,
            "frames reach the outer sink"
        );
        assert_eq!(h.decoder.frames_output(), 10);
        assert_eq!(h.decoder.active(), Backend::Primary);
        assert!(!h.secondary.built());
    }

    #[test]
    fn no_switch_on_bad_data() {
        // Far more rejections than any limit, on IDRs and P-frames alike.
        let mut h = harness(FallbackConfig::default(), vec![], bad_data);
        for _ in 0..10 {
            assert!(matches!(decode(&mut h, IDR), Err(Error::BadData)));
            assert!(matches!(decode(&mut h, P), Err(Error::BadData)));
        }
        assert_eq!(h.decoder.active(), Backend::Primary);
        assert!(!h.decoder.switched());
        assert!(!h.secondary.built());
        assert_eq!(h.primary.decodes(), 20);
    }

    #[test]
    fn no_switch_on_waiting_for_parameter_sets() {
        let mut h = harness(FallbackConfig::default(), vec![], || {
            Step::Fail(Error::WaitingForParameterSets)
        });
        for _ in 0..10 {
            assert!(matches!(
                decode(&mut h, IDR),
                Err(Error::WaitingForParameterSets)
            ));
        }
        assert_eq!(h.decoder.active(), Backend::Primary);
        assert!(!h.secondary.built());
    }

    #[test]
    fn switch_on_unavailable_resubmits_the_access_unit() {
        let mut h = harness(
            FallbackConfig::default(),
            vec![
                one_frame(),
                Step::Fail(Error::Unavailable(
                    "VTDecompressionSessionCreate returned -12906".into(),
                )),
            ],
            one_frame,
        );
        decode(&mut h, IDR).expect("primary decodes the first IDR");
        assert_eq!(h.frames.load(Ordering::SeqCst), 1);

        // The second IDR hits the error: the caller sees success from the secondary.
        decode(&mut h, IDR).expect("the secondary takes the resubmitted access unit");
        assert!(h.decoder.switched());
        assert_eq!(h.decoder.active(), Backend::Secondary);
        assert_eq!(h.decoder.status().active(), Backend::Secondary);
        assert!(h.decoder.status().switched());
        assert_eq!(
            h.decoder.switch_reason(),
            Some(&SwitchReason::Unavailable(
                "VTDecompressionSessionCreate returned -12906".into()
            ))
        );
        assert!(h.secondary.built());
        assert_eq!(
            h.secondary.aus(),
            vec![IDR.to_vec()],
            "same bytes, resubmitted"
        );
        assert_eq!(
            h.frames.load(Ordering::SeqCst),
            2,
            "the secondary's frame arrived"
        );

        // From now on everything goes to the secondary; the primary is gone.
        decode(&mut h, P).expect("secondary");
        decode(&mut h, IDR).expect("secondary");
        assert_eq!(h.secondary.decodes(), 3);
        assert_eq!(
            h.primary.decodes(),
            2,
            "the primary saw nothing after the switch"
        );
        assert_eq!(h.frames.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn switch_on_os_error_from_session_creation() {
        let mut h = harness(
            FallbackConfig::default(),
            vec![Step::Fail(Error::Os {
                what: "VTDecompressionSessionCreate",
                status: -12_911,
            })],
            one_frame,
        );
        decode(&mut h, P).expect("secondary accepts the resubmitted access unit");
        assert!(h.decoder.switched());
        assert_eq!(
            h.decoder.switch_reason(),
            Some(&SwitchReason::Os {
                what: "VTDecompressionSessionCreate",
                status: -12_911,
            })
        );
        assert_eq!(h.secondary.aus(), vec![P.to_vec()]);
    }

    #[test]
    fn switch_on_os_error_from_format_description_creation() {
        let mut h = harness(
            FallbackConfig::default(),
            vec![Step::Fail(Error::Os {
                what: "CMVideoFormatDescriptionCreateFromH264ParameterSets",
                status: -12_712,
            })],
            one_frame,
        );
        decode(&mut h, IDR).expect("secondary accepts the resubmitted access unit");
        assert!(h.decoder.switched());
        assert_eq!(
            h.decoder.switch_reason(),
            Some(&SwitchReason::Os {
                what: "CMVideoFormatDescriptionCreateFromH264ParameterSets",
                status: -12_712,
            })
        );
    }

    #[test]
    fn no_switch_on_os_error_from_decode_frame() {
        // A transient kVTVideoDecoderMalfunctionErr (-12911) on one access unit, then a
        // kVTVideoDecoderNotAvailableNowErr (-12913) while the hardware is busy, then a
        // failed CMBlockBuffer allocation: all per-frame, all reported to the caller, none
        // demotes the stream to the secondary.
        let decode_frame = || {
            Step::Fail(Error::Os {
                what: "VTDecompressionSessionDecodeFrame",
                status: -12_911,
            })
        };
        let mut h = harness(
            FallbackConfig::default(),
            vec![
                one_frame(),
                decode_frame(),
                Step::Fail(Error::Os {
                    what: "VTDecompressionSessionDecodeFrame",
                    status: -12_913,
                }),
                Step::Fail(Error::Os {
                    what: "CMBlockBufferCreateWithMemoryBlock",
                    status: -12_780,
                }),
            ],
            decode_frame,
        );
        decode(&mut h, IDR).expect("primary decodes the first IDR");
        for _ in 0..10 {
            let result = decode(&mut h, P);
            assert!(
                matches!(result, Err(Error::Os { status, .. }) if status != 0),
                "the per-frame error is reported as-is: {result:?}"
            );
            assert!(!h.decoder.switched());
        }
        assert_eq!(h.decoder.active(), Backend::Primary);
        assert!(h.decoder.switch_reason().is_none());
        assert!(!h.secondary.built());
        assert_eq!(
            h.primary.decodes(),
            11,
            "every access unit reached the primary"
        );

        // The primary recovers after the keyframe request: still the primary.
        let mut h = harness(
            FallbackConfig::default(),
            vec![one_frame(), decode_frame()],
            one_frame,
        );
        decode(&mut h, IDR).expect("primary");
        assert!(matches!(decode(&mut h, P), Err(Error::Os { .. })));
        decode(&mut h, IDR).expect("primary again");
        assert!(!h.decoder.switched());
        assert_eq!(h.frames.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn decode_frame_os_error_resets_the_invalid_session_run() {
        // Like BadData, a per-frame Os error breaks a run of InvalidSession results.
        let mut h = harness(
            FallbackConfig::default(),
            vec![
                Step::Fail(Error::InvalidSession),
                Step::Fail(Error::InvalidSession),
                Step::Fail(Error::Os {
                    what: "VTDecompressionSessionDecodeFrame",
                    status: -12_911,
                }),
                Step::Fail(Error::InvalidSession),
                Step::Fail(Error::InvalidSession),
            ],
            one_frame,
        );
        for _ in 0..5 {
            let _ = decode(&mut h, P);
        }
        assert!(!h.decoder.switched(), "never three InvalidSession in a row");
        assert!(!h.secondary.built());
    }

    #[test]
    fn repeated_invalid_session_switches() {
        let mut h = harness(FallbackConfig::default(), vec![], || {
            Step::Fail(Error::InvalidSession)
        });
        // The first N-1 are reported as what they are (the caller requests a keyframe).
        assert!(matches!(decode(&mut h, IDR), Err(Error::InvalidSession)));
        assert!(matches!(decode(&mut h, P), Err(Error::InvalidSession)));
        assert!(!h.decoder.switched());
        // The N-th in a row switches and the access unit goes to the secondary.
        decode(&mut h, P).expect("secondary");
        assert!(h.decoder.switched());
        assert_eq!(
            h.decoder.switch_reason(),
            Some(&SwitchReason::InvalidSessions(3))
        );
        assert_eq!(h.secondary.aus(), vec![P.to_vec()]);
        assert_eq!(h.primary.decodes(), 3);
    }

    #[test]
    fn invalid_session_count_resets_on_success() {
        let mut h = harness(
            FallbackConfig::default(),
            vec![
                Step::Fail(Error::InvalidSession),
                Step::Fail(Error::InvalidSession),
                one_frame(),
                Step::Fail(Error::InvalidSession),
                Step::Fail(Error::InvalidSession),
            ],
            one_frame,
        );
        for _ in 0..5 {
            let _ = decode(&mut h, P);
        }
        assert!(!h.decoder.switched(), "never three in a row");
        assert!(!h.secondary.built());
    }

    #[test]
    fn switch_after_n_frameless_idrs() {
        let mut h = harness(FallbackConfig::default(), vec![], no_frame);
        // Three IDRs (with P-frames in between) that the primary accepts but never turns
        // into a frame.
        for _ in 0..3 {
            decode(&mut h, IDR).expect("accepted");
            decode(&mut h, P).expect("accepted");
            assert!(!h.decoder.switched());
        }
        assert_eq!(h.frames.load(Ordering::SeqCst), 0);
        assert!(!h.secondary.built());

        // The fourth IDR is the first chance to seed the secondary with a keyframe.
        decode(&mut h, IDR).expect("secondary");
        assert!(h.decoder.switched());
        assert_eq!(
            h.decoder.switch_reason(),
            Some(&SwitchReason::FramelessIdrs(3))
        );
        assert_eq!(
            h.secondary.aus(),
            vec![IDR.to_vec()],
            "the IDR was resubmitted"
        );
        assert_eq!(
            h.primary.decodes(),
            6,
            "the fourth IDR never reached the primary"
        );
        assert_eq!(h.frames.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn frames_between_idrs_prevent_the_frameless_switch() {
        // Frames arrive late (with the P-frames), never on the IDR itself: still fine.
        let script = (0..10)
            .flat_map(|_| [Step::Frames(0), Step::Frames(1)])
            .collect();
        let mut h = harness(FallbackConfig::default(), script, no_frame);
        for _ in 0..10 {
            decode(&mut h, IDR).expect("accepted, no frame yet");
            decode(&mut h, P).expect("accepted, the frame arrives now");
        }
        assert_eq!(h.frames.load(Ordering::SeqCst), 10);
        assert!(!h.decoder.switched());
        assert!(!h.secondary.built());
    }

    #[test]
    fn frameless_idr_limit_is_configurable() {
        let config = FallbackConfig {
            frameless_idr_limit: 1,
            ..FallbackConfig::default()
        };
        let mut h = harness(config, vec![], no_frame);
        decode(&mut h, IDR).expect("accepted");
        assert!(!h.decoder.switched());
        decode(&mut h, IDR).expect("secondary");
        assert!(h.decoder.switched());
        assert_eq!(
            h.decoder.switch_reason(),
            Some(&SwitchReason::FramelessIdrs(1))
        );

        // 0 disables the check entirely.
        let config = FallbackConfig {
            frameless_idr_limit: 0,
            ..FallbackConfig::default()
        };
        let mut h = harness(config, vec![], no_frame);
        for _ in 0..20 {
            decode(&mut h, IDR).expect("accepted");
        }
        assert!(!h.decoder.switched());
    }

    #[test]
    fn rejected_idrs_do_not_count_as_frameless() {
        // The primary rejects every IDR (BadData) and accepts P-frames without output: a
        // keyframe-request situation, not a decoder that silently drops keyframes.
        let script = (0..10)
            .flat_map(|_| [bad_data(), Step::Frames(0)])
            .collect();
        let mut h = harness(FallbackConfig::default(), script, no_frame);
        for _ in 0..10 {
            assert!(matches!(decode(&mut h, IDR), Err(Error::BadData)));
            decode(&mut h, P).expect("accepted");
        }
        assert!(!h.decoder.switched());
        assert!(!h.secondary.built());
    }

    #[test]
    fn rejected_idrs_after_an_accepted_one_do_not_count_as_frameless() {
        // One IDR accepted without output, then the primary rejects every IDR (BadData)
        // while accepting P-frames without output. Only *accepted* IDRs count, so the
        // rejected ones must neither add to the run (the reviewer's scenario: three
        // rejected IDRs used to switch the fourth) nor switch on their own.
        let script = std::iter::once(Step::Frames(0))
            .chain((0..10).flat_map(|_| [bad_data(), Step::Frames(0)]))
            .collect();
        let mut h = harness(FallbackConfig::default(), script, no_frame);
        decode(&mut h, IDR).expect("the first IDR is accepted");
        for _ in 0..10 {
            assert!(matches!(decode(&mut h, IDR), Err(Error::BadData)));
            assert!(!h.decoder.switched());
            decode(&mut h, P).expect("accepted");
        }
        assert!(!h.decoder.switched());
        assert!(h.decoder.switch_reason().is_none());
        assert!(!h.secondary.built());
        assert_eq!(
            h.primary.decodes(),
            21,
            "every access unit reached the primary"
        );
    }

    #[test]
    fn rejected_idrs_do_not_reset_the_frameless_run() {
        // Accepted frameless IDRs interleaved with rejected ones: the rejected IDRs are
        // skipped (the Qt `continue`), the accepted ones still add up to the limit.
        let script = vec![
            Step::Frames(0), // IDR 1 accepted
            bad_data(),      // IDR rejected
            Step::Frames(0), // IDR 2 accepted: run = 1
            bad_data(),      // IDR rejected
            Step::Frames(0), // IDR 3 accepted: run = 2
        ];
        let mut h = harness(FallbackConfig::default(), script, no_frame);
        decode(&mut h, IDR).expect("accepted");
        for _ in 0..2 {
            assert!(matches!(decode(&mut h, IDR), Err(Error::BadData)));
            assert!(!h.decoder.switched());
            decode(&mut h, IDR).expect("accepted");
            assert!(!h.decoder.switched());
        }
        // The next IDR would be the third accepted frameless one in a row: switch.
        decode(&mut h, IDR).expect("secondary");
        assert!(h.decoder.switched());
        assert_eq!(
            h.decoder.switch_reason(),
            Some(&SwitchReason::FramelessIdrs(3))
        );
        assert_eq!(h.secondary.aus(), vec![IDR.to_vec()]);
        assert_eq!(
            h.primary.decodes(),
            5,
            "the switching IDR never reached the primary"
        );
    }

    #[test]
    fn flush_forwards_to_the_active_backend() {
        let mut h = harness(
            FallbackConfig::default(),
            vec![one_frame(), Step::Fail(Error::Unavailable("gone".into()))],
            one_frame,
        );
        h.decoder.flush();
        assert_eq!(h.primary.flushes(), 1);
        assert!(!h.secondary.built(), "flush does not build the secondary");

        decode(&mut h, IDR).expect("primary");
        decode(&mut h, IDR).expect("secondary");
        assert!(h.decoder.switched());
        // The switch itself drains the primary once (the Qt `reset()` before letting go).
        assert_eq!(h.primary.flushes(), 2);
        assert_eq!(h.secondary.flushes(), 0);

        h.decoder.flush();
        assert_eq!(h.secondary.flushes(), 1);
        assert_eq!(h.primary.flushes(), 2, "the primary is gone");
        assert!(h.decoder.switched(), "a flush never switches back");
    }

    #[test]
    fn flush_restarts_the_idr_bookkeeping() {
        let mut h = harness(FallbackConfig::default(), vec![], no_frame);
        decode(&mut h, IDR).expect("accepted");
        decode(&mut h, IDR).expect("accepted");
        h.decoder.flush();
        // Two more frameless IDRs after the reset: still under the limit of three.
        decode(&mut h, IDR).expect("accepted");
        decode(&mut h, IDR).expect("accepted");
        decode(&mut h, IDR).expect("accepted");
        assert!(!h.decoder.switched());
        decode(&mut h, IDR).expect("secondary");
        assert!(h.decoder.switched());
    }

    #[test]
    fn never_switches_back_even_when_the_secondary_fails() {
        let primary = Arc::new(Probe::default());
        let secondary = Arc::new(Probe::default());
        let builds = Arc::new(AtomicU32::new(0));
        let sink: FrameSink = Box::new(|_frame| {});
        let primary_probe = Arc::clone(&primary);
        let secondary_probe = Arc::clone(&secondary);
        let builds_probe = Arc::clone(&builds);
        let mut decoder = FallbackDecoder::new(
            sink,
            move |sink| {
                Box::new(VtLike {
                    sink,
                    probe: primary_probe,
                    script: VecDeque::new(),
                    fallthrough: || Step::Fail(Error::Unavailable("primary".into())),
                })
            },
            move |sink| {
                builds_probe.fetch_add(1, Ordering::SeqCst);
                // A secondary that fails exactly like the primary did.
                Box::new(VtLike {
                    sink,
                    probe: secondary_probe,
                    script: VecDeque::new(),
                    fallthrough: || Step::Fail(Error::Unavailable("secondary".into())),
                })
            },
        );
        let result = decoder.decode(P, Instant::now());
        assert!(
            matches!(result, Err(Error::Unavailable(ref what)) if what == "secondary"),
            "the secondary's own error is reported: {result:?}"
        );
        assert!(decoder.switched());
        for _ in 0..5 {
            let result = decoder.decode(IDR, Instant::now());
            assert!(matches!(result, Err(Error::Unavailable(ref what)) if what == "secondary"));
        }
        assert_eq!(builds.load(Ordering::SeqCst), 1, "the factory runs once");
        assert_eq!(primary.decodes(), 1);
        assert_eq!(secondary.decodes(), 6);
        assert_eq!(decoder.active(), Backend::Secondary);
    }

    #[test]
    fn status_handle_follows_the_switch_across_threads() {
        let mut h = harness(
            FallbackConfig::default(),
            vec![Step::Fail(Error::Unavailable("gone".into()))],
            one_frame,
        );
        let status = h.decoder.status();
        assert_eq!(status.active(), Backend::Primary);
        let worker = std::thread::spawn(move || {
            h.decoder.decode(IDR, Instant::now()).expect("secondary");
            h
        });
        let h = worker.join().expect("decode thread");
        assert!(status.switched());
        assert!(h.decoder.switched());
    }

    #[test]
    fn is_send() {
        fn assert_send<T: Send>(_: &T) {}
        let h = harness(FallbackConfig::default(), vec![], one_frame);
        assert_send(&h.decoder);
        assert_send(&h.decoder.status());
        let boxed: Box<dyn VideoDecoder> = Box::new(h.decoder);
        assert_send(&boxed);
    }

    #[test]
    fn switch_reason_display() {
        assert_eq!(
            SwitchReason::Unavailable("x".into()).to_string(),
            "no decoder available: x"
        );
        assert_eq!(
            SwitchReason::Os {
                what: "VTDecompressionSessionCreate",
                status: -12_911
            }
            .to_string(),
            "VTDecompressionSessionCreate failed with OSStatus -12911"
        );
        assert_eq!(
            SwitchReason::InvalidSessions(3).to_string(),
            "session invalidated 3 times in a row"
        );
        assert_eq!(
            SwitchReason::FramelessIdrs(3).to_string(),
            "3 consecutive keyframes produced no frame"
        );
    }
}
