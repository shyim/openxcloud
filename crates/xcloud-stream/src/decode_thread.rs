//! The video decode thread of docs/archive/PORTING_PLAN.md section 4: a `crossbeam` bounded(4) channel
//! of complete access units feeding a [`VideoDecoder`] that lives on its own `std` thread,
//! with decoded frames landing in a [`FrameMailbox`](xcloud_media::FrameMailbox) for the
//! render thread.
//!
//! In the Qt app (`qt/src/streaming/NativeMediaPipeline.cpp`) the H.264 decoder ran inline
//! on the transport's pump thread and `VideoFrameMailbox` hopped the frame to the GUI
//! thread. Here the pump thread only hands the access unit over and never waits: when the
//! decoder falls behind the channel is full, the access unit is dropped and the pipeline
//! gates on the next IDR exactly as it does after a jitter-buffer drop.
//!
//! Decoder feedback (bad data, decode latency, frame size) flows back to the pipeline
//! through shared atomics that the pipeline polls with [`DecodeThread::take_feedback`],
//! so the decode thread never calls into the pipeline either.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use parking_lot::Mutex;
use xcloud_media::{Error as MediaError, FrameSink, MailboxWriter, VideoDecoder};

/// Access units the pump thread may queue ahead of the decoder before dropping.
pub const QUEUE_DEPTH: usize = 4;

const LOG: &str = "Video";

/// Result of [`DecodeThread::submit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// The access unit was queued for the decoder.
    Accepted,
    /// The queue was momentarily full: the access unit was dropped and the caller must
    /// wait for the next IDR and request a keyframe.
    DroppedFull,
    /// The decode thread is gone for good (the decoder could not be constructed, its
    /// thread panicked or could not be spawned): the access unit was discarded and no
    /// keyframe can help, so the caller must not request one. This is the C++
    /// `H264Decoder::decode` returning a null image while `!m_valid`.
    Unavailable,
}

/// Decode timing over one stats window (the C++ `m_maxDecodeMs`, plus an average).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DecodeLatency {
    /// Frames the window is based on.
    pub samples: u32,
    pub avg_ms: f64,
    pub max_ms: f64,
}

impl DecodeLatency {
    /// Average and maximum of a window of `samples` measurements in microseconds (the
    /// average is 0 for an empty window). Also used by the pipeline for the jitter wait.
    pub fn from_window(samples: u32, sum_us: u64, max_us: u64) -> Self {
        let avg_ms = if samples == 0 {
            0.0
        } else {
            sum_us as f64 / 1000.0 / f64::from(samples)
        };
        Self {
            samples,
            avg_ms,
            max_ms: max_us as f64 / 1000.0,
        }
    }
}

/// Everything the decode thread reports back to the pipeline, accumulated since the last
/// [`DecodeThread::take_feedback`] call.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DecodeFeedback {
    /// The decoder rejected an access unit (`BadData`, missing parameter sets, or an
    /// invalidated session): request a keyframe and gate until the next IDR.
    pub want_keyframe: bool,
    /// Frames the decoder delivered to the mailbox in this window.
    pub frames: u32,
    /// Access units whose decode failed in this window.
    pub errors: u32,
    /// Time from `submit` to the decoder's output callback (`Frame::decode_latency`).
    pub decode: DecodeLatency,
    /// Time an access unit waited in the queue plus the synchronous `decode()` call.
    pub queue: DecodeLatency,
    /// Size of the latest decoded frame, when one arrived.
    pub resolution: Option<(u32, u32)>,
}

/// Window accumulator shared between the owning thread, the decoder's output callback and
/// the pipeline. Everything is an atomic so no path ever blocks on another.
#[derive(Default)]
struct Shared {
    generation: AtomicU32,
    want_keyframe: AtomicBool,
    frames: AtomicU32,
    errors: AtomicU32,
    decode_sum_us: AtomicU64,
    decode_max_us: AtomicU64,
    queue_count: AtomicU32,
    queue_sum_us: AtomicU64,
    queue_max_us: AtomicU64,
    /// `width << 32 | height`, 0 when no frame was seen yet.
    resolution: AtomicU64,
    disconnected_logged: AtomicBool,
}

impl Shared {
    fn record(sum: &AtomicU64, max: &AtomicU64, elapsed: Duration) {
        let us = elapsed.as_micros() as u64;
        sum.fetch_add(us, Ordering::Relaxed);
        max.fetch_max(us, Ordering::Relaxed);
    }

