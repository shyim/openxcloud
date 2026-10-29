//! Port of qt/src/streaming/InputPacket.{h,cpp}
//!
//! The binary little-endian input protocol sent over the `input` data channel,
//! plus the parsers for the two inbound report types the client cares about
//! (vibration and server metadata).
//!
//! Every outbound packet starts with a 14-byte header:
//!
//! ```text
//! offset  size  field
//! 0       2     report type bitmask (LE u16, see [`ReportType`])
//! 2       4     sequence number     (LE u32)
//! 6       8     timestamp           (LE f64, monotonic milliseconds)
//! ```
//!
//! The C++ reads a process-wide `QElapsedTimer` for the timestamp. Here the
//! caller passes `now_ms` so packets are deterministic in tests; the caller
//! (the input crate) keeps the monotonic clock.

use super::models::{FrameTimingMetadata, GamepadInputFrame};

/// Report type bits at the head of every outbound input packet.
///
/// A transparent `u16` bitmask; several bits can be set on one packet
/// (`METADATA | GAMEPAD` is the usual flush).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
#[repr(transparent)]
pub struct ReportType(pub u16);

impl ReportType {
    pub const NONE: Self = Self(0);
    pub const METADATA: Self = Self(1 << 0);
    pub const GAMEPAD: Self = Self(1 << 1);
    pub const POINTER: Self = Self(1 << 2);
    pub const CLIENT_METADATA: Self = Self(1 << 3);
    pub const SERVER_METADATA: Self = Self(1 << 4);
    pub const MOUSE: Self = Self(1 << 5);
    pub const KEYBOARD: Self = Self(1 << 6);
    pub const VIBRATION: Self = Self(1 << 7);
    pub const SENSOR: Self = Self(1 << 8);
    pub const UNRELIABLE_INPUT: Self = Self(1 << 9);
    pub const UNRELIABLE_ACK: Self = Self(1 << 10);

    #[inline]
    pub const fn bits(self) -> u16 {
        self.0
    }

    #[inline]
    pub const fn contains(self, flag: Self) -> bool {
        (self.0 & flag.0) == flag.0
    }
}

