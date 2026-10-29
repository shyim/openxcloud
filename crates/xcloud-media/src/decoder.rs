//! Port of the decoder interface of `qt/src/streaming/H264Decoder.h` (as the
//! [`VideoDecoder`] trait) and of `qt/src/streaming/VideoFrameMailbox.{h,cpp}` (as the
//! lock-free [`FrameMailbox`]).
//!
//! The Qt mailbox was a mutex-protected "latest frame" slot plus a queued `invokeMethod`
//! wake-up to the GUI thread. Here the slot is a `triple_buffer` (lock-free, latest-wins)
//! and the wake-up is the caller's business (`egui::Context::request_repaint`), see
//! docs/archive/PORTING_PLAN.md section 4.

use std::sync::Arc;
use std::time::Instant;

use crate::Result;
use crate::frame::Frame;

/// Where decoded frames go. Called on the decoder's output thread, so it must be quick:
/// push into a mailbox and request a repaint.
pub type FrameSink = Box<dyn FnMut(Frame) + Send>;

/// An H.264 access-unit decoder. One instance per stream; not `Sync`, lives on the
/// decode thread. Output is delivered asynchronously through the [`FrameSink`] given at
/// construction.
pub trait VideoDecoder: Send {
    /// Decodes one complete Annex-B access unit as produced by `VideoJitterBuffer`.
    /// `t_received` is when the access unit became complete (carried into the frame
    /// for the latency HUD).
    ///
    /// Returns `Err(BadData)` / `Err(WaitingForParameterSets)` when the caller should
    /// request a keyframe; other errors are fatal for the stream.
    fn decode(&mut self, au: &[u8], t_received: Instant) -> Result<()>;

    /// Drains pending asynchronous output and drops decoder state (the Qt `reset()`).
    /// Pending frames are still delivered to the sink before this returns.
    fn flush(&mut self);
}

/// Latest-frame hand-off between the decode thread (writer) and the render thread
/// (reader). Port of `VideoFrameMailbox`: "at most one frame and one wakeup are retained
/// even when the GUI temporarily stops processing events".
///
/// Retention bound: the mailbox holds **at most one frame**, the newest pending one. A
/// `triple_buffer` would normally also keep the previously published frame in the
/// producer's private slot; the writer clears that slot right after publishing, so a
/// superseded frame is released by the next `submit`, like the C++ `m_native = frame`
/// overwrite. [`MailboxReader::take_latest`] moves the frame out, so a delivered frame is
/// owned by the reader alone (the Qt signal receiver's copy). [`MailboxWriter::clear`]
/// releases the pending frame but, like the C++, leaves the "new frame" flag set: the next
/// poll finds nothing or the new stream's first frame.
pub struct FrameMailbox;

impl FrameMailbox {
    /// Creates a connected writer/reader pair.
    pub fn open() -> (MailboxWriter, MailboxReader) {
        let (input, output) = triple_buffer::triple_buffer(&None);
        (MailboxWriter { input }, MailboxReader { output })
    }
}

/// Producer side of the [`FrameMailbox`]. Lives on the decode thread.
pub struct MailboxWriter {
    input: triple_buffer::Input<Option<Arc<Frame>>>,
}

impl MailboxWriter {
    /// Publishes a frame, replacing any frame the reader has not picked up yet.
    pub fn submit(&mut self, frame: Frame) {
        self.submit_arc(Arc::new(frame));
    }

    /// Publishes an already shared frame.
    pub fn submit_arc(&mut self, frame: Arc<Frame>) {
        self.publish(Some(frame));
    }

    /// Drops the pending frame (`VideoFrameMailbox::clear`). Used when a stream ends so
    /// the render thread does not show a stale picture after reconnecting.
    pub fn clear(&mut self) {
        self.publish(None);
    }

    /// Wraps the writer as a [`FrameSink`] for a [`VideoDecoder`].
    pub fn into_sink(mut self) -> FrameSink {
        Box::new(move |frame| self.submit(frame))
    }

