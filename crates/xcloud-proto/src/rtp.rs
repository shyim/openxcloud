//! Port of qt/src/streaming/RtpPacket.{h,cpp}
//!
//! Fixed RTP header parsing shared by the media pipeline. Ported from green-nx
//! (src/switch/stream/video_jitter.cpp) so the jitter buffers and the pipeline
//! agree on header layout and padding handling.

use crate::{Error, Result};

/// A borrowed view of one RTP packet: the fixed header fields plus the payload
/// with CSRCs, the header extension and RTP padding already removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpPacketView<'a> {
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload_type: u8,
    pub marker: bool,
    pub payload: &'a [u8],
}

/// Parses the fixed RTP header, skipping CSRCs and the extension, and strips
/// RTP padding. Returns an error for a truncated or malformed packet.
///
/// Stripping padding matters: libwebrtc sends RTP padding packets (bandwidth
/// probing) on the video stream, and their bytes would otherwise be parsed as
/// a NALU, making a pure padding packet look like a lost video packet.
pub fn parse_rtp_packet(data: &[u8]) -> Result<RtpPacketView<'_>> {
    if data.len() < 12 {
        return Err(Error::MalformedPacket("rtp header truncated"));
    }

    let csrc_count = usize::from(data[0] & 0x0f);
    let has_extension = (data[0] & 0x10) != 0;
    let has_padding = (data[0] & 0x20) != 0;

    let marker = (data[1] & 0x80) != 0;
    let payload_type = data[1] & 0x7f;
    let sequence = u16::from_be_bytes([data[2], data[3]]);
    let timestamp = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

    let mut header = 12 + 4 * csrc_count;
    if data.len() < header {
        return Err(Error::MalformedPacket("rtp csrc list truncated"));
    }

    if has_extension {
        if data.len() < header + 4 {
            return Err(Error::MalformedPacket("rtp extension header truncated"));
        }
        let words = usize::from(u16::from_be_bytes([data[header + 2], data[header + 3]]));
        header += 4 + 4 * words;
        if data.len() < header {
            return Err(Error::MalformedPacket("rtp extension truncated"));
        }
    }

    let mut payload_length = data.len() - header;
    if has_padding && payload_length > 0 {
        let padding = usize::from(data[data.len() - 1]);
        payload_length = payload_length.saturating_sub(padding);
    }

    Ok(RtpPacketView {
        sequence,
        timestamp,
        ssrc,
        payload_type,
        marker,
        payload: &data[header..header + payload_length],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(first: u8, marker: bool, seq: u16, ts: u32, ssrc: u32) -> Vec<u8> {
        let mut out = vec![first, (if marker { 0x80 } else { 0x00 }) | 96];
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(&ts.to_be_bytes());
        out.extend_from_slice(&ssrc.to_be_bytes());
        out
    }

    #[test]
    fn parses_fixed_header_and_payload() {
        let mut packet = header(0x80, true, 0x1234, 0xdead_beef, 7);
        packet.extend_from_slice(&[0x65, 0x01, 0x02]);
        let view = parse_rtp_packet(&packet).unwrap();
        assert_eq!(view.sequence, 0x1234);
        assert_eq!(view.timestamp, 0xdead_beef);
        assert_eq!(view.ssrc, 7);
        assert_eq!(view.payload_type, 96);
        assert!(view.marker);
        assert_eq!(view.payload, &[0x65, 0x01, 0x02]);
    }

    #[test]
    fn rejects_truncated_header() {
        assert!(parse_rtp_packet(&[0x80; 11]).is_err());
        // One CSRC announced but not present.
        assert!(parse_rtp_packet(&header(0x81, false, 1, 1, 1)).is_err());
        // Extension flag but no extension header.
        assert!(parse_rtp_packet(&header(0x90, false, 1, 1, 1)).is_err());
        // Extension announces two words but only one is present.
        let mut packet = header(0x90, false, 1, 1, 1);
        packet.extend_from_slice(&[0xbe, 0xde, 0x00, 0x02, 1, 2, 3, 4]);
        assert!(parse_rtp_packet(&packet).is_err());
    }

    #[test]
    fn skips_csrcs_and_extension() {
        let mut packet = header(0x92, false, 1, 1, 1);
        packet.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 2]); // two CSRCs
        packet.extend_from_slice(&[0xbe, 0xde, 0x00, 0x01, 0xaa, 0xbb, 0xcc, 0xdd]); // 1 word
        packet.extend_from_slice(&[0x41, 0x9a]);
        let view = parse_rtp_packet(&packet).unwrap();
        assert_eq!(view.payload, &[0x41, 0x9a]);
    }

    #[test]
    fn strips_padding() {
        let mut packet = header(0xa0, false, 1, 1, 1);
        packet.extend_from_slice(&[0x41, 0x9a, 0x00, 0x00, 0x03]);
        let view = parse_rtp_packet(&packet).unwrap();
        assert_eq!(view.payload, &[0x41, 0x9a]);

        // A pure padding (probe) packet has an empty payload rather than
        // its padding bytes being mistaken for a NALU.
        let mut probe = header(0xa0, false, 2, 1, 1);
        probe.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 8]);
        let view = parse_rtp_packet(&probe).unwrap();
        assert!(view.payload.is_empty());

        // Padding count larger than the payload clamps to empty.
        let mut bogus = header(0xa0, false, 3, 1, 1);
        bogus.extend_from_slice(&[1, 2, 0xff]);
        let view = parse_rtp_packet(&bogus).unwrap();
        assert!(view.payload.is_empty());
    }
}