    fn take_feedback(&self) -> DecodeFeedback {
        let frames = self.frames.swap(0, Ordering::Relaxed);
        let queue_count = self.queue_count.swap(0, Ordering::Relaxed);
        let packed = self.resolution.load(Ordering::Relaxed);
        DecodeFeedback {
            want_keyframe: self.want_keyframe.swap(false, Ordering::AcqRel),
            frames,
            errors: self.errors.swap(0, Ordering::Relaxed),
            decode: DecodeLatency::from_window(
                frames,
                self.decode_sum_us.swap(0, Ordering::Relaxed),
                self.decode_max_us.swap(0, Ordering::Relaxed),
            ),
            queue: DecodeLatency::from_window(
                queue_count,
                self.queue_sum_us.swap(0, Ordering::Relaxed),
                self.queue_max_us.swap(0, Ordering::Relaxed),
            ),
            resolution: (packed != 0).then_some(((packed >> 32) as u32, packed as u32)),
        }
    }

    fn clear_window(&self) {
        self.take_feedback();
        self.resolution.store(0, Ordering::Relaxed);
    }
}

enum Msg {
    Au {
        data: Vec<u8>,
        t_received: Instant,
        generation: u32,
    },
    Reset,
}

/// Optional wake-up run after every decoded frame (`egui::Context::request_repaint`).
pub type FrameCallback = Box<dyn Fn() + Send + Sync>;

/// Handle to the decode thread. Dropping it closes the queue and joins the thread.
pub struct DecodeThread {
    tx: Option<Sender<Msg>>,
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

impl DecodeThread {
    /// Spawns the decode thread. `make_decoder` runs on the new thread and receives the
    /// [`FrameSink`] the decoder must deliver frames to; the sink writes into `mailbox`,
    /// records frame statistics and runs `on_frame`.
    pub fn spawn<D, F>(
        mailbox: MailboxWriter,
        on_frame: Option<FrameCallback>,
        make_decoder: F,
    ) -> Self
    where
        D: VideoDecoder + 'static,
        F: FnOnce(FrameSink) -> D + Send + 'static,
    {
        let (tx, rx) = crossbeam_channel::bounded::<Msg>(QUEUE_DEPTH);
        let shared = Arc::new(Shared::default());
        let mailbox = Arc::new(Mutex::new(mailbox));
        let thread_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("xcloud-video-decode".into())
            .spawn(move || {
                let sink = frame_sink(Arc::clone(&mailbox), Arc::clone(&thread_shared), on_frame);
                let decoder = make_decoder(sink);
                run(rx, decoder, mailbox, thread_shared);
            });
        let handle = match handle {
            Ok(handle) => Some(handle),
            Err(err) => {
                tracing::error!(target: LOG, error = %err, "failed to spawn the video decode thread");
                None
            }
        };
        Self {
            tx: Some(tx),
            shared,
            handle,
        }
    }

    /// Spawns the thread with the VideoToolbox decoder (`OPENXCLOUD_SOFTWARE_VIDEO` honoured).
    #[cfg(target_os = "macos")]
    pub fn with_videotoolbox(mailbox: MailboxWriter, on_frame: Option<FrameCallback>) -> Self {
        Self::spawn(
            mailbox,
            on_frame,
            xcloud_media::vt::VtDecoder::with_defaults,
        )
    }

    /// Queues one complete Annex-B access unit. Never blocks: a full queue drops the
    /// access unit and returns [`SubmitOutcome::DroppedFull`]; once the thread has exited
    /// every call returns [`SubmitOutcome::Unavailable`].
    pub fn submit(&self, au: &[u8], t_received: Instant) -> SubmitOutcome {
        let Some(tx) = self.tx.as_ref() else {
            return SubmitOutcome::Unavailable;
        };
        let msg = Msg::Au {
            data: au.to_vec(),
            t_received,
            generation: self.shared.generation.load(Ordering::Acquire),
        };
        match tx.try_send(msg) {
            Ok(()) => SubmitOutcome::Accepted,
            Err(TrySendError::Full(_)) => SubmitOutcome::DroppedFull,
            Err(TrySendError::Disconnected(_)) => {
                // The receiver lives in the thread closure: a failed spawn, a panicking
                // decoder constructor or a dead thread all end up here.
                if !self
                    .shared
                    .disconnected_logged
                    .swap(true, Ordering::Relaxed)
                {
                    tracing::error!(target: LOG, "video decode thread is gone; discarding access units");
                }
                SubmitOutcome::Unavailable
            }
        }
    }

