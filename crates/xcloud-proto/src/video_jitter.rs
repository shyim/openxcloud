//! Port of qt/src/streaming/VideoJitterBuffer.{h,cpp}
//!
//! RTP to H.264 access-unit reassembly with reordering. Ported from green-nx
//! (src/switch/stream/video_jitter.{hpp,cpp}), which in turn models green-vita.
//! A naive "assemble the current frame, drop it the instant the next timestamp
//! arrives" buffer drops any frame that lost a packet before the NACK
//! retransmit can arrive (~1 RTT), then feeds the decoder P-frames that
//! reference the dropped frame. This buffer instead:
//!   * holds incomplete frames for a short time so retransmits complete them,
//!   * emits complete frames strictly in timestamp (decode) order,
//!   * on unrecoverable loss drops and refuses output until a fresh IDR.
//!
//! # RTP H.264 payload cases (RFC 6184)
//!
//! Each RTP payload is depacketized into Annex-B (4-byte start codes):
//!
//! | NAL type | Mode            | Handling                                                        |
//! |----------|-----------------|-----------------------------------------------------------------|
//! | 1..=23   | single NAL unit | `00 00 00 01` + the payload verbatim                            |
//! | 24       | STAP-A          | each `u16 size` + NALU becomes `00 00 00 01` + NALU; stops at a zero or truncated size |
//! | 28       | FU-A            | start fragment (S bit) emits start code + reconstructed NAL header `(indicator & 0xe0) \| (fu_header & 0x1f)`, then every fragment appends its bytes after the 2-byte FU header |
//! | other    | STAP-B, MTAP16/24, FU-B, reserved | ignored (xCloud never sends them)                |
//!
//! A frame is a set of packets sharing one RTP timestamp. It is complete when a
//! packet carries the marker bit, the packets are contiguous in sequence up to
//! the marker, and the lowest-seq packet is a partition head (single NALU,
//! STAP-A or an FU-A start fragment). Contiguity across frames is not required
//! because libwebrtc padding/probe packets sit between frames and consume
//! sequence numbers; [`crate::rtp::parse_rtp_packet`] strips their padding so
//! they arrive here with an empty payload and only feed the sequence tracker.
//!
//! Per-packet work allocates at most one payload buffer (recycled from a small
//! pool when a frame is released) and the access unit is written exactly once
//! into a reusable scratch buffer that is handed to `emit` as a slice.

use std::collections::VecDeque;

use crate::rtp::parse_rtp_packet;

/// A 1080p H.264 IDR keyframe is hundreds of KB.
const MAX_ACCESS_UNIT_BYTES: usize = 2 * 1024 * 1024;
/// Allow enough time for a NACK round trip on a distant xCloud route. The added
/// delay applies only while a frame is already incomplete.
const HOLD_MS: u64 = 200;
const MAX_FRAMES: usize = 32;
/// Upper bound on recycled payload buffers kept around between frames.
const MAX_SPARE_PAYLOADS: usize = 64;
const START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// Counters for the HUD / health log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub packets: u32,
    pub frames: u32,
    pub dropped: u32,
    pub nacks: u32,
    pub resyncs: u32,
    pub last_frame_bytes: u32,
}

/// What the caller must do after [`VideoJitterBuffer::receive`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReceiveOutcome {
    /// Set when the caller should send a keyframe request (PLI / FIR).
    pub want_keyframe: bool,
}

/// RFC 3550 receiver-report loss fields, from [`VideoJitterBuffer::report_stats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReceiverReport {
    pub fraction_lost: u8,
    /// 24-bit cumulative number of packets lost.
    pub cumulative_lost: u32,
    /// Extended highest sequence number received (cycles + max seq).
    pub highest_seq_ext: u32,
}

struct Packet {
    seq: u16,
    marker: bool,
    payload: Vec<u8>,
}

