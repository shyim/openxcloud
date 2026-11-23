//! Port of qt/src/streaming/AudioJitterBuffer.{h,cpp}
//!
//! Sequence-ordered buffer for Opus RTP packets. Ported from green-nx
//! (src/switch/stream/audio_jitter.{hpp,cpp}), which mirrors green-vita's
//! SampleBuilder. Opus is stateful, so packets go to the decoder strictly in
//! sequence order, and a gap is reported as [`Popped::Lost`] so the caller can
//! conceal it instead of feeding the decoder a discontinuity.
//!
//! Unlike the original, a missing packet is not waited for. Audio is not
//! retransmitted, so the only thing waiting can win is a reordered packet, and
//! it costs silence for as long as the wait lasts and a burst afterwards (the
//! original held up to 32 packets, over half a second, behind every lost one).

/// How many later packets must be waiting before a missing one counts as lost.
/// One: the moment its successor arrives. A packet that does arrive after its
/// successor is dropped as late, and concealed like a lost one.
const REORDER_WINDOW: usize = 1;

/// What [`AudioJitterBuffer::pop`] hands out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Popped {
    /// The next Opus payload in sequence.
    Packet(Vec<u8>),
    /// This many packets before the next one never arrived. The next `pop`
    /// returns the packet after the gap; [`AudioJitterBuffer::peek`] shows it,
    /// for decoding its in-band FEC copy of the last lost frame.
    Lost(u32),
}

struct Entry {
    seq: u16,
    data: Vec<u8>,
}

/// RFC 1982 serial comparison: is `a` earlier than `b` (mod 2^16)?
#[inline]
fn seq_before(a: u16, b: u16) -> bool {
    (a.wrapping_sub(b) as i16) < 0
}

#[derive(Default)]
pub struct AudioJitterBuffer {
    pending: Vec<Entry>,
    next_seq: u16,
    have_next: bool,
    lost: u32,
}