    /// Discards queued access units, flushes the decoder and clears the mailbox so the
    /// render thread does not keep showing the previous stream. Never blocks: queued
    /// access units of the old stream are skipped by the thread, which performs the
    /// flush as soon as it sees the new generation.
    pub fn reset(&self) {
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        self.shared.clear_window();
        if let Some(tx) = self.tx.as_ref() {
            // A full queue is fine: the stale access units already queued carry the old
            // generation and trigger the same flush when the thread reaches them.
            let _ = tx.try_send(Msg::Reset);
        }
    }

    /// Takes the feedback accumulated since the previous call and starts a new window.
    pub fn take_feedback(&self) -> DecodeFeedback {
        self.shared.take_feedback()
    }

    /// Access units currently waiting for the decoder.
    pub fn queue_len(&self) -> usize {
        self.tx.as_ref().map_or(0, Sender::len)
    }

    /// `false` once the decode thread has exited (decoder construction panicked, …).
    pub fn is_alive(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }
}

impl Drop for DecodeThread {
    fn drop(&mut self) {
        // Closing the channel ends the thread's receive loop.
        self.tx = None;
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            tracing::error!(target: LOG, "video decode thread panicked");
        }
    }
}

fn frame_sink(
    mailbox: Arc<Mutex<MailboxWriter>>,
    shared: Arc<Shared>,
    on_frame: Option<FrameCallback>,
) -> FrameSink {
    Box::new(move |frame| {
        shared.frames.fetch_add(1, Ordering::Relaxed);
        Shared::record(
            &shared.decode_sum_us,
            &shared.decode_max_us,
            frame.decode_latency(),
        );
        shared.resolution.store(
            (u64::from(frame.width) << 32) | u64::from(frame.height),
            Ordering::Relaxed,
        );
        mailbox.lock().submit(frame);
        if let Some(cb) = on_frame.as_ref() {
            cb();
        }
    })
}