struct Frame {
    timestamp: u32,
    first_seen_ms: u64,
    packets: Vec<Packet>,
}

pub struct VideoJitterBuffer {
    frames: VecDeque<Frame>,
    last_timestamp: Option<u32>,
    waiting_keyframe: bool,

    have_seq: bool,
    max_seq: u16,
    cycles: u32,
    base_seq: u32,
    received: u32,
    received_prior: u32,
    expected_prior: u32,

    stats: Stats,

    /// Scratch: the access unit being assembled (reused across calls).
    access_unit: Vec<u8>,
    /// Scratch: packet indices sorted by sequence (reused across calls).
    ordered: Vec<usize>,
    /// Recycled payload buffers from released frames.
    spare_payloads: Vec<Vec<u8>>,
}

impl Default for VideoJitterBuffer {
    fn default() -> Self {
        Self {
            frames: VecDeque::with_capacity(MAX_FRAMES),
            last_timestamp: None,
            waiting_keyframe: true,
            have_seq: false,
            max_seq: 0,
            cycles: 0,
            base_seq: 0,
            received: 0,
            received_prior: 0,
            expected_prior: 0,
            stats: Stats::default(),
            access_unit: Vec::new(),
            ordered: Vec::new(),
            spare_payloads: Vec::new(),
        }
    }
}

#[inline]
fn timestamp_newer(a: u32, b: u32) -> bool {
    let delta = a.wrapping_sub(b);
    delta != 0 && delta < (1u32 << 31)
}

#[inline]
fn nal_type(p: &[u8]) -> u8 {
    p.first().map_or(0, |b| b & 0x1f)
}

fn is_partition_head(p: &[u8]) -> bool {
    let ty = nal_type(p);
    if ty == 28 {
        return p.len() > 1 && (p[1] & 0x80) != 0; // FU-A start
    }
    (1..=24).contains(&ty) // single NALU or STAP-A
}

/// Appends the NAL unit(s) carried by one RTP payload to `au` as Annex-B.
fn depacketize(p: &[u8], au: &mut Vec<u8>) {
    if p.is_empty() {
        return;
    }
    let ty = nal_type(p);
    if (1..=23).contains(&ty) {
        au.extend_from_slice(&START_CODE);
        au.extend_from_slice(p);
    } else if ty == 24 {
        // STAP-A
        let mut i = 1;
        while i + 2 <= p.len() {
            let n = usize::from(u16::from_be_bytes([p[i], p[i + 1]]));
            i += 2;
            if n == 0 || i + n > p.len() {
                break;
            }
            au.extend_from_slice(&START_CODE);
            au.extend_from_slice(&p[i..i + n]);
            i += n;
        }
    } else if ty == 28 {
        // FU-A
        if p.len() < 2 {
            return;
        }
        if p[1] & 0x80 != 0 {
            // start
            au.extend_from_slice(&START_CODE);
            au.push((p[0] & 0xe0) | (p[1] & 0x1f));
        }
        au.extend_from_slice(&p[2..]);
    }
}

fn has_keyframe(au: &[u8]) -> bool {
    // IDR slices only (NAL type 5). xCloud repeats SPS/PPS in-band without an
    // IDR, and accepting a bare SPS resumed decode on P-frames whose references
    // were never decoded, producing self-sustaining green/pixelated garbage.
    au.windows(5)
        .any(|w| w[0] == 0 && w[1] == 0 && w[2] == 0 && w[3] == 1 && (w[4] & 0x1f) == 5)
}

