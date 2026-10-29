//! Port of qt/src/streaming/AudioJitterBuffer.{h,cpp}
//!
//! Sequence-ordered reorder buffer for Opus RTP packets. Ported from green-nx
//! (src/switch/stream/audio_jitter.{hpp,cpp}), which mirrors green-vita's
//! SampleBuilder. Opus is stateful, so decoding packets out of order is
//! audible; this releases packets to the decoder strictly in sequence order,
//! holding briefly for a late packet and skipping a genuinely lost one once
//! too many pile up behind it.

/// Give up waiting for a missing packet once this many later ones have piled up
/// behind it. At one 20 ms Opus frame per packet that is far more slack than
/// any normal reordering needs.
const MAX_LATE: usize = 32;

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

    /// Moves the next in-sequence packet out; `None` while waiting for it.
    pub fn pop(&mut self) -> Option<Vec<u8>> {
        if self.pending.is_empty() {
            return None;
        }

        let mut index = self.find_seq(self.next_seq);
        if index.is_none() {
            // The next packet has not arrived. Wait for it unless too many later
            // packets piled up, then treat the gap as lost and skip ahead.
            if self.pending.len() < MAX_LATE {
                return None;
            }
            let mut oldest = self.pending[0].seq;
            for entry in &self.pending {
                if seq_before(entry.seq, oldest) {
                    oldest = entry.seq;
                }
            }
            self.lost += u32::from(oldest.wrapping_sub(self.next_seq));
            self.next_seq = oldest;
            index = self.find_seq(self.next_seq);
        }

        let index = index?;
        let entry = self.pending.remove(index);
        self.next_seq = self.next_seq.wrapping_add(1);
        Some(entry.data)
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

    #[test]
    fn releases_in_sequence_order() {
        let mut buffer = AudioJitterBuffer::new();
        buffer.push(1, &payload(1));
        let out = buffer.pop().unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], 1);
        assert!(buffer.pop().is_none()); // nothing more pending
    }

    #[test]
    fn reorders_out_of_order_arrival() {
        let mut buffer = AudioJitterBuffer::new();
        // The first packet establishes the stream start; later packets may arrive
        // out of order and are released in sequence order.
        buffer.push(1, &payload(1));
        buffer.push(3, &payload(3));
        buffer.push(2, &payload(2));
        assert_eq!(buffer.pop().unwrap()[0], 1);
        assert_eq!(buffer.pop().unwrap()[0], 2);
        assert_eq!(buffer.pop().unwrap()[0], 3);
        assert!(buffer.pop().is_none());
    }

    #[test]
    fn drops_duplicates() {
        let mut buffer = AudioJitterBuffer::new();
        buffer.push(1, &payload(1));
        buffer.push(1, &payload(1));
        buffer.push(2, &payload(2));
        assert_eq!(buffer.pop().unwrap()[0], 1);
        assert_eq!(buffer.pop().unwrap()[0], 2);
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
        assert_eq!(buffer.pop().unwrap()[0], 2);
    }

    #[test]
    fn skips_lost_after_backlog() {
        let mut buffer = AudioJitterBuffer::new();
        buffer.push(1, &payload(1));
        assert!(buffer.pop().is_some()); // next is now 2

        // 2 never arrives; pile up 33 later packets.
        for seq in 3..3 + 33 {
            buffer.push(seq as u16, &payload(seq & 0xff));
        }

        let out = buffer.pop().unwrap();
        // We skipped the lost seq 2 and released the oldest held packet.
        assert_eq!(out[0], 3);
        assert_eq!(buffer.lost(), 1);
    }

    #[test]
    fn reset_clears_state() {
        let mut buffer = AudioJitterBuffer::new();
        buffer.push(10, &payload(10));
        assert!(buffer.pop().is_some());
        buffer.reset();
        // A new stream may restart at any sequence number.
        buffer.push(5, &payload(5));
        assert_eq!(buffer.pop().unwrap()[0], 5);
        assert_eq!(buffer.lost(), 0);
    }

    #[test]
    fn sequence_wraparound() {
        let mut buffer = AudioJitterBuffer::new();
        buffer.push(0xffff, &payload(1));
        buffer.push(0, &payload(2));
        assert_eq!(buffer.pop().unwrap()[0], 1);
        assert_eq!(buffer.pop().unwrap()[0], 2);
        // 0xfffe is now "before" the next expected (1): dropped as late.
        buffer.push(0xfffe, &payload(9));
        assert!(buffer.pop().is_none());
        assert_eq!(buffer.pending_len(), 0);
    }
}