fn run<D: VideoDecoder>(
    rx: Receiver<Msg>,
    mut decoder: D,
    mailbox: Arc<Mutex<MailboxWriter>>,
    shared: Arc<Shared>,
) {
    let mut applied_generation = shared.generation.load(Ordering::Acquire);
    let apply_reset = |decoder: &mut D, applied: &mut u32| {
        let current = shared.generation.load(Ordering::Acquire);
        if *applied != current {
            // Pending frames are still delivered to the sink by `flush`; clear afterwards so
            // nothing of the old stream survives in the mailbox.
            decoder.flush();
            mailbox.lock().clear();
            *applied = current;
        }
    };

    for msg in rx {
        match msg {
            Msg::Reset => apply_reset(&mut decoder, &mut applied_generation),
            Msg::Au {
                data,
                t_received,
                generation,
            } => {
                apply_reset(&mut decoder, &mut applied_generation);
                if generation != applied_generation {
                    continue; // queued before a reset: belongs to the previous stream
                }
                match decoder.decode(&data, t_received) {
                    Ok(()) => {}
                    Err(
                        MediaError::BadData
                        | MediaError::WaitingForParameterSets
                        | MediaError::InvalidSession,
                    ) => {
                        shared.errors.fetch_add(1, Ordering::Relaxed);
                        shared.want_keyframe.store(true, Ordering::Release);
                    }
                    Err(err) => {
                        shared.errors.fetch_add(1, Ordering::Relaxed);
                        shared.want_keyframe.store(true, Ordering::Release);
                        tracing::error!(target: LOG, error = %err, "video decode failed");
                    }
                }
                Shared::record(
                    &shared.queue_sum_us,
                    &shared.queue_max_us,
                    t_received.elapsed(),
                );
                shared.queue_count.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    decoder.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64 as Delay;
    use xcloud_media::{ColorInfo, Frame, FrameMailbox, NativeBuffer};

    /// Records every access unit, optionally sleeps per decode, emits one frame per
    /// access unit and fails on a configurable marker byte.
    struct FakeDecoder {
        sink: FrameSink,
        log: Arc<Mutex<Vec<Vec<u8>>>>,
        delay_ms: Arc<Delay>,
        flushes: Arc<AtomicU32>,
    }

    impl VideoDecoder for FakeDecoder {
        fn decode(&mut self, au: &[u8], t_received: Instant) -> xcloud_media::Result<()> {
            // Log before the simulated decode time so a test that waits for the log entry
            // gets the whole delay as its window to act while this access unit is "busy".
            self.log.lock().push(au.to_vec());
            let delay = self.delay_ms.load(Ordering::Relaxed);
            if delay > 0 {
                std::thread::sleep(Duration::from_millis(delay));
            }
            if au.first() == Some(&0xff) {
                return Err(MediaError::BadData);
            }
            let now = Instant::now();
            (self.sink)(Frame {
                buf: NativeBuffer::Nv12Cpu {
                    y: vec![au.first().copied().unwrap_or(0); 4],
                    uv: vec![0; 2],
                    y_stride: 2,
                    uv_stride: 2,
                },
                width: 2,
                height: 2,
                color: ColorInfo::default(),
                t_received,
                t_submitted: t_received,
                t_decoded: now,
            });
            Ok(())
        }

        fn flush(&mut self) {
            self.flushes.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct Harness {
        thread: DecodeThread,
        reader: xcloud_media::MailboxReader,
        log: Arc<Mutex<Vec<Vec<u8>>>>,
        delay_ms: Arc<Delay>,
        flushes: Arc<AtomicU32>,
        repaints: Arc<AtomicU32>,
    }

    fn harness(delay_ms: u64) -> Harness {
        let (writer, reader) = FrameMailbox::open();
        let log = Arc::new(Mutex::new(Vec::new()));
        let delay = Arc::new(Delay::new(delay_ms));
        let flushes = Arc::new(AtomicU32::new(0));
        let repaints = Arc::new(AtomicU32::new(0));
        let (log2, delay2, flushes2, repaints2) = (
            Arc::clone(&log),
            Arc::clone(&delay),
            Arc::clone(&flushes),
            Arc::clone(&repaints),
        );
        let thread = DecodeThread::spawn(
            writer,
            Some(Box::new(move || {
                repaints2.fetch_add(1, Ordering::Relaxed);
            })),
            move |sink| FakeDecoder {
                sink,
                log: log2,
                delay_ms: delay2,
                flushes: flushes2,
            },
        );
        Harness {
            thread,
            reader,
            log,
            delay_ms: delay,
            flushes,
            repaints,
        }
    }

    fn wait_until(mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the decode thread"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn latency_window_averages_in_milliseconds() {
        let w = DecodeLatency::from_window(4, 10_000, 4_500);
        assert_eq!(w.samples, 4);
        assert!((w.avg_ms - 2.5).abs() < f64::EPSILON);
        assert!((w.max_ms - 4.5).abs() < f64::EPSILON);
        assert_eq!(
            DecodeLatency::from_window(0, 0, 0),
            DecodeLatency::default()
        );
    }

    #[test]
    fn decodes_in_order_and_fills_the_mailbox() {
        let mut h = harness(0);
        for i in 1..=10u8 {
            assert_eq!(
                h.thread.submit(&[i, 0, 0], Instant::now()),
                SubmitOutcome::Accepted
            );
            wait_until(|| h.log.lock().len() == usize::from(i));
        }
        let aus: Vec<u8> = h.log.lock().iter().map(|au| au[0]).collect();
        assert_eq!(aus, (1..=10).collect::<Vec<u8>>());
        let frame = h.reader.take_latest().expect("a decoded frame");
        assert_eq!(frame.width, 2);
        assert_eq!(h.repaints.load(Ordering::Relaxed), 10);
        let fb = h.thread.take_feedback();
        assert_eq!(fb.frames, 10);
        assert_eq!(fb.resolution, Some((2, 2)));
        assert!(!fb.want_keyframe);
        assert_eq!(fb.queue.samples, 10);
        // The window restarts after the read.
        assert_eq!(h.thread.take_feedback().frames, 0);
    }

    #[test]
    fn drops_when_the_queue_is_full() {
        let h = harness(200);
        let now = Instant::now();
        // The first one is taken by the thread almost immediately and blocks it; the
        // queue then holds QUEUE_DEPTH more.
        assert_eq!(h.thread.submit(&[1], now), SubmitOutcome::Accepted);
        wait_until(|| !h.log.lock().is_empty());
        for i in 2..=(QUEUE_DEPTH as u8 + 1) {
            assert_eq!(h.thread.submit(&[i], now), SubmitOutcome::Accepted);
        }
        assert_eq!(h.thread.queue_len(), QUEUE_DEPTH);
        let t = Instant::now();
        assert_eq!(h.thread.submit(&[99], now), SubmitOutcome::DroppedFull);
        assert!(
            t.elapsed() < Duration::from_millis(50),
            "submit must not block"
        );
        h.delay_ms.store(0, Ordering::Relaxed);
        wait_until(|| h.log.lock().len() == QUEUE_DEPTH + 1);
        assert!(h.log.lock().iter().all(|au| au[0] != 99));
    }

    #[test]
    fn reset_skips_stale_access_units_flushes_and_clears_the_mailbox() {
        let mut h = harness(100);
        let now = Instant::now();
        assert_eq!(h.thread.submit(&[1], now), SubmitOutcome::Accepted);
        wait_until(|| h.log.lock().len() == 1);
        for i in 2..=4u8 {
            assert_eq!(h.thread.submit(&[i], now), SubmitOutcome::Accepted);
        }
        let t = Instant::now();
        h.thread.reset();
        assert!(
            t.elapsed() < Duration::from_millis(50),
            "reset must not block"
        );
        h.delay_ms.store(0, Ordering::Relaxed);
        // The reset marker took the last queue slot; once the thread has skipped the stale
        // access units (2..4) and applied the reset, the new stream's access unit goes in.
        wait_until(|| h.thread.queue_len() == 0);
        assert_eq!(
            h.thread.submit(&[7], Instant::now()),
            SubmitOutcome::Accepted
        );
        wait_until(|| h.log.lock().len() == 2);
        assert_eq!(h.log.lock()[1], vec![7]);
        assert_eq!(h.flushes.load(Ordering::Relaxed), 1);
        // The frame of the old stream was cleared; the new stream's frame is what the
        // render thread sees.
        let frame = h.reader.take_latest().expect("new stream frame");
        let (y, ..) = frame.nv12_planes().expect("cpu planes");
        assert_eq!(y[0], 7);
    }

    #[test]
    fn reset_without_traffic_still_clears_the_mailbox() {
        let mut h = harness(0);
        h.thread.submit(&[3], Instant::now());
        wait_until(|| h.log.lock().len() == 1);
        assert!(h.reader.has_new());
        h.thread.reset();
        wait_until(|| h.flushes.load(Ordering::Relaxed) == 1);
        // The clear follows the flush; `latest` peeks without consuming the wake-up.
        wait_until(|| h.reader.latest().is_none());
        assert!(h.reader.take_latest().is_none());
    }

    #[test]
    fn bad_data_asks_for_a_keyframe() {
        let h = harness(0);
        h.thread.submit(&[0xff, 1], Instant::now());
        wait_until(|| h.log.lock().len() == 1);
        // The worker increments errors before publishing the recovery flag. Wait
        // for that Release store so feedback cannot be consumed between the two.
        wait_until(|| h.thread.shared.want_keyframe.load(Ordering::Acquire));
        let fb = h.thread.take_feedback();
        assert!(fb.want_keyframe);
        assert_eq!(fb.errors, 1);
        assert_eq!(fb.frames, 0);
        assert!(!h.thread.take_feedback().want_keyframe, "flag is one-shot");
    }

    #[test]
    fn a_dead_decode_thread_reports_unavailable_not_a_full_queue() {
        let (writer, _reader) = FrameMailbox::open();
        let thread = DecodeThread::spawn(writer, None, |_sink| -> FakeDecoder {
            panic!("no video decoder on this machine")
        });
        wait_until(|| !thread.is_alive());
        // The C++ decoder answered every call with a null image while `!m_valid`; the
        // pipeline must be told the decoder is gone, not that the queue is full, or it
        // would request keyframes for the rest of the session.
        assert_eq!(
            thread.submit(&[1], Instant::now()),
            SubmitOutcome::Unavailable
        );
        assert_eq!(
            thread.submit(&[2], Instant::now()),
            SubmitOutcome::Unavailable,
            "every access unit, not only the first"
        );
        assert_eq!(thread.queue_len(), 0);
        assert!(!thread.take_feedback().want_keyframe);
    }

    #[test]
    fn drop_joins_the_thread_and_flushes() {
        let h = harness(0);
        let flushes = Arc::clone(&h.flushes);
        assert!(h.thread.is_alive());
        drop(h);
        assert_eq!(flushes.load(Ordering::Relaxed), 1);
    }
}