/// Tries to assemble `frame` into `access_unit`. Returns the marker packet's
/// sequence number when the frame was complete and fits.
fn try_assemble(frame: &Frame, access_unit: &mut Vec<u8>, ordered: &mut Vec<usize>) -> Option<u16> {
    let marker = frame.packets.iter().find(|p| p.marker)?.seq;

    ordered.clear();
    ordered.extend(0..frame.packets.len());
    // Ascending sequence relative to the marker (largest distance first).
    ordered.sort_unstable_by_key(|&i| std::cmp::Reverse(marker.wrapping_sub(frame.packets[i].seq)));

    // Emit as soon as the frame is internally complete: its lowest-seq packet
    // must be a NALU/partition head and the packets must be contiguous up to the
    // marker. Contiguity across frames is deliberately not required, because RTP
    // padding/probing packets sit between frames and consume sequence numbers.
    let first = &frame.packets[ordered[0]];
    if !is_partition_head(&first.payload) {
        return None;
    }
    for pair in ordered.windows(2) {
        if frame.packets[pair[1]].seq != frame.packets[pair[0]].seq.wrapping_add(1) {
            return None;
        }
    }

    access_unit.clear();
    for &i in ordered.iter() {
        depacketize(&frame.packets[i].payload, access_unit);
        if access_unit.len() > MAX_ACCESS_UNIT_BYTES {
            return None;
        }
    }
    Some(marker)
}

