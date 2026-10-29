//! Port of qt/src/app/AudioOutput.{h,cpp}: push-mode playback of interleaved 16-bit PCM.
//!
//! The Qt class hopped from the pump thread to the GUI thread with queued signals
//! (`startRequested`, `dataReady`) and wrote into a `QAudioSink` there, dropping the chunk
//! when the sink's buffer was full. The pump thread itself only emitted signals. This port
//! keeps that split, with the GUI thread replaced by a dedicated device thread:
//!
//! * `push` (pump thread) writes straight into a lock-free SPSC ring ([`rtrb`]) and the
//!   device callback drains it; the hand-off is wait-free in both directions.
//! * Opening and closing the device (`QAudioSink` creation and `stop()` in the C++; tens of
//!   milliseconds of HAL calls in CoreAudio) happens on the `audio-device` thread, which
//!   receives [`Command`]s over a channel and reports back with [`Reply`]s. The pump thread
//!   never touches the device, so `push` honours the `MediaSink` contract of never blocking
//!   on I/O (docs/archive/PORTING_PLAN.md section 4, `xcloud-webrtc/src/transport.rs`).
//!
//! The device is behind [`AudioBackend`] so the ring, overflow and format-change logic is
//! unit-tested without opening a device; [`CpalBackend`] is the real implementation.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat};
use crossbeam_channel::{Receiver, Sender};
use tracing::{debug, info, warn};

use crate::{Error, Result};

/// `QAudioSink::setBufferSize(qMax(sampleRate / 10 * channels * 2, 16384))`, in bytes.
const MIN_RING_BYTES: usize = 16384;

/// How long to wait before retrying a failed device open (the C++ retried on every push).
const OPEN_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Sample rate and interleaved channel count of a PCM stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PcmFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl PcmFormat {
    pub const fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            sample_rate,
            channels,
        }
    }

    /// Ring capacity in samples for this format: a tenth of a second, at least 16 KiB.
    fn ring_capacity(self) -> usize {
        let tenth = (self.sample_rate / 10) as usize * usize::from(self.channels.max(1));
        tenth.max(MIN_RING_BYTES / size_of::<i16>())
    }
}

/// Counters shared between the pump thread and the device callback, for the HUD and tests.
#[derive(Debug, Default)]
pub struct AudioCounters {
    /// Chunks `push` discarded because the ring was full or no device could be opened.
    pub dropped_chunks: AtomicU64,
    /// Output samples the callback had to zero-fill because the ring ran dry.
    pub underrun_samples: AtomicU64,
}

/// The consumer side of the ring, pulled by the device callback. Converts to the device's
/// sample type and channel count on the fly; never blocks or allocates.
pub struct PcmSource {
    consumer: rtrb::Consumer<i16>,
    channels: u16,
    counters: Arc<AudioCounters>,
}

impl PcmSource {
    /// Channel count of the interleaved samples in the ring.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Fills `out` (interleaved, `out_channels` wide) from the ring, zero-filling whatever
    /// the ring cannot supply. Channel mismatches are mapped cheaply: extra output channels
    /// repeat the last source channel, extra source channels are discarded.
    pub fn fill<T>(&mut self, out: &mut [T], out_channels: u16)
    where
        T: Sample + FromSample<i16>,
    {
        let src_channels = usize::from(self.channels.max(1));
        let out_channels = usize::from(out_channels.max(1));
        let mut underrun = 0u64;

        if src_channels == out_channels {
            for sample in out.iter_mut() {
                *sample = match self.consumer.pop() {
                    Ok(v) => T::from_sample(v),
                    Err(_) => {
                        underrun += 1;
                        T::EQUILIBRIUM
                    }
                };
            }
        } else {
            for frame in out.chunks_mut(out_channels) {
                if self.consumer.slots() < src_channels {
                    frame.fill(T::EQUILIBRIUM);
                    underrun += frame.len() as u64;
                    continue;
                }
                // The slot count was checked above; a failed pop cannot happen on an SPSC
                // ring with one consumer, so treat it as silence.
                let mut last = 0i16;
                let mut popped = 0;
                for sample in frame.iter_mut().take(src_channels) {
                    last = self.consumer.pop().unwrap_or(0);
                    popped += 1;
                    *sample = T::from_sample(last);
                }
                // More source channels than the device has: discard the rest of the frame.
                for _ in popped..src_channels {
                    last = self.consumer.pop().unwrap_or(0);
                }
                // Fewer source channels: repeat the last one.
                for sample in frame.iter_mut().skip(src_channels) {
                    *sample = T::from_sample(last);
                }
            }
        }

        if underrun > 0 {
            self.counters
                .underrun_samples
                .fetch_add(underrun, Ordering::Relaxed);
        }
    }
}