impl AudioJitterBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clears all state for a fresh RTP stream (the sequence number restarts).
    pub fn reset(&mut self) {
        self.pending.clear();
        self.next_seq = 0;
        self.have_next = false;
        self.lost = 0;
    }

    /// Cumulative count of packets skipped as lost.
    pub fn lost(&self) -> u32 {
        self.lost
    }

    /// Number of packets currently held waiting for release.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Adds one arriving Opus payload tagged with its RTP sequence number.
    pub fn push(&mut self, seq: u16, data: &[u8]) {
        if !self.have_next {
            self.next_seq = seq;
            self.have_next = true;
        }
        // Already delivered past this sequence: arrived too late, drop it.
        if seq_before(seq, self.next_seq) {
            return;
        }
        if self.pending.iter().any(|entry| entry.seq == seq) {
            return; // duplicate
        }
        self.pending.push(Entry {
            seq,
            data: data.to_vec(),
        });
    }

    /// The next in-sequence packet, or the size of the gap before it; `None`
    /// when nothing is buffered.
    pub fn pop(&mut self) -> Option<Popped> {
        if self.pending.is_empty() {
            return None;
        }
        if let Some(index) = self.find_seq(self.next_seq) {
            let entry = self.pending.remove(index);
            self.next_seq = self.next_seq.wrapping_add(1);
            return Some(Popped::Packet(entry.data));
        }
        // The next packet has not arrived but later ones have.
        if self.pending.len() < REORDER_WINDOW {
            return None;
        }
        let mut oldest = self.pending[0].seq;
        for entry in &self.pending {
            if seq_before(entry.seq, oldest) {
                oldest = entry.seq;
            }
        }
        let gap = u32::from(oldest.wrapping_sub(self.next_seq));
        self.lost += gap;
        self.next_seq = oldest;
        Some(Popped::Lost(gap))
    }

    /// The payload the next [`Self::pop`] returns as a packet, if it is buffered.
    pub fn peek(&self) -> Option<&[u8]> {
        self.find_seq(self.next_seq)
            .map(|index| self.pending[index].data.as_slice())
    }

    fn find_seq(&self, want: u16) -> Option<usize> {
        self.pending.iter().position(|entry| entry.seq == want)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(value: i32) -> Vec<u8> {
        vec![value as u8, (value + 1) as u8]
    }

    /// The first byte of the next packet; panics on a gap or an empty buffer.
    fn packet(buffer: &mut AudioJitterBuffer) -> u8 {
        match buffer.pop() {
            Some(Popped::Packet(data)) => data[0],
            other => panic!("expected a packet, got {other:?}"),
        }
    }

    #[test]
    fn releases_in_sequence_order() {
        let mut buffer = AudioJitterBuffer::new();
        buffer.push(1, &payload(1));
        buffer.push(2, &payload(2));
        assert_eq!(packet(&mut buffer), 1);
        assert_eq!(packet(&mut buffer), 2);
        assert!(buffer.pop().is_none()); // nothing more pending
    }

    #[test]
    fn a_gap_is_reported_as_soon_as_a_later_packet_arrives() {
        let mut buffer = AudioJitterBuffer::new();
        buffer.push(1, &payload(1));
        assert_eq!(packet(&mut buffer), 1);
        // 2 never arrives; 3 shows the gap immediately.
        buffer.push(3, &payload(3));
        assert_eq!(buffer.peek(), None, "the expected packet is missing");
        assert_eq!(buffer.pop(), Some(Popped::Lost(1)));
        assert_eq!(buffer.peek(), Some(&payload(3)[..]), "for the FEC decode");
        assert_eq!(packet(&mut buffer), 3);
        assert!(buffer.pop().is_none());
        assert_eq!(buffer.lost(), 1);
    }

    #[test]
    fn a_burst_loss_is_one_gap() {
        let mut buffer = AudioJitterBuffer::new();
        buffer.push(10, &payload(10));
        assert_eq!(packet(&mut buffer), 10);
        buffer.push(15, &payload(15));
        assert_eq!(buffer.pop(), Some(Popped::Lost(4)));
        assert_eq!(packet(&mut buffer), 15);
        assert_eq!(buffer.lost(), 4);
    }

    #[test]
    fn a_packet_arriving_after_its_successor_is_late() {
        let mut buffer = AudioJitterBuffer::new();
        buffer.push(1, &payload(1));
        assert_eq!(packet(&mut buffer), 1);
        buffer.push(3, &payload(3));
        assert_eq!(buffer.pop(), Some(Popped::Lost(1)));
        assert_eq!(packet(&mut buffer), 3);
        // 2 turns up now: its slot was concealed already.
        buffer.push(2, &payload(2));
        assert!(buffer.pop().is_none());
        assert_eq!(buffer.pending_len(), 0);
    }

    #[test]
    fn packets_pushed_together_are_ordered() {
        let mut buffer = AudioJitterBuffer::new();
        // Both are buffered before the first pop, so their order is restored.
        buffer.push(1, &payload(1));
        assert_eq!(packet(&mut buffer), 1);
        buffer.push(3, &payload(3));
        buffer.push(2, &payload(2));
        assert_eq!(packet(&mut buffer), 2);
        assert_eq!(packet(&mut buffer), 3);
        assert_eq!(buffer.lost(), 0);
    }

    #[test]
    fn drops_duplicates() {
        let mut buffer = AudioJitterBuffer::new();
        buffer.push(1, &payload(1));
        buffer.push(1, &payload(1));
        buffer.push(2, &payload(2));
        assert_eq!(packet(&mut buffer), 1);
        assert_eq!(packet(&mut buffer), 2);
        assert!(buffer.pop().is_none());
    }

    #[test]
    fn drops_packets_already_delivered() {
        let mut buffer = AudioJitterBuffer::new();
        buffer.push(1, &payload(1));
        assert!(buffer.pop().is_some());
        // seq 1 arrived again after delivery: too late.
        buffer.push(1, &payload(1));
        buffer.push(2, &payload(2));
        assert_eq!(packet(&mut buffer), 2);
    }

    #[test]
    fn reset_clears_state() {
        let mut buffer = AudioJitterBuffer::new();
        buffer.push(10, &payload(10));
        assert!(buffer.pop().is_some());
        buffer.reset();
        // A new stream may restart at any sequence number.
        buffer.push(5, &payload(5));
        assert_eq!(packet(&mut buffer), 5);
        assert_eq!(buffer.lost(), 0);
    }

    #[test]
    fn sequence_wraparound() {
        let mut buffer = AudioJitterBuffer::new();
        buffer.push(0xffff, &payload(1));
        buffer.push(0, &payload(2));
        assert_eq!(packet(&mut buffer), 1);
        assert_eq!(packet(&mut buffer), 2);
        // 0xfffe is now "before" the next expected (1): dropped as late.
        buffer.push(0xfffe, &payload(9));
        assert!(buffer.pop().is_none());
        assert_eq!(buffer.pending_len(), 0);
    }
}