impl std::ops::BitOr for ReportType {
    type Output = Self;
    #[inline]
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for ReportType {
    #[inline]
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl std::ops::BitAnd for ReportType {
    type Output = Self;
    #[inline]
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

/// Vibration report received from the server over the input channel.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct VibrationReport {
    pub gamepad_index: u8,
    /// Motor strengths as `0.0..=1.0` (wire carries integer percent).
    pub left_motor_percent: f32,
    pub right_motor_percent: f32,
    pub left_trigger_motor_percent: f32,
    pub right_trigger_motor_percent: f32,
    pub duration_ms: u16,
    pub delay_ms: u16,
    pub repeat_count: u8,
}

/// Size of the common packet header.
pub const HEADER_SIZE: usize = 14;

/// Bytes per gamepad entry in a `GAMEPAD` report (after the 1-byte count).
pub const GAMEPAD_ENTRY_SIZE: usize = 23;

/// Bytes per timing entry in a `METADATA` report (after the 1-byte count).
pub const METADATA_ENTRY_SIZE: usize = 28;

#[inline]
fn write_le16(buffer: &mut [u8], offset: usize, value: u16) {
    buffer[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

#[inline]
fn write_le16_signed(buffer: &mut [u8], offset: usize, value: i16) {
    write_le16(buffer, offset, value as u16);
}

#[inline]
fn write_le32(buffer: &mut [u8], offset: usize, value: u32) {
    buffer[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[inline]
fn write_be32(buffer: &mut [u8], offset: usize, value: u32) {
    buffer[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

#[inline]
fn write_le_double(buffer: &mut [u8], offset: usize, value: f64) {
    buffer[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[inline]
fn read_le16(buffer: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([buffer[offset], buffer[offset + 1]])
}

#[inline]
fn read_le32(buffer: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        buffer[offset],
        buffer[offset + 1],
        buffer[offset + 2],
        buffer[offset + 3],
    ])
}

/// Stick axis `-1.0..=1.0` → `i16` by truncation toward zero (`qint16(clamped * 32767.0f)`).
#[inline]
fn normalize_axis(value: f32) -> i16 {
    let clamped = value.clamp(-1.0, 1.0);
    (clamped * 32767.0) as i16
}

/// Trigger `0.0..=1.0` → `u16` by truncation (`quint16(clamped * 65535.0f)`).
#[inline]
fn normalize_trigger(value: f32) -> u16 {
    let clamped = value.clamp(0.0, 1.0);
    (clamped * 65535.0) as u16
}

/// Allocates a zeroed packet of `HEADER_SIZE + payload_size` bytes with the header filled in.
fn make_header(
    report_type: ReportType,
    sequence: u32,
    payload_size: usize,
    now_ms: u64,
) -> Vec<u8> {
    let mut buffer = vec![0u8; HEADER_SIZE + payload_size];
    write_le16(&mut buffer, 0, report_type.bits());
    write_le32(&mut buffer, 2, sequence);
    write_le_double(&mut buffer, 6, now_ms as f64);
    buffer
}

/// Wire count byte: `char(qMin<int>(frames.size(), 255))`.
#[inline]
fn count_byte(len: usize) -> u8 {
    len.min(255) as u8
}

/// `GAMEPAD` report body: count byte followed by 23 bytes per frame.
///
/// Note the Y axes are negated on the wire relative to [`GamepadInputFrame`],
/// and the two "physicality" markers differ in endianness (LE then BE) exactly
/// as the browser client writes them.
fn encode_gamepads(frames: &[GamepadInputFrame]) -> Vec<u8> {
    let mut data = vec![0u8; 1 + frames.len() * GAMEPAD_ENTRY_SIZE];
    data[0] = count_byte(frames.len());
    let mut offset = 1;
    for frame in frames {
        data[offset] = frame.gamepad_index;
        offset += 1;
        write_le16(&mut data, offset, frame.buttons.bits());
        offset += 2;
        write_le16_signed(&mut data, offset, normalize_axis(frame.left_thumb.x));
        offset += 2;
        write_le16_signed(&mut data, offset, normalize_axis(-frame.left_thumb.y));
        offset += 2;
        write_le16_signed(&mut data, offset, normalize_axis(frame.right_thumb.x));
        offset += 2;
        write_le16_signed(&mut data, offset, normalize_axis(-frame.right_thumb.y));
        offset += 2;
        write_le16(&mut data, offset, normalize_trigger(frame.triggers.x));
        offset += 2;
        write_le16(&mut data, offset, normalize_trigger(frame.triggers.y));
        offset += 2;
        write_le32(&mut data, offset, 1); // PhysicalPhysicality
        offset += 4;
        write_be32(&mut data, offset, 1); // VirtualPhysicality
        offset += 4;
    }
    data
}

/// `METADATA` report body: count byte followed by 28 bytes (7 × LE u32) per frame.
fn encode_metadata(frames: &[FrameTimingMetadata], now_ms: u64) -> Vec<u8> {
    let mut data = vec![0u8; 1 + frames.len() * METADATA_ENTRY_SIZE];
    data[0] = count_byte(frames.len());
    let mut offset = 1;
    // `quint32(InputPacket::currentTimestampMs())`: wraps after ~49 days, as in C++.
    let now_ms32 = now_ms as u32;
    for frame in frames {
        write_le32(&mut data, offset, frame.server_data_key);
        offset += 4;
        write_le32(&mut data, offset, frame.first_frame_packet_arrival_time_ms);
        offset += 4;
        write_le32(&mut data, offset, frame.frame_submitted_time_ms);
        offset += 4;
        write_le32(&mut data, offset, frame.frame_decoded_time_ms);
        offset += 4;
        write_le32(&mut data, offset, frame.frame_rendered_time_ms);
        offset += 4;
        write_le32(&mut data, offset, now_ms32); // framePacketTime
        offset += 4;
        write_le32(&mut data, offset, now_ms32); // frameDateNow
        offset += 4;
    }
    data
}

/// Client metadata packet sent once when the input channel opens
/// (`InputPacket::clientMetadata`). 15 bytes: header + `max_touch_points`.
pub fn client_metadata(sequence: u32, max_touch_points: u8, now_ms: u64) -> Vec<u8> {
    let mut buffer = make_header(ReportType::CLIENT_METADATA, sequence, 1, now_ms);
    buffer[HEADER_SIZE] = max_touch_points;
    buffer
}

/// Gamepad (and optional timing metadata) packet sent on every flush
/// (`InputPacket::gamepadPacket`).
///
/// The metadata block, when present, precedes the gamepad block. With both
/// slices empty the result is a bare header with report type `NONE`; callers
/// ([`super::queue::InputQueue::flush`]) avoid that case.
pub fn gamepad_packet(
    sequence: u32,
    frames: &[GamepadInputFrame],
    timing_frames: &[FrameTimingMetadata],
    now_ms: u64,
) -> Vec<u8> {
    let mut report_type = ReportType::NONE;
    let metadata_bytes = if timing_frames.is_empty() {
        Vec::new()
    } else {
        report_type |= ReportType::METADATA;
        encode_metadata(timing_frames, now_ms)
    };

    let gamepad_bytes = if frames.is_empty() {
        Vec::new()
    } else {
        report_type |= ReportType::GAMEPAD;
        encode_gamepads(frames)
    };

    let mut buffer = make_header(
        report_type,
        sequence,
        metadata_bytes.len() + gamepad_bytes.len(),
        now_ms,
    );
    let mut offset = HEADER_SIZE;
    if !metadata_bytes.is_empty() {
        buffer[offset..offset + metadata_bytes.len()].copy_from_slice(&metadata_bytes);
        offset += metadata_bytes.len();
    }
    if !gamepad_bytes.is_empty() {
        buffer[offset..offset + gamepad_bytes.len()].copy_from_slice(&gamepad_bytes);
    }
    buffer
}

/// Extracts the sequence number from a packet header; `0` for buffers shorter than 6 bytes.
pub fn sequence_of(packet: &[u8]) -> u32 {
    if packet.len() < 6 {
        return 0;
    }
    read_le32(packet, 2)
}

/// Extracts the report type bitmask from a packet header; `0` for buffers shorter than 2 bytes.
pub fn report_type_of(packet: &[u8]) -> u16 {
    if packet.len() < 2 {
        return 0;
    }
    read_le16(packet, 0)
}

/// Extracts the header timestamp (LE f64 milliseconds); `None` for buffers shorter than the header.
pub fn timestamp_of(packet: &[u8]) -> Option<f64> {
    if packet.len() < HEADER_SIZE {
        return None;
    }
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&packet[6..14]);
    Some(f64::from_le_bytes(bytes))
}

/// Parses an inbound `VIBRATION` report (`InputPacket::parseVibration`).
///
/// Layout: `[0..2]` report type, `[2]` unused, `[3]` gamepad index, `[4..8]` four
/// motor percents (0-100), `[8..10]` duration LE, `[10..12]` delay LE, `[12]` repeat.
/// `None` when the buffer is shorter than 13 bytes.
pub fn parse_vibration(data: &[u8]) -> Option<VibrationReport> {
    if data.len() < 13 {
        return None;
    }
    Some(VibrationReport {
        gamepad_index: data[3],
        left_motor_percent: f32::from(data[4]) / 100.0,
        right_motor_percent: f32::from(data[5]) / 100.0,
        left_trigger_motor_percent: f32::from(data[6]) / 100.0,
        right_trigger_motor_percent: f32::from(data[7]) / 100.0,
        duration_ms: read_le16(data, 8),
        delay_ms: read_le16(data, 10),
        repeat_count: data[12],
    })
}

/// Parses an inbound `SERVER_METADATA` report (`InputPacket::parseServerMetadata`).
///
/// Returns `(width, height)`; on the wire height comes first at offset 2, width at
/// offset 6 (both LE u32). `None` when the buffer is shorter than 10 bytes.
pub fn parse_server_metadata(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 10 {
        return None;
    }
    let height = read_le32(data, 2);
    let width = read_le32(data, 6);
    Some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::models::{GamepadButtons, Vec2};

    // --- tst_inputpacket.cpp -------------------------------------------------

    #[test]
    fn client_metadata_layout() {
        let packet = client_metadata(0, 1, 0);
        assert_eq!(packet.len(), 15);
        // reportType = ClientMetadata (1 << 3) little endian.
        assert_eq!(packet[0], 8);
        assert_eq!(packet[1], 0);
        assert_eq!(report_type_of(&packet), 8);
        assert_eq!(packet[14], 1);
    }

    #[test]
    fn gamepad_packet_layout() {
        let frame = GamepadInputFrame {
            gamepad_index: 0,
            buttons: GamepadButtons::A | GamepadButtons::B,
            left_thumb: Vec2 { x: 0.5, y: 0.25 },
            right_thumb: Vec2 { x: 0.0, y: 0.0 },
            triggers: Vec2 { x: 1.0, y: 0.0 },
        };

        let packet = gamepad_packet(1, &[frame], &[], 0);
        assert_eq!(packet.len(), 14 + 1 + 23);
        assert_eq!(report_type_of(&packet), 2);
        assert_eq!(sequence_of(&packet), 1);

        // count + gamepad index
        assert_eq!(packet[14], 1);
        assert_eq!(packet[15], 0);
        // button mask 0x0030 little endian
        assert_eq!(packet[16], 0x30);
        assert_eq!(packet[17], 0x00);
        // left thumb X = 16383 (0x3FFF)
        assert_eq!(packet[18], 0xFF);
        assert_eq!(packet[19], 0x3F);
        // left thumb Y is inverted: -0.25 -> -8191 (0xE001)
        assert_eq!(packet[20], 0x01);
        assert_eq!(packet[21], 0xE0);
        // right stick zero
        assert_eq!(packet[22], 0x00);
        assert_eq!(packet[23], 0x00);
        assert_eq!(packet[24], 0x00);
        assert_eq!(packet[25], 0x00);
        // triggers: left full (0xFFFF), right zero
        assert_eq!(packet[26], 0xFF);
        assert_eq!(packet[27], 0xFF);
        assert_eq!(packet[28], 0x00);
        assert_eq!(packet[29], 0x00);
        // PhysicalPhysicality little endian 1 at 30, VirtualPhysicality big endian 1 at 34
        assert_eq!(packet[30], 0x01);
        assert_eq!(packet[33], 0x00);
        assert_eq!(packet[34], 0x00);
        assert_eq!(packet[37], 0x01);
    }

    #[test]
    fn vibration_parsing() {
        let mut data = vec![0u8; 13];
        data[0] = 128;
        data[3] = 2;
        data[4] = 50; // 0.50
        data[5] = 100; // 1.00
        data[6] = 0;
        data[7] = 25; // 0.25
        data[8] = 0x2C; // duration 300 LE
        data[9] = 0x01;
        data[10] = 0x0A; // delay 10
        data[11] = 0x00;
        data[12] = 3;

        let report = parse_vibration(&data).expect("vibration report");
        assert_eq!(report.gamepad_index, 2);
        assert_eq!(report.left_motor_percent, 0.5);
        assert_eq!(report.right_motor_percent, 1.0);
        assert_eq!(report.left_trigger_motor_percent, 0.0);
        assert_eq!(report.right_trigger_motor_percent, 0.25);
        assert_eq!(report.duration_ms, 300);
        assert_eq!(report.delay_ms, 10);
        assert_eq!(report.repeat_count, 3);
    }

    #[test]
    fn server_metadata_parsing() {
        let mut data = vec![0u8; 10];
        data[0] = 16;
        // height 1080 LE at offset 2
        data[2] = 0x38;
        data[3] = 0x04;
        // width 1920 LE at offset 6
        data[6] = 0x80;
        data[7] = 0x07;
        let (width, height) = parse_server_metadata(&data).expect("server metadata");
        assert_eq!(width, 1920);
        assert_eq!(height, 1080);
    }

    #[test]
    fn rejects_short_buffers() {
        assert!(parse_vibration(&[0u8; 5]).is_none());
        assert!(parse_server_metadata(&[0u8; 3]).is_none());
    }

    // --- additional coverage (no Qt counterpart) ----------------------------

    #[test]
    fn header_timestamp_is_le_double() {
        let packet = client_metadata(7, 1, 123_456);
        assert_eq!(sequence_of(&packet), 7);
        assert_eq!(&packet[6..14], &123_456f64.to_le_bytes());
        assert_eq!(timestamp_of(&packet), Some(123_456.0));
        assert_eq!(timestamp_of(&packet[..13]), None);
    }

    #[test]
    fn short_headers_read_as_zero() {
        assert_eq!(sequence_of(&[1, 2, 3, 4, 5]), 0);
        assert_eq!(report_type_of(&[1]), 0);
        assert_eq!(report_type_of(&[]), 0);
    }

    #[test]
    fn metadata_block_precedes_gamepad_block() {
        let timing = FrameTimingMetadata {
            server_data_key: 0x0102_0304,
            first_frame_packet_arrival_time_ms: 10,
            frame_submitted_time_ms: 20,
            frame_decoded_time_ms: 30,
            frame_rendered_time_ms: 40,
        };
        let frame = GamepadInputFrame {
            gamepad_index: 3,
            ..GamepadInputFrame::default()
        };
        let now_ms = 0x1_0000_0005; // > u32::MAX: wraps to 5 in the metadata block
        let packet = gamepad_packet(9, &[frame], &[timing], now_ms);

        assert_eq!(packet.len(), HEADER_SIZE + 1 + 28 + 1 + 23);
        assert_eq!(
            report_type_of(&packet),
            (ReportType::METADATA | ReportType::GAMEPAD).bits()
        );
        assert_eq!(report_type_of(&packet), 3);
        assert_eq!(sequence_of(&packet), 9);
        assert_eq!(timestamp_of(&packet), Some(now_ms as f64));

        // metadata: count, then 7 LE u32 values
        assert_eq!(packet[14], 1);
        assert_eq!(&packet[15..19], &[0x04, 0x03, 0x02, 0x01]);
        assert_eq!(&packet[19..23], &10u32.to_le_bytes());
        assert_eq!(&packet[23..27], &20u32.to_le_bytes());
        assert_eq!(&packet[27..31], &30u32.to_le_bytes());
        assert_eq!(&packet[31..35], &40u32.to_le_bytes());
        assert_eq!(&packet[35..39], &5u32.to_le_bytes()); // framePacketTime
        assert_eq!(&packet[39..43], &5u32.to_le_bytes()); // frameDateNow

        // gamepad block follows
        assert_eq!(packet[43], 1);
        assert_eq!(packet[44], 3);
    }

    #[test]
    fn metadata_only_packet() {
        let packet = gamepad_packet(2, &[], &[FrameTimingMetadata::default()], 1);
        assert_eq!(packet.len(), HEADER_SIZE + 1 + 28);
        assert_eq!(report_type_of(&packet), 1);
        assert_eq!(packet[14], 1);
    }

    #[test]
    fn empty_packet_is_bare_header() {
        let packet = gamepad_packet(0, &[], &[], 0);
        assert_eq!(packet.len(), HEADER_SIZE);
        assert_eq!(report_type_of(&packet), 0);
    }

    #[test]
    fn axis_and_trigger_normalisation_clamp_and_truncate() {
        assert_eq!(normalize_axis(1.0), 32767);
        assert_eq!(normalize_axis(-1.0), -32767);
        assert_eq!(normalize_axis(2.0), 32767);
        assert_eq!(normalize_axis(-5.0), -32767);
        assert_eq!(normalize_axis(0.5), 16383);
        assert_eq!(normalize_axis(-0.25), -8191);
        assert_eq!(normalize_axis(0.0), 0);

        assert_eq!(normalize_trigger(1.0), 0xFFFF);
        assert_eq!(normalize_trigger(0.0), 0);
        assert_eq!(normalize_trigger(-0.5), 0);
        assert_eq!(normalize_trigger(1.5), 0xFFFF);
        assert_eq!(normalize_trigger(0.5), 32767);
    }

    #[test]
    fn multiple_gamepads_are_laid_out_consecutively() {
        let frames = [
            GamepadInputFrame {
                gamepad_index: 0,
                buttons: GamepadButtons::MENU,
                ..GamepadInputFrame::default()
            },
            GamepadInputFrame {
                gamepad_index: 1,
                buttons: GamepadButtons::A,
                ..GamepadInputFrame::default()
            },
        ];
        let packet = gamepad_packet(5, &frames, &[], 0);
        assert_eq!(packet.len(), HEADER_SIZE + 1 + 2 * GAMEPAD_ENTRY_SIZE);
        assert_eq!(packet[14], 2);
        assert_eq!(packet[15], 0);
        assert_eq!(packet[16], GamepadButtons::MENU.bits() as u8);
        assert_eq!(packet[38], 1);
        assert_eq!(packet[39], GamepadButtons::A.bits() as u8);
    }

    #[test]
    fn count_byte_saturates_at_255() {
        assert_eq!(count_byte(0), 0);
        assert_eq!(count_byte(255), 255);
        assert_eq!(count_byte(300), 255);
    }
}