/// A playing output stream; playback stops when the handle is dropped. It is created and
/// dropped on the device thread, so it need not be `Send`.
pub trait OutputStream {
    /// The format the device is actually running at.
    fn format(&self) -> PcmFormat;
}

/// The device layer. `open` runs on the dedicated device thread when the stream format is
/// first seen or changes, never on the pump thread, so it may block on the audio HAL.
pub trait AudioBackend: Send {
    /// Opens the default output device for `wanted`, draining `source` from the device
    /// callback. Returns the stream, whose [`OutputStream::format`] is what the device
    /// negotiated (it may differ from `wanted`).
    fn open(&mut self, wanted: PcmFormat, source: PcmSource) -> Result<Box<dyn OutputStream>>;
}

/// What the pump thread asks the device thread to do (`startRequested` / `stop()`).
enum Command {
    /// Close the current stream, if any, and open one for `format` fed by `source`.
    Open {
        generation: u64,
        format: PcmFormat,
        source: PcmSource,
    },
    /// Close the current stream, if any.
    Close,
    /// Test hook: answered once every command before it has been processed.
    #[cfg(test)]
    Barrier(Sender<()>),
}

/// The device thread's answer to a [`Command::Open`]. `generation` ties it to the request
/// so answers to superseded requests (format changed or stopped meanwhile) are ignored.
enum Reply {
    Opened {
        generation: u64,
        device_format: PcmFormat,
    },
    Failed {
        generation: u64,
        error: Error,
    },
}

/// Body of the `audio-device` thread: owns the backend and the open stream.
fn device_thread(
    mut backend: Box<dyn AudioBackend>,
    commands: Receiver<Command>,
    replies: Sender<Reply>,
) {
    let mut stream: Option<Box<dyn OutputStream>> = None;
    for command in commands {
        match command {
            Command::Open {
                generation,
                format,
                source,
            } => {
                // Like the C++ `start()`: stop the old sink before creating the new one.
                stream = None;
                let reply = match backend.open(format, source) {
                    Ok(opened) => {
                        let device_format = opened.format();
                        stream = Some(opened);
                        Reply::Opened {
                            generation,
                            device_format,
                        }
                    }
                    Err(error) => Reply::Failed { generation, error },
                };
                // The owner may be gone already; nothing left to tell then.
                let _ = replies.send(reply);
            }
            Command::Close => stream = None,
            #[cfg(test)]
            Command::Barrier(done) => {
                let _ = done.send(());
            }
        }
    }
    // The owning `AudioOutput` was dropped: release the device here, on this thread.
    drop(stream);
}

/// Plays interleaved 16-bit PCM. Port of `AudioOutput`: the stream is (re)created
/// when the pushed format changes, the first push starts it, and chunks that do not fit
/// the buffer are dropped rather than blocking the caller.
///
/// `push` and `stop` only touch the ring and a channel; the device itself is opened and
/// closed on the `audio-device` thread spawned by the constructor.
pub struct AudioOutput {
    commands: Sender<Command>,
    replies: Receiver<Reply>,
    producer: Option<rtrb::Producer<i16>>,
    /// Format of the samples in the ring (`m_sampleRate`/`m_channels`); `None` when stopped.
    format: Option<PcmFormat>,
    /// What the device negotiated; `None` until the device thread reports the open.
    device_format: Option<PcmFormat>,
    /// Bumped on every open request and stop, so stale replies are recognised.
    generation: u64,
    /// Chunks queued since the last open request; they are lost if the open fails.
    chunks_since_open: u64,
    last_open_failure: Option<(PcmFormat, Instant)>,
    counters: Arc<AudioCounters>,
}

impl Default for AudioOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioOutput {
    /// Output through the system's default device ([`CpalBackend`]).
    pub fn new() -> Self {
        Self::with_backend(Box::new(CpalBackend))
    }