    fn publish(&mut self, value: Option<Arc<Frame>>) {
        self.input.write(value);
        // After `write` the producer's private slot holds the frame that was pending before
        // this one (the old back buffer). Nobody can read it any more; release it now so
        // the mailbox never holds more than the single newest frame.
        *self.input.input_buffer_mut() = None;
    }
}

/// Consumer side of the [`FrameMailbox`]. Lives on the render thread.
pub struct MailboxReader {
    output: triple_buffer::Output<Option<Arc<Frame>>>,
}

impl MailboxReader {
    /// `true` if the writer published (or cleared) since the last `take_latest`/`latest`.
    pub fn has_new(&self) -> bool {
        self.output.updated()
    }

    /// Returns the newest frame if one was published since the last `take_latest`/`latest`
    /// call, `None` otherwise. The frame leaves the mailbox: the caller owns it, mirroring
    /// the single `nativeFrameReady` emission per wake-up of the Qt mailbox.
    pub fn take_latest(&mut self) -> Option<Arc<Frame>> {
        if !self.output.update() {
            return None;
        }
        self.output.output_buffer_mut().take()
    }

    /// Returns the current frame without removing it, new or not: the newest pending frame,
    /// or the frame a previous `latest` call returned. Frames removed by `take_latest` are
    /// gone. Use when redrawing for another reason and the mailbox should keep the frame
    /// alive for you.
    pub fn latest(&mut self) -> Option<Arc<Frame>> {
        self.output.read().clone()
    }
}