impl VideoJitterBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        for frame in self.frames.drain(..) {
            // Keep the buffers; the stream restarts but the sizes stay similar.
            for packet in frame.packets {
                if self.spare_payloads.len() < MAX_SPARE_PAYLOADS {
                    let mut payload = packet.payload;
                    payload.clear();
                    self.spare_payloads.push(payload);
                }
            }
        }
        self.last_timestamp = None;
        self.waiting_keyframe = true;
        self.stats = Stats::default();
        self.have_seq = false;
        self.max_seq = 0;
        self.cycles = 0;
        self.base_seq = 0;
        self.received = 0;
        self.received_prior = 0;
        self.expected_prior = 0;
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// True while dropping everything until a real IDR arrives.
    pub fn waiting_keyframe(&self) -> bool {
        self.waiting_keyframe
    }

    /// Fills receiver-report loss statistics (RFC 3550 A.3). `None` when
    /// nothing was seen yet. Each call closes one report interval.
    pub fn report_stats(&mut self) -> Option<ReceiverReport> {
        if !self.have_seq {
            return None;
        }
        let extended_max = self.cycles.wrapping_add(u32::from(self.max_seq));
        let expected = extended_max.wrapping_sub(self.base_seq).wrapping_add(1);
        let lost = expected.saturating_sub(self.received);
        let expected_interval = expected.wrapping_sub(self.expected_prior);
        let received_interval = self.received.wrapping_sub(self.received_prior);
        self.expected_prior = expected;
        self.received_prior = self.received;
        let lost_interval = expected_interval.saturating_sub(received_interval);
        let fraction_lost = if expected_interval == 0 || lost_interval == 0 {
            0
        } else {
            ((lost_interval << 8) / expected_interval) as u8
        };
        Some(ReceiverReport {
            fraction_lost,
            cumulative_lost: lost & 0x00ff_ffff,
            highest_seq_ext: extended_max,
        })
    }

    /// Feeds one decrypted RTP packet at `now_ms` (monotonic milliseconds).
    ///
    /// `emit` receives every complete Annex-B access unit in timestamp order;
    /// `nack(pid, blp)` is called for each generic-NACK item to send. The
    /// returned outcome says whether a keyframe request is due.
    pub fn receive(
        &mut self,
        rtp: &[u8],
        now_ms: u64,
        mut emit: impl FnMut(&[u8]),
        nack: impl FnMut(u16, u16),
    ) -> ReceiveOutcome {
        self.receive_with_wait(rtp, now_ms, |au, _wait_ms| emit(au), nack)
    }

    /// [`VideoJitterBuffer::receive`] whose `emit` also gets the access unit's jitter
    /// wait: the milliseconds from the first RTP packet of the frame arriving
    /// (`Frame.first_seen_ms`) to the frame being emitted at `now_ms`. Zero when the frame
    /// completed with the packet that created it. The bytes emitted are identical to
    /// `receive`.
    pub fn receive_with_wait(
        &mut self,
        rtp: &[u8],
        now_ms: u64,
        mut emit: impl FnMut(&[u8], u64),
        mut nack: impl FnMut(u16, u16),
    ) -> ReceiveOutcome {
        let mut outcome = ReceiveOutcome::default();
        let Ok(view) = parse_rtp_packet(rtp) else {
            return outcome;
        };
        self.stats.packets += 1;

        // Global sequence tracking: NACK gaps and receiver-report statistics.
        if !self.have_seq {
            self.have_seq = true;
            self.base_seq = u32::from(view.sequence);
            self.max_seq = view.sequence;
            self.cycles = 0;
            self.received = 0;
        }
        self.received = self.received.wrapping_add(1);
        let seq_delta = view.sequence.wrapping_sub(self.max_seq);
        if seq_delta != 0 && seq_delta < 0x8000 {
            if view.sequence < self.max_seq {
                self.cycles = self.cycles.wrapping_add(0x10000);
            }
            let gap = seq_delta - 1;
            if gap > 0 && gap <= 255 {
                let mut pid = self.max_seq.wrapping_add(1);
                while pid != view.sequence {
                    let mut blp: u16 = 0;
                    let mut next = pid.wrapping_add(1);
                    let mut bit = 0;
                    while bit < 16 && next != view.sequence {
                        blp |= 1u16 << bit;
                        bit += 1;
                        next = next.wrapping_add(1);
                    }
                    nack(pid, blp);
                    self.stats.nacks += 1;
                    pid = next;
                }
            }
            self.max_seq = view.sequence;
        }

        if view.payload.is_empty() {
            return outcome; // padding or empty packet: nothing to assemble
        }

        let Some(index) = self.find_or_create(view.timestamp, now_ms) else {
            return outcome;
        };
        {
            let frame = &mut self.frames[index];
            if frame.packets.iter().any(|p| p.seq == view.sequence) {
                return outcome; // duplicate or retransmit
            }
            let mut payload = self.spare_payloads.pop().unwrap_or_default();
            payload.extend_from_slice(view.payload);
            frame.packets.push(Packet {
                seq: view.sequence,
                marker: view.marker,
                payload,
            });
        }

        // Drain: emit complete frames in timestamp order, time out the rest.
        while let Some(front) = self.frames.front() {
            if try_assemble(front, &mut self.access_unit, &mut self.ordered).is_some() {
                let wait_ms = now_ms.saturating_sub(front.first_seen_ms);
                self.last_timestamp = Some(front.timestamp);
                self.recycle_front();
                if self.waiting_keyframe {
                    if !has_keyframe(&self.access_unit) {
                        outcome.want_keyframe = true;
                        self.stats.resyncs += 1;
                        continue;
                    }
                    self.waiting_keyframe = false;
                }
                self.stats.frames += 1;
                self.stats.last_frame_bytes = self.access_unit.len() as u32;
                emit(&self.access_unit, wait_ms);
                continue;
            }
            if now_ms.saturating_sub(front.first_seen_ms) > HOLD_MS {
                tracing::debug!(
                    target: "Video",
                    timestamp = front.timestamp,
                    packets = front.packets.len(),
                    "dropping incomplete frame after hold; waiting for IDR"
                );
                self.last_timestamp = Some(front.timestamp);
                self.recycle_front();
                self.waiting_keyframe = true;
                outcome.want_keyframe = true;
                self.stats.dropped += 1;
                continue;
            }
            break;
        }
        outcome
    }

    /// Finds the frame with `timestamp` or inserts a new one in timestamp
    /// order. `None` when the timestamp is stale (already emitted/dropped past
    /// it) or the buffer is full.
    fn find_or_create(&mut self, timestamp: u32, now_ms: u64) -> Option<usize> {
        if let Some(i) = self.frames.iter().position(|f| f.timestamp == timestamp) {
            return Some(i);
        }
        if let Some(last) = self.last_timestamp
            && !timestamp_newer(timestamp, last)
        {
            return None;
        }
        if self.frames.len() >= MAX_FRAMES {
            return None;
        }

        let frame = Frame {
            timestamp,
            first_seen_ms: now_ms,
            packets: Vec::new(),
        };
        let mut index = 0;
        while index < self.frames.len() && timestamp_newer(timestamp, self.frames[index].timestamp)
        {
            index += 1;
        }
        self.frames.insert(index, frame);
        Some(index)
    }

    /// Pops the front frame and returns its payload buffers to the pool.
    fn recycle_front(&mut self) {
        if let Some(frame) = self.frames.pop_front() {
            for packet in frame.packets {
                if self.spare_payloads.len() < MAX_SPARE_PAYLOADS {
                    let mut payload = packet.payload;
                    payload.clear();
                    self.spare_payloads.push(payload);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtp_packet(seq: u16, timestamp: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
        let ssrc: u32 = 1;
        let mut packet = Vec::with_capacity(12 + payload.len());
        packet.push(0x80);
        packet.push((if marker { 0x80 } else { 0x00 }) | 96);
        packet.extend_from_slice(&seq.to_be_bytes());
        packet.extend_from_slice(&timestamp.to_be_bytes());
        packet.extend_from_slice(&ssrc.to_be_bytes());
        packet.extend_from_slice(payload);
        packet
    }

    fn no_nack(_: u16, _: u16) {}

    #[test]
    fn emits_complete_idr_frame() {
        let mut buffer = VideoJitterBuffer::new();
        let mut emitted: Vec<Vec<u8>> = Vec::new();
        // Single NALU IDR (type 5), marker set.
        let outcome = buffer.receive(
            &rtp_packet(1, 1000, true, &[0x65, 0x01, 0x02]),
            0,
            |au| emitted.push(au.to_vec()),
            no_nack,
        );

        assert_eq!(emitted.len(), 1);
        assert!(!outcome.want_keyframe);
        // Access unit starts with a 4-byte Annex-B start code then the IDR NAL.
        assert_eq!(emitted[0].len(), 7);
        assert_eq!(emitted[0][0], 0);
        assert_eq!(emitted[0][3], 1);
        assert_eq!(emitted[0][4] & 0x1f, 5);
        assert_eq!(buffer.stats().frames, 1);
    }

    #[test]
    fn gates_on_idr_until_keyframe() {
        let mut buffer = VideoJitterBuffer::new();
        let mut emitted = 0;
        // Non-IDR single NALU (type 1): must be dropped while waiting for an IDR.
        let outcome = buffer.receive(
            &rtp_packet(1, 1000, true, &[0x41, 0x01]),
            0,
            |_| emitted += 1,
            no_nack,
        );
        assert_eq!(emitted, 0);
        assert!(outcome.want_keyframe);
        assert!(buffer.waiting_keyframe());
        assert!(buffer.stats().resyncs >= 1);

        // A real IDR then releases output.
        buffer.receive(
            &rtp_packet(2, 2000, true, &[0x65, 0x01]),
            10,
            |_| emitted += 1,
            no_nack,
        );
        assert_eq!(emitted, 1);
        assert!(!buffer.waiting_keyframe());
    }

    #[test]
    fn reassembles_fu_a() {
        let mut buffer = VideoJitterBuffer::new();
        let mut emitted: Vec<Vec<u8>> = Vec::new();

        // FU-A fragmentation of an IDR: indicator 0x7c (F=0,NRI=3,type=28), 0x85 start.
        buffer.receive(
            &rtp_packet(1, 1000, false, &[0x7c, 0x85, 0xaa]),
            0,
            |au| emitted.push(au.to_vec()),
            no_nack,
        );
        buffer.receive(
            &rtp_packet(2, 1000, false, &[0x7c, 0x05, 0xbb]),
            5,
            |au| emitted.push(au.to_vec()),
            no_nack,
        );
        assert_eq!(emitted.len(), 0); // no marker yet

        buffer.receive(
            &rtp_packet(3, 1000, true, &[0x7c, 0x45, 0xcc]),
            10,
            |au| emitted.push(au.to_vec()),
            no_nack,
        );
        assert_eq!(emitted.len(), 1);
        // start code + reconstructed IDR header + one payload byte per fragment.
        let au = &emitted[0];
        assert_eq!(au.len(), 4 + 1 + 1 + 1 + 1);
        assert_eq!(au[4] & 0x1f, 5);
        assert_eq!(&au[5..], &[0xaa, 0xbb, 0xcc]);
    }

    #[test]
    fn emits_in_timestamp_order() {
        let mut buffer = VideoJitterBuffer::new();
        let mut emitted: Vec<Vec<u8>> = Vec::new();
        // Newer IDR (ts 2000) is emitted; the older frame (ts 1000) that arrives
        // afterwards is rejected as stale, so output never goes backwards.
        buffer.receive(
            &rtp_packet(1, 2000, true, &[0x65, 0xAA]),
            0,
            |au| emitted.push(au.to_vec()),
            no_nack,
        );
        buffer.receive(
            &rtp_packet(2, 1000, true, &[0x65, 0xBB]),
            1,
            |au| emitted.push(au.to_vec()),
            no_nack,
        );
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0][5], 0xAA);
    }

    #[test]
    fn generates_nack_for_gap() {
        let mut buffer = VideoJitterBuffer::new();
        let mut nacks: Vec<(u16, u16)> = Vec::new();
        // Seq 1 arrives, then seq 4 -> 2 and 3 are missing.
        buffer.receive(
            &rtp_packet(1, 1000, false, &[0x65, 0x01]),
            0,
            |_| {},
            |pid, blp| nacks.push((pid, blp)),
        );
        buffer.receive(
            &rtp_packet(4, 1000, true, &[0x65, 0x04]),
            5,
            |_| {},
            |pid, blp| nacks.push((pid, blp)),
        );
        assert_eq!(nacks.len(), 1);
        assert_eq!(nacks[0].0, 2);
        // Bit 0 covers seq 3.
        assert_eq!(nacks[0].1 & 0x0001, 1);
        assert!(buffer.stats().nacks >= 1);
    }

    #[test]
    fn drops_incomplete_frame_after_hold() {
        let mut buffer = VideoJitterBuffer::new();
        let mut emitted = 0;
        // IDR frame that is missing its marker packet: never complete.
        let first = buffer.receive(
            &rtp_packet(1, 1000, false, &[0x65, 0x01]),
            0,
            |_| emitted += 1,
            no_nack,
        );
        // After the hold window, the incomplete frame is dropped and a keyframe is
        // requested again.
        let second = buffer.receive(
            &rtp_packet(5, 2000, true, &[0x65, 0x02]),
            500,
            |_| emitted += 1,
            no_nack,
        );
        assert!(buffer.stats().dropped >= 1);
        assert!(first.want_keyframe || second.want_keyframe);
    }

    // --- Additional coverage beyond tst_videojitter.cpp ---

    #[test]
    fn retransmit_completes_held_frame() {
        let mut buffer = VideoJitterBuffer::new();
        let mut emitted: Vec<Vec<u8>> = Vec::new();
        let mut nacks: Vec<(u16, u16)> = Vec::new();
        // FU-A IDR: start, (lost middle), end with marker.
        buffer.receive(
            &rtp_packet(1, 1000, false, &[0x7c, 0x85, 0xaa]),
            0,
            |au| emitted.push(au.to_vec()),
            |p, b| nacks.push((p, b)),
        );
        buffer.receive(
            &rtp_packet(3, 1000, true, &[0x7c, 0x45, 0xcc]),
            10,
            |au| emitted.push(au.to_vec()),
            |p, b| nacks.push((p, b)),
        );
        assert_eq!(nacks, vec![(2, 0)]);
        assert!(emitted.is_empty());
        // Retransmit arrives within the hold window and completes the frame.
        let outcome = buffer.receive(
            &rtp_packet(2, 1000, false, &[0x7c, 0x05, 0xbb]),
            60,
            |au| emitted.push(au.to_vec()),
            |p, b| nacks.push((p, b)),
        );
        assert!(!outcome.want_keyframe);
        assert_eq!(emitted.len(), 1);
        assert_eq!(&emitted[0][5..], &[0xaa, 0xbb, 0xcc]);
        assert_eq!(buffer.stats().dropped, 0);
    }

    #[test]
    fn stap_a_splits_aggregated_nalus() {
        let mut buffer = VideoJitterBuffer::new();
        let mut emitted: Vec<Vec<u8>> = Vec::new();
        // STAP-A carrying SPS (type 7, 2 bytes), PPS (type 8, 1 byte), IDR (type 5, 2 bytes).
        let payload = [
            0x78, 0x00, 0x02, 0x67, 0x42, 0x00, 0x01, 0x68, 0x00, 0x02, 0x65, 0x88,
        ];
        buffer.receive(
            &rtp_packet(1, 1000, true, &payload),
            0,
            |au| emitted.push(au.to_vec()),
            no_nack,
        );
        assert_eq!(emitted.len(), 1);
        assert_eq!(
            emitted[0],
            vec![
                0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65, 0x88
            ]
        );
    }

    #[test]
    fn padding_only_packets_feed_sequence_tracking_without_a_frame() {
        let mut buffer = VideoJitterBuffer::new();
        let mut emitted = 0;
        let mut nacks = 0;
        buffer.receive(
            &rtp_packet(1, 1000, true, &[0x65, 0x01]),
            0,
            |_| emitted += 1,
            |_, _| nacks += 1,
        );
        // Pure padding probe packet (P bit set, payload is all padding).
        let mut probe = rtp_packet(2, 1000, false, &[0, 0, 0, 4]);
        probe[0] |= 0x20;
        buffer.receive(&probe, 1, |_| emitted += 1, |_, _| nacks += 1);
        // Next frame is contiguous with the probe: no NACK, emitted normally.
        buffer.receive(
            &rtp_packet(3, 2000, true, &[0x41, 0x01]),
            17,
            |_| emitted += 1,
            |_, _| nacks += 1,
        );
        assert_eq!(emitted, 2);
        assert_eq!(nacks, 0);
        assert_eq!(buffer.stats().packets, 3);
        let report = buffer.report_stats().unwrap();
        assert_eq!(report.highest_seq_ext, 3);
        assert_eq!(report.cumulative_lost, 0);
        assert_eq!(report.fraction_lost, 0);
    }

    #[test]
    fn report_stats_counts_loss_and_cycles() {
        let mut buffer = VideoJitterBuffer::new();
        assert!(buffer.report_stats().is_none());
        buffer.receive(
            &rtp_packet(0xfffe, 1000, true, &[0x65, 0x01]),
            0,
            |_| {},
            no_nack,
        );
        // 0xffff lost; 0 and 1 arrive after the wrap.
        buffer.receive(
            &rtp_packet(0, 2000, true, &[0x41, 0x01]),
            16,
            |_| {},
            no_nack,
        );
        buffer.receive(
            &rtp_packet(1, 3000, true, &[0x41, 0x01]),
            33,
            |_| {},
            no_nack,
        );
        let report = buffer.report_stats().unwrap();
        assert_eq!(report.highest_seq_ext, 0x10001);
        assert_eq!(report.cumulative_lost, 1);
        // 4 expected, 3 received: 1/4 * 256 = 64.
        assert_eq!(report.fraction_lost, 64);
        // Second interval with no loss reports zero fraction, same cumulative.
        buffer.receive(
            &rtp_packet(2, 4000, true, &[0x41, 0x01]),
            50,
            |_| {},
            no_nack,
        );
        let report = buffer.report_stats().unwrap();
        assert_eq!(report.fraction_lost, 0);
        assert_eq!(report.cumulative_lost, 1);
        assert_eq!(report.highest_seq_ext, 0x10002);
    }

    #[test]
    fn nack_covers_long_gaps_with_multiple_items() {
        let mut buffer = VideoJitterBuffer::new();
        let mut nacks: Vec<(u16, u16)> = Vec::new();
        buffer.receive(
            &rtp_packet(10, 1000, true, &[0x65, 0x01]),
            0,
            |_| {},
            |p, b| nacks.push((p, b)),
        );
        // 11..=30 missing (20 packets): one full item (pid 11 + 16 bits) + one with 2 bits.
        buffer.receive(
            &rtp_packet(31, 2000, true, &[0x41, 0x01]),
            5,
            |_| {},
            |p, b| nacks.push((p, b)),
        );
        assert_eq!(nacks, vec![(11, 0xffff), (28, 0x0003)]);
        assert_eq!(buffer.stats().nacks, 2);
        // Gaps above 255 packets are treated as a stream jump: no NACK burst.
        nacks.clear();
        buffer.receive(
            &rtp_packet(1000, 3000, true, &[0x41, 0x01]),
            10,
            |_| {},
            |p, b| nacks.push((p, b)),
        );
        assert!(nacks.is_empty());
    }

    #[test]
    fn reset_clears_state() {
        let mut buffer = VideoJitterBuffer::new();
        buffer.receive(
            &rtp_packet(1, 1000, true, &[0x65, 0x01]),
            0,
            |_| {},
            no_nack,
        );
        buffer.receive(
            &rtp_packet(2, 2000, false, &[0x41, 0x01]),
            16,
            |_| {},
            no_nack,
        );
        assert!(!buffer.waiting_keyframe());
        buffer.reset();
        assert!(buffer.waiting_keyframe());
        assert_eq!(buffer.stats(), Stats::default());
        assert!(buffer.report_stats().is_none());
        // An older timestamp is accepted again after reset.
        let mut emitted = 0;
        buffer.receive(
            &rtp_packet(7, 500, true, &[0x65, 0x01]),
            20,
            |_| emitted += 1,
            no_nack,
        );
        assert_eq!(emitted, 1);
    }

    #[test]
    fn receive_with_wait_reports_the_hold_since_the_first_packet() {
        let mut buffer = VideoJitterBuffer::new();
        let mut emitted: Vec<(Vec<u8>, u64)> = Vec::new();
        // Single-packet IDR: complete on arrival, no wait.
        buffer.receive_with_wait(
            &rtp_packet(1, 1000, true, &[0x65, 0x01]),
            100,
            |au, wait| emitted.push((au.to_vec(), wait)),
            no_nack,
        );
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].1, 0);
        // FU-A P-frame whose first fragment arrives at 110 and the marker at 123.
        buffer.receive_with_wait(
            &rtp_packet(2, 2000, false, &[0x5c, 0x81, 0xaa]),
            110,
            |au, wait| emitted.push((au.to_vec(), wait)),
            no_nack,
        );
        buffer.receive_with_wait(
            &rtp_packet(3, 2000, true, &[0x5c, 0x41, 0xbb]),
            123,
            |au, wait| emitted.push((au.to_vec(), wait)),
            no_nack,
        );
        assert_eq!(emitted.len(), 2);
        assert_eq!(emitted[1].1, 13);
        // Same bytes as `receive` would have produced.
        assert_eq!(emitted[1].0, vec![0, 0, 0, 1, 0x41, 0xaa, 0xbb]);
    }
}