    /// Spawns the device thread that owns `backend`. If the thread cannot be spawned, the
    /// output behaves like the C++ with no audio device: every chunk is dropped.
    pub fn with_backend(backend: Box<dyn AudioBackend>) -> Self {
        let (commands, command_rx) = crossbeam_channel::unbounded();
        let (reply_tx, replies) = crossbeam_channel::unbounded();
        let spawned = std::thread::Builder::new()
            .name("audio-device".into())
            .spawn(move || device_thread(backend, command_rx, reply_tx));
        if let Err(err) = spawned {
            // The receiver went down with the closure, so every later `send` fails and
            // `start` records an open failure.
            warn!(target: "Audio", "cannot spawn the audio device thread: {err}");
        }
        Self {
            commands,
            replies,
            producer: None,
            format: None,
            device_format: None,
            generation: 0,
            chunks_since_open: 0,
            last_open_failure: None,
            counters: Arc::new(AudioCounters::default()),
        }
    }

    /// Queues `pcm` (interleaved `i16`, `channels` wide at `sample_rate` Hz) for playback.
    /// Called from the pump thread; never blocks. Requests a new stream from the device
    /// thread when the format differs from the current one; samples pushed while the device
    /// is still opening wait in the ring. The whole chunk is dropped when it does not fit.
    pub fn push(&mut self, pcm: &[i16], sample_rate: u32, channels: u16) {
        if pcm.is_empty() {
            return;
        }
        self.poll_replies();
        let format = PcmFormat::new(sample_rate, channels);
        if self.format != Some(format) {
            self.start(format);
        }
        let Some(producer) = self.producer.as_mut() else {
            // No device: behave like the C++ with a null sink, the data is discarded.
            self.counters.dropped_chunks.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if producer.push_entire_slice(pcm).is_err() {
            let dropped = self.counters.dropped_chunks.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped.is_power_of_two() {
                debug!(target: "Audio", "audio buffer full, dropped {dropped} chunks so far");
            }
        } else {
            self.chunks_since_open += 1;
        }
    }

    /// Stops playback and forgets the format; the next push starts a new stream. The device
    /// is released on the device thread, this call does not wait for it.
    pub fn stop(&mut self) {
        self.generation += 1;
        if self.format.is_some() || self.device_format.is_some() {
            let _ = self.commands.send(Command::Close);
        }
        self.producer = None;
        self.format = None;
        self.device_format = None;
        self.chunks_since_open = 0;
        self.last_open_failure = None;
    }

    /// Whether the device thread has reported an open stream.
    pub fn is_playing(&self) -> bool {
        self.device_format.is_some()
    }

    /// Whether an open request is in flight on the device thread.
    pub fn is_starting(&self) -> bool {
        self.format.is_some() && self.device_format.is_none()
    }

    /// Format of the PCM currently accepted by `push`, `None` when stopped.
    pub fn format(&self) -> Option<PcmFormat> {
        self.format
    }

    /// Format the device negotiated, `None` when stopped or still opening.
    pub fn device_format(&self) -> Option<PcmFormat> {
        self.device_format
    }

    /// Drop/underrun counters for the HUD.
    pub fn counters(&self) -> &Arc<AudioCounters> {
        &self.counters
    }

    /// Free space in the ring, in samples (`QAudioSink::bytesFree` in samples).
    pub fn free_samples(&self) -> usize {
        self.producer.as_ref().map_or(0, |p| p.slots())
    }

    /// Picks up the device thread's answers without waiting. Also called by `push`.
    pub fn poll_replies(&mut self) {
        while let Ok(reply) = self.replies.try_recv() {
            match reply {
                Reply::Opened {
                    generation,
                    device_format,
                } if generation == self.generation => {
                    self.device_format = Some(device_format);
                    self.chunks_since_open = 0;
                }
                Reply::Failed { generation, error } if generation == self.generation => {
                    warn!(target: "Audio", "audio output failed to start: {error}");
                    if let Some(format) = self.format.take() {
                        self.last_open_failure = Some((format, Instant::now()));
                    }
                    // Whatever was queued for this stream is lost with its ring.
                    self.counters
                        .dropped_chunks
                        .fetch_add(self.chunks_since_open, Ordering::Relaxed);
                    self.chunks_since_open = 0;
                    self.producer = None;
                    self.device_format = None;
                }
                // Answer to a request that `stop` or a format change superseded.
                Reply::Opened { .. } | Reply::Failed { .. } => {}
            }
        }
    }

    /// Port of `AudioOutput::start`: creates the ring and asks the device thread to open a
    /// stream for `format`. Returns at once; the answer arrives through `poll_replies`.
    fn start(&mut self, format: PcmFormat) {
        if self.format == Some(format) {
            return;
        }
        if let Some((failed, at)) = self.last_open_failure
            && failed == format
            && at.elapsed() < OPEN_RETRY_INTERVAL
        {
            return;
        }
        self.stop();

        let (producer, consumer) = rtrb::RingBuffer::new(format.ring_capacity());
        let source = PcmSource {
            consumer,
            channels: format.channels,
            counters: Arc::clone(&self.counters),
        };
        let request = Command::Open {
            generation: self.generation,
            format,
            source,
        };
        if self.commands.send(request).is_err() {
            warn!(target: "Audio", "audio output failed to start: device thread is gone");
            self.last_open_failure = Some((format, Instant::now()));
            return;
        }
        self.producer = Some(producer);
        self.format = Some(format);
    }

    /// Blocks until the device thread has processed every command sent so far and folds
    /// in its replies. Test-only: production callers never wait on the device.
    #[cfg(test)]
    fn settle(&mut self) {
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        if self.commands.send(Command::Barrier(done_tx)).is_ok() {
            done_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("audio device thread answers the barrier");
        }
        self.poll_replies();
    }
}

impl std::fmt::Debug for AudioOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioOutput")
            .field("format", &self.format)
            .field("device_format", &self.device_format)
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

/// Default output device through cpal (CoreAudio on macOS, WASAPI/ALSA elsewhere).
///
/// Asks for `i16` at the pushed rate and channel count like the Qt code; when the device
/// does not support that, falls back to its default configuration and converts samples in
/// the callback.
#[derive(Debug, Default, Clone, Copy)]
pub struct CpalBackend;

struct CpalStream {
    _stream: cpal::Stream,
    format: PcmFormat,
}

impl OutputStream for CpalStream {
    fn format(&self) -> PcmFormat {
        self.format
    }
}

fn cpal_error(what: &str, err: impl std::fmt::Display) -> Error {
    Error::Unavailable(format!("{what}: {err}"))
}

impl CpalBackend {
    fn pick_config(
        device: &cpal::Device,
        wanted: PcmFormat,
    ) -> Result<cpal::SupportedStreamConfig> {
        let exact = device
            .supported_output_configs()
            .map_err(|e| cpal_error("querying output configs", e))?
            .find(|range| {
                range.channels() == wanted.channels
                    && range.sample_format() == SampleFormat::I16
                    && range.contains_rate(wanted.sample_rate)
            })
            .map(|range| range.with_sample_rate(wanted.sample_rate));
        match exact {
            Some(config) => Ok(config),
            None => {
                warn!(
                    target: "Audio",
                    "{} Hz {} ch Int16 not supported; using preferred format",
                    wanted.sample_rate, wanted.channels
                );
                device
                    .default_output_config()
                    .map_err(|e| cpal_error("querying default output config", e))
            }
        }
    }