// The reader lives on the render thread, the writer on the decode thread.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<MailboxWriter>();
    assert_send::<MailboxReader>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{ColorInfo, NativeBuffer};
    use std::sync::Weak;
    use std::time::Duration;

    fn frame(tag: u8) -> Frame {
        let now = Instant::now();
        Frame {
            buf: NativeBuffer::Nv12Cpu {
                y: vec![tag; 4],
                uv: vec![tag; 2],
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

    fn tag_of(frame: &Frame) -> u8 {
        frame.nv12_planes().map(|(y, ..)| y[0]).unwrap_or(0)
    }

    fn released(frames: &[Weak<Frame>]) -> usize {
        frames.iter().filter(|w| w.strong_count() == 0).count()
    }

    #[test]
    fn mailbox_starts_empty() {
        let (_writer, mut reader) = FrameMailbox::open();
        assert!(!reader.has_new());
        assert!(reader.take_latest().is_none());
        assert!(reader.latest().is_none());
    }

    #[test]
    fn mailbox_keeps_only_the_latest_frame() {
        let (mut writer, mut reader) = FrameMailbox::open();
        writer.submit(frame(1));
        writer.submit(frame(2));
        writer.submit(frame(3));
        assert!(reader.has_new());
        let latest = reader.take_latest().expect("a frame");
        assert_eq!(tag_of(&latest), 3);
        // Nothing new until the next submit, and the taken frame has left the mailbox.
        assert!(!reader.has_new());
        assert!(reader.take_latest().is_none());
        assert!(reader.latest().is_none());
        writer.submit(frame(4));
        assert_eq!(tag_of(&reader.take_latest().expect("new frame")), 4);
    }

    #[test]
    fn latest_peeks_without_removing_the_frame() {
        let (mut writer, mut reader) = FrameMailbox::open();
        writer.submit(frame(5));
        assert_eq!(tag_of(&reader.latest().expect("frame")), 5);
        assert!(!reader.has_new(), "latest consumes the new-frame flag");
        assert_eq!(tag_of(&reader.latest().expect("still there")), 5);
        assert!(reader.take_latest().is_none(), "not new any more");
        writer.submit(frame(6));
        assert_eq!(tag_of(&reader.take_latest().expect("new frame")), 6);
        assert!(reader.latest().is_none());
    }

    #[test]
    fn mailbox_clear_drops_pending_frame() {
        let (mut writer, mut reader) = FrameMailbox::open();
        writer.submit(frame(9));
        writer.clear();
        assert!(reader.take_latest().is_none());
        assert!(reader.latest().is_none());
    }

    #[test]
    fn mailbox_sink_feeds_reader_across_threads() {
        let (writer, mut reader) = FrameMailbox::open();
        let mut sink = writer.into_sink();
        let producer = std::thread::spawn(move || {
            for i in 1..=50u8 {
                sink(frame(i));
            }
        });
        producer.join().expect("producer thread");
        let latest = reader.take_latest().expect("the last published frame");
        assert_eq!(tag_of(&latest), 50);
        assert_eq!(
            Arc::strong_count(&latest),
            1,
            "the reader owns the taken frame"
        );
    }

    #[test]
    fn mailbox_submit_arc_shares_the_frame() {
        let (mut writer, mut reader) = FrameMailbox::open();
        let shared = Arc::new(frame(7));
        writer.submit_arc(Arc::clone(&shared));
        let got = reader.take_latest().expect("frame");
        assert!(Arc::ptr_eq(&got, &shared));
        assert_eq!(got.decode_latency(), Duration::ZERO);
    }

    /// Port of `TestVideoFrameMailbox::slowConsumerReceivesOnlyLatestFrame`.
    #[test]
    fn slow_consumer_receives_only_latest_frame() {
        let (mut writer, mut reader) = FrameMailbox::open();
        let producer = std::thread::spawn(move || {
            for i in 0..100u8 {
                writer.submit(frame(i));
            }
            writer
        });
        let mut writer = producer.join().expect("producer thread");
        // The consumer has not polled yet: exactly one wake-up with the latest frame.
        assert!(reader.has_new());
        let first = reader.take_latest().expect("one delivery");
        assert_eq!(tag_of(&first), 99);
        assert!(
            reader.take_latest().is_none(),
            "no second delivery for the batch"
        );
        writer.submit(frame(0));
        assert!(
            reader.take_latest().is_some(),
            "the next submit wakes the consumer again"
        );
    }

    /// Port of `TestVideoFrameMailbox::nativeFramesStayBoundedAndReleaseOnClear`: at most
    /// one frame is retained, the delivered frame is owned by the consumer, `clear`
    /// releases the pending one.
    #[test]
    fn native_frames_stay_bounded_and_release_on_clear() {
        let (mut writer, mut reader) = FrameMailbox::open();
        let mut frames: Vec<Weak<Frame>> = Vec::new();
        for i in 0..100u8 {
            let shared = Arc::new(frame(i));
            frames.push(Arc::downgrade(&shared));
            writer.submit_arc(shared);
        }
        assert_eq!(released(&frames), 99);
        let latest = reader.take_latest().expect("one delivery");
        assert_eq!(tag_of(&latest), 99);
        assert!(reader.take_latest().is_none());
        drop(latest);
        assert_eq!(released(&frames), 100);

        let extra = Arc::new(frame(0));
        frames.push(Arc::downgrade(&extra));
        writer.submit_arc(extra);
        writer.clear();
        assert!(reader.take_latest().is_none(), "the wake-up finds no frame");
        assert_eq!(released(&frames), 101);
    }

    /// Port of `TestVideoFrameMailbox::clearAndDestroyCancelPendingFrames`.
    #[test]
    fn clear_and_destroy_cancel_pending_frames() {
        let (mut writer, mut reader) = FrameMailbox::open();
        writer.submit(frame(1));
        writer.clear();
        assert!(reader.take_latest().is_none());
        assert!(reader.latest().is_none());

        let (mut writer, reader) = FrameMailbox::open();
        let pending = Arc::new(frame(2));
        let weak = Arc::downgrade(&pending);
        writer.submit_arc(pending);
        drop(writer);
        assert_eq!(weak.strong_count(), 1, "still reachable through the reader");
        drop(reader);
        assert_eq!(
            weak.strong_count(),
            0,
            "destroying the mailbox releases the frame"
        );
    }
}