    fn build<T>(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        mut source: PcmSource,
    ) -> Result<cpal::Stream>
    where
        T: cpal::SizedSample + FromSample<i16>,
    {
        let out_channels = config.channels;
        device
            .build_output_stream::<T, _, _>(
                *config,
                move |data, _info| source.fill(data, out_channels),
                |err| warn!(target: "Audio", "audio stream error: {err}"),
                None,
            )
            .map_err(|e| cpal_error("building output stream", e))
    }
}

impl AudioBackend for CpalBackend {
    fn open(&mut self, wanted: PcmFormat, source: PcmSource) -> Result<Box<dyn OutputStream>> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| Error::Unavailable("no default audio output device".into()))?;
        let supported = Self::pick_config(&device, wanted)?;
        let format = PcmFormat::new(supported.sample_rate(), supported.channels());
        if format.sample_rate != wanted.sample_rate {
            warn!(
                target: "Audio",
                "device runs at {} Hz, source is {} Hz; no resampling, pitch will be off",
                format.sample_rate, wanted.sample_rate
            );
        }
        let sample_format = supported.sample_format();
        let config = supported.config();
        let stream = match sample_format {
            SampleFormat::I16 => Self::build::<i16>(&device, &config, source)?,
            SampleFormat::F32 => {
                debug!(target: "Audio", "device wants f32 samples; converting from i16");
                Self::build::<f32>(&device, &config, source)?
            }
            SampleFormat::I32 => Self::build::<i32>(&device, &config, source)?,
            SampleFormat::U16 => Self::build::<u16>(&device, &config, source)?,
            SampleFormat::F64 => Self::build::<f64>(&device, &config, source)?,
            other => {
                return Err(Error::Unavailable(format!(
                    "unsupported device sample format {other:?}"
                )));
            }
        };
        stream
            .play()
            .map_err(|e| cpal_error("starting output stream", e))?;
        info!(
            target: "Audio",
            "audio output started {} Hz {} ch ({sample_format:?})",
            format.sample_rate, format.channels
        );
        Ok(Box::new(CpalStream {
            _stream: stream,
            format,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::thread::ThreadId;

    /// Records every open and hands the ring consumer to the test.
    #[derive(Default)]
    struct MockState {
        opened: Vec<PcmFormat>,
        source: Option<PcmSource>,
        /// Format the "device" negotiates instead of the wanted one.
        negotiated: Option<PcmFormat>,
        fail: bool,
        /// When set, `open` blocks until the test sends on it, like a slow HAL call.
        gate: Option<Receiver<()>>,
        /// Thread that ran `open`.
        opened_on: Option<ThreadId>,
        /// Threads that dropped a stream, in order.
        closed_on: Vec<ThreadId>,
    }

    #[derive(Clone, Default)]
    struct MockBackend(Arc<Mutex<MockState>>);

    struct MockStream {
        format: PcmFormat,
        state: Arc<Mutex<MockState>>,
    }

    impl OutputStream for MockStream {
        fn format(&self) -> PcmFormat {
            self.format
        }
    }

    impl Drop for MockStream {
        fn drop(&mut self) {
            self.state
                .lock()
                .closed_on
                .push(std::thread::current().id());
        }
    }

    impl AudioBackend for MockBackend {
        fn open(&mut self, wanted: PcmFormat, source: PcmSource) -> Result<Box<dyn OutputStream>> {
            let gate = self.0.lock().gate.clone();
            if let Some(gate) = gate {
                let _ = gate.recv();
            }
            let mut state = self.0.lock();
            state.opened.push(wanted);
            state.opened_on = Some(std::thread::current().id());
            if state.fail {
                return Err(Error::Unavailable("mock: no device".into()));
            }
            state.source = Some(source);
            let format = state.negotiated.unwrap_or(wanted);
            Ok(Box::new(MockStream {
                format,
                state: Arc::clone(&self.0),
            }))
        }
    }

    fn output() -> (AudioOutput, MockBackend) {
        let backend = MockBackend::default();
        (
            AudioOutput::with_backend(Box::new(backend.clone())),
            backend,
        )
    }

    fn drain(backend: &MockBackend, n: usize) -> Vec<i16> {
        let mut state = backend.0.lock();
        let source = state.source.as_mut().expect("stream opened");
        let mut out = vec![0i16; n];
        let channels = source.channels();
        source.fill(&mut out, channels);
        out
    }

    fn ramp(n: usize) -> Vec<i16> {
        (0..n).map(|i| i as i16).collect()
    }

    #[test]
    fn first_push_starts_the_stream_with_the_pushed_format() {
        let (mut out, backend) = output();
        assert!(!out.is_playing());
        assert!(!out.is_starting());
        out.push(&ramp(1920), 48_000, 2);
        assert!(out.is_starting());
        assert_eq!(out.format(), Some(PcmFormat::new(48_000, 2)));
        out.settle();
        assert!(out.is_playing());
        assert!(!out.is_starting());
        assert_eq!(backend.0.lock().opened, vec![PcmFormat::new(48_000, 2)]);
    }

    #[test]
    fn pushed_samples_come_out_of_the_callback_in_order() {
        let (mut out, backend) = output();
        out.push(&ramp(1920), 48_000, 2);
        out.push(&[-1, -2, -3, -4], 48_000, 2);
        out.settle();
        let got = drain(&backend, 1924);
        assert_eq!(&got[..1920], &ramp(1920)[..]);
        assert_eq!(&got[1920..], &[-1, -2, -3, -4]);
        assert_eq!(out.counters().underrun_samples.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn empty_push_is_ignored() {
        let (mut out, backend) = output();
        out.push(&[], 48_000, 2);
        out.settle();
        assert!(!out.is_playing());
        assert!(!out.is_starting());
        assert!(backend.0.lock().opened.is_empty());
    }

    #[test]
    fn same_format_keeps_the_stream() {
        let (mut out, backend) = output();
        for _ in 0..10 {
            out.push(&ramp(960), 48_000, 2);
        }
        out.settle();
        for _ in 0..10 {
            out.push(&ramp(960), 48_000, 2);
        }
        out.settle();
        assert_eq!(backend.0.lock().opened.len(), 1);
        assert!(backend.0.lock().closed_on.is_empty());
    }

    #[test]
    fn format_change_recreates_the_stream_and_ring() {
        let (mut out, backend) = output();
        out.push(&ramp(1920), 48_000, 2);
        out.settle();
        out.push(&ramp(480), 24_000, 1);
        assert_eq!(out.format(), Some(PcmFormat::new(24_000, 1)));
        assert!(out.is_starting(), "the new stream is being opened");
        out.settle();
        assert_eq!(
            backend.0.lock().opened,
            vec![PcmFormat::new(48_000, 2), PcmFormat::new(24_000, 1)]
        );
        assert_eq!(out.device_format(), Some(PcmFormat::new(24_000, 1)));
        // The old ring went with the old stream: only the mono data is left.
        let got = drain(&backend, 480);
        assert_eq!(got, ramp(480));
        assert_eq!(backend.0.lock().source.as_ref().unwrap().channels(), 1);
        assert_eq!(
            backend.0.lock().closed_on.len(),
            1,
            "old stream was dropped"
        );
    }

    #[test]
    fn ring_capacity_matches_the_qt_buffer_size() {
        // qMax(sampleRate / 10 * channels * 2, 16384) bytes.
        assert_eq!(PcmFormat::new(48_000, 2).ring_capacity(), 9600);
        assert_eq!(PcmFormat::new(48_000, 1).ring_capacity(), 8192);
        assert_eq!(PcmFormat::new(8_000, 1).ring_capacity(), 8192);
        assert_eq!(PcmFormat::new(96_000, 2).ring_capacity(), 19_200);

        let (mut out, _backend) = output();
        out.push(&[1, 2], 48_000, 2);
        assert_eq!(out.free_samples(), 9600 - 2);
    }

    #[test]
    fn overflow_drops_the_whole_chunk_without_blocking() {
        let (mut out, backend) = output();
        let frame = ramp(1920);
        // 9600 slots: five 20 ms frames fit exactly.
        for _ in 0..5 {
            out.push(&frame, 48_000, 2);
        }
        assert_eq!(out.free_samples(), 0);
        out.push(&[9; 1920], 48_000, 2);
        assert_eq!(out.counters().dropped_chunks.load(Ordering::Relaxed), 1);
        out.settle();

        // Partially free: a chunk larger than the free space is still dropped entirely.
        let got = drain(&backend, 1000);
        assert_eq!(&got[..], &frame[..1000]);
        assert_eq!(out.free_samples(), 1000);
        out.push(&[9; 1001], 48_000, 2);
        assert_eq!(out.counters().dropped_chunks.load(Ordering::Relaxed), 2);
        assert_eq!(out.free_samples(), 1000);
        // ... and a chunk that fits goes in.
        out.push(&[9; 1000], 48_000, 2);
        assert_eq!(out.free_samples(), 0);
        let rest = drain(&backend, 9600);
        assert_eq!(&rest[..920], &frame[1000..]);
        assert!(rest[8600..].iter().all(|&s| s == 9));
    }

    #[test]
    fn callback_zero_fills_on_underrun() {
        let (mut out, backend) = output();
        out.push(&[5; 100], 48_000, 2);
        out.settle();
        let got = drain(&backend, 300);
        assert!(got[..100].iter().all(|&s| s == 5));
        assert!(got[100..].iter().all(|&s| s == 0));
        assert_eq!(out.counters().underrun_samples.load(Ordering::Relaxed), 200);
        // Fully dry callback.
        let got = drain(&backend, 64);
        assert!(got.iter().all(|&s| s == 0));
        assert_eq!(out.counters().underrun_samples.load(Ordering::Relaxed), 264);
    }

    #[test]
    fn callback_converts_i16_to_f32() {
        let (mut out, backend) = output();
        out.push(&[i16::MAX, i16::MIN, 0, 16384], 48_000, 2);
        out.settle();
        let mut got = [0.5f32; 6];
        backend.0.lock().source.as_mut().unwrap().fill(&mut got, 2);
        assert!((got[0] - 1.0).abs() < 1e-3, "{}", got[0]);
        assert!((got[1] + 1.0).abs() < 1e-3, "{}", got[1]);
        assert_eq!(got[2], 0.0);
        assert!((got[3] - 0.5).abs() < 1e-3, "{}", got[3]);
        assert_eq!(&got[4..], &[0.0, 0.0], "underrun is f32 silence");
    }

    #[test]
    fn callback_maps_mono_source_to_stereo_device() {
        let (mut out, backend) = output();
        out.push(&[1, 2, 3], 48_000, 1);
        out.settle();
        let mut got = [7i16; 8];
        backend.0.lock().source.as_mut().unwrap().fill(&mut got, 2);
        assert_eq!(got, [1, 1, 2, 2, 3, 3, 0, 0]);
        assert_eq!(out.counters().underrun_samples.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn callback_maps_stereo_source_to_mono_device() {
        let (mut out, backend) = output();
        out.push(&[1, 2, 3, 4, 5], 48_000, 2);
        out.settle();
        let mut got = [7i16; 4];
        backend.0.lock().source.as_mut().unwrap().fill(&mut got, 1);
        // Left channel of each frame; the trailing half frame cannot be played.
        assert_eq!(got, [1, 3, 0, 0]);
    }

    #[test]
    fn device_format_is_what_the_backend_negotiated() {
        let (mut out, backend) = output();
        backend.0.lock().negotiated = Some(PcmFormat::new(44_100, 2));
        out.push(&ramp(10), 48_000, 2);
        assert_eq!(
            out.device_format(),
            None,
            "not known until the device thread answers"
        );
        out.settle();
        assert_eq!(out.format(), Some(PcmFormat::new(48_000, 2)));
        assert_eq!(out.device_format(), Some(PcmFormat::new(44_100, 2)));
    }

    #[test]
    fn stop_forgets_the_format_and_the_next_push_restarts() {
        let (mut out, backend) = output();
        out.push(&ramp(10), 48_000, 2);
        out.settle();
        out.stop();
        assert!(!out.is_playing());
        assert!(!out.is_starting());
        assert_eq!(out.format(), None);
        assert_eq!(out.device_format(), None);
        assert_eq!(out.free_samples(), 0);
        out.push(&ramp(10), 48_000, 2);
        out.settle();
        assert!(out.is_playing());
        assert_eq!(backend.0.lock().opened.len(), 2);
    }

    #[test]
    fn open_failure_discards_data_and_does_not_retry_immediately() {
        let (mut out, backend) = output();
        backend.0.lock().fail = true;
        out.push(&ramp(10), 48_000, 2);
        out.settle();
        // The chunk queued while the open was pending is lost with its ring, like the
        // C++ `dataReady` handler finding no sink.
        assert_eq!(out.counters().dropped_chunks.load(Ordering::Relaxed), 1);
        for _ in 0..4 {
            out.push(&ramp(10), 48_000, 2);
        }
        out.settle();
        assert!(!out.is_playing());
        assert!(!out.is_starting());
        assert_eq!(backend.0.lock().opened.len(), 1, "retry is rate limited");
        assert_eq!(out.counters().dropped_chunks.load(Ordering::Relaxed), 5);
        // A different format is tried straight away.
        out.push(&ramp(10), 24_000, 1);
        out.settle();
        assert_eq!(backend.0.lock().opened.len(), 2);
        // Once the device is back, stop() clears the back-off and playback starts.
        backend.0.lock().fail = false;
        out.stop();
        out.push(&ramp(10), 48_000, 2);
        out.settle();
        assert!(out.is_playing());
    }

    /// The fix for the pump-thread divergence: `push` must return before, and
    /// independently of, the device open (`startRequested` was a queued signal in Qt).
    #[test]
    fn push_does_not_wait_for_the_device_to_open() {
        let (mut out, backend) = output();
        let (release, gate) = crossbeam_channel::bounded::<()>(1);
        backend.0.lock().gate = Some(gate);

        let before = Instant::now();
        for _ in 0..3 {
            out.push(&ramp(100), 48_000, 2);
        }
        assert!(
            before.elapsed() < Duration::from_millis(500),
            "push blocked on the device open"
        );
        assert!(out.is_starting());
        assert!(!out.is_playing());
        assert!(backend.0.lock().opened.is_empty(), "open is still blocked");
        assert_eq!(
            out.free_samples(),
            9600 - 300,
            "samples wait in the ring meanwhile"
        );

        release
            .send(())
            .expect("device thread is waiting on the gate");
        out.settle();
        assert!(out.is_playing());
        let state = backend.0.lock();
        assert_eq!(state.opened, vec![PcmFormat::new(48_000, 2)]);
        assert_ne!(
            state.opened_on,
            Some(std::thread::current().id()),
            "open ran on the device thread, not the caller's"
        );
        drop(state);
        assert_eq!(
            drain(&backend, 300),
            [ramp(100), ramp(100), ramp(100)].concat()
        );
    }

    #[test]
    fn stream_is_closed_on_the_device_thread_without_waiting() {
        let (mut out, backend) = output();
        out.push(&ramp(10), 48_000, 2);
        out.settle();
        let (release, gate) = crossbeam_channel::bounded::<()>(1);
        backend.0.lock().gate = Some(gate);
        // Keep the device thread busy so a blocking stop() would be detectable.
        out.push(&ramp(10), 24_000, 1);
        let before = Instant::now();
        out.stop();
        assert!(
            before.elapsed() < Duration::from_millis(500),
            "stop blocked"
        );
        assert!(!out.is_playing());
        release
            .send(())
            .expect("device thread is waiting on the gate");
        out.settle();

        let state = backend.0.lock();
        assert_eq!(
            state.closed_on.len(),
            2,
            "48 kHz stream and the 24 kHz stream"
        );
        assert!(
            state
                .closed_on
                .iter()
                .all(|id| Some(*id) == state.opened_on),
            "streams are dropped on the thread that opened them"
        );
        assert_ne!(state.opened_on, Some(std::thread::current().id()));
    }

    #[test]
    fn format_change_while_opening_ignores_the_stale_reply() {
        let (mut out, backend) = output();
        let (release, gate) = crossbeam_channel::bounded::<()>(2);
        backend.0.lock().gate = Some(gate);
        out.push(&ramp(10), 48_000, 2);
        out.push(&ramp(10), 24_000, 1);
        assert_eq!(out.format(), Some(PcmFormat::new(24_000, 1)));
        release.send(()).expect("gate");
        release.send(()).expect("gate");
        out.settle();
        assert_eq!(
            backend.0.lock().opened,
            vec![PcmFormat::new(48_000, 2), PcmFormat::new(24_000, 1)]
        );
        assert_eq!(out.format(), Some(PcmFormat::new(24_000, 1)));
        assert_eq!(out.device_format(), Some(PcmFormat::new(24_000, 1)));
        assert_eq!(drain(&backend, 10), ramp(10));
    }

    #[test]
    fn stop_while_opening_ignores_the_late_reply() {
        let (mut out, backend) = output();
        let (release, gate) = crossbeam_channel::bounded::<()>(1);
        backend.0.lock().gate = Some(gate);
        out.push(&ramp(10), 48_000, 2);
        out.stop();
        release.send(()).expect("gate");
        out.settle();
        assert!(!out.is_playing());
        assert!(!out.is_starting());
        assert_eq!(out.device_format(), None);
        assert_eq!(
            backend.0.lock().closed_on.len(),
            1,
            "the late stream was released"
        );
    }

    #[test]
    fn dropping_the_output_releases_the_device_on_the_device_thread() {
        let (mut out, backend) = output();
        out.push(&ramp(10), 48_000, 2);
        out.settle();
        drop(out);
        let deadline = Instant::now() + Duration::from_secs(10);
        while backend.0.lock().closed_on.is_empty() {
            assert!(Instant::now() < deadline, "device thread did not exit");
            std::thread::sleep(Duration::from_millis(1));
        }
        let state = backend.0.lock();
        assert_eq!(state.closed_on.len(), 1);
        assert_eq!(state.closed_on[0], state.opened_on.expect("opened"));
    }

    #[test]
    fn audio_output_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<AudioOutput>();
        assert_send::<PcmSource>();
    }

    /// Opens the real default device and plays 200 ms of a 440 Hz tone.
    #[test]
    #[ignore = "opens the default audio output device"]
    fn real_device_plays_a_tone() {
        let mut out = AudioOutput::new();
        let frame: Vec<i16> = (0..960)
            .flat_map(|n| {
                let v =
                    ((n as f32 / 48_000.0 * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
                [v, v]
            })
            .collect();
        // The first push only requests the device; wait for the device thread to open it
        // (CoreAudio takes ~100 ms, longer than the ring holds) before timing the pushes.
        out.push(&frame, 48_000, 2);
        assert!(out.is_starting(), "{out:?}");
        out.settle();
        assert!(out.is_playing(), "{out:?}");
        let device = out.device_format().expect("device format");
        assert!(device.channels >= 1);
        for _ in 0..9 {
            out.push(&frame, 48_000, 2);
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(out.counters().dropped_chunks.load(Ordering::Relaxed), 0);
        out.stop();
    }
}
