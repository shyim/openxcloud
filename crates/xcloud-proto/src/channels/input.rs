//! Port of qt/src/streaming/channels/InputChannel.{h,cpp}
//!
//! Binary gamepad input channel with a 125 Hz flush loop. Sends ClientMetadata on
//! open, flushes queued gamepad frames every [`InputChannel::LOOP_INTERVAL_MS`] and
//! parses inbound vibration and server-metadata reports. Mirrors channel/input.ts.
//!
//! Sans-IO port of the `QObject` + `QTimer` + `QFutureWatcher` design:
//!
//! * the `QTimer` becomes the `next_tick_due_ms` deadline driven by
//!   [`InputChannel::tick`];
//! * the `QFuture<Result<void>>` of `WebRtcBridge::send` becomes
//!   [`InputChannel::on_send_result`]: while one send is in flight, newer packets
//!   replace the single `pending` slot (newest state wins, the sequence number
//!   inside the packet still increases), and the pending packet goes out when the
//!   in-flight send completes. The driver must deliver exactly one
//!   `on_send_result` for every `SendBinary`, including after [`InputChannel::destroy`]:
//!   like the C++ `QFutureWatcher`, which outlives `destroy()`, the completion of
//!   a send started before `destroy()` is what clears the in-flight flag and
//!   releases a ClientMetadata parked by the next [`InputChannel::on_open`];
//! * the `InputQueue*` the C++ held is passed in through [`InputPacketSource`] so
//!   the channel does not depend on the queue's construction.

use std::collections::VecDeque;

use crate::input::packet::{self, ReportType};
use crate::streaming_types::DataChannelKind;

pub use super::control::ChannelAction;

pub use crate::input::queue::InputPacketSource;

/// Sends ClientMetadata on open, flushes queued gamepad frames at ~125 Hz and
/// parses inbound vibration and server-metadata reports.
#[derive(Debug, Clone, PartialEq)]
pub struct InputChannel {
    running: bool,
    send_in_flight: bool,
    pending: Option<Vec<u8>>,
    next_tick_due_ms: Option<u64>,
    server_video_width: u32,
    server_video_height: u32,
    packets_sent: u32,
    send_successes: u64,
    send_failures: u64,
    last_send_ms: Option<u64>,
    intervals: VecDeque<u64>,
    last_telemetry_ms: u64,
    flush_hz: f64,
    flush_jitter_ms: f64,
}

impl Default for InputChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl InputChannel {
    pub const LABEL: &'static str = "input";
    pub const PROTOCOL_NAME: &'static str = "1.0";
    /// Loop cadence in milliseconds (125 Hz).
    pub const LOOP_INTERVAL_MS: u64 = 8;
    /// Intervals longer than this reset the cadence window (controller idle).
    const ACTIVE_WINDOW_MAX_INTERVAL_MS: u64 = 750;
    /// Size of the rolling interval window used for the Hz/jitter telemetry.
    const INTERVAL_WINDOW: usize = 256;
    const TELEMETRY_PERIOD_MS: u64 = 1000;
    const LOG_EVERY_N: u64 = 625;

    const KIND: DataChannelKind = DataChannelKind::Input;

    /// `InputChannel(bridge, queue)`.
    pub fn new() -> Self {
        Self {
            running: false,
            send_in_flight: false,
            pending: None,
            next_tick_due_ms: None,
            server_video_width: 1920,
            server_video_height: 1080,
            packets_sent: 0,
            send_successes: 0,
            send_failures: 0,
            last_send_ms: None,
            intervals: VecDeque::with_capacity(Self::INTERVAL_WINDOW),
            last_telemetry_ms: 0,
            flush_hz: 0.0,
            flush_jitter_ms: 0.0,
        }
    }

    /// `onOpen`: sends the client metadata and starts the flush loop. A second
    /// open while running does nothing (tst_streamcontroller: exactly one
    /// metadata packet).
    pub fn on_open(
        &mut self,
        queue: &mut impl InputPacketSource,
        now_ms: u64,
    ) -> Vec<ChannelAction> {
        if self.running {
            return Vec::new();
        }
        tracing::info!(target: "InputChannel", "onOpen: sending client metadata and starting loop");
        let mut actions = Vec::new();
        let metadata = queue.make_initial_metadata(now_ms);
        self.send_packet(metadata, &mut actions);

        self.running = true;
        self.next_tick_due_ms = Some(now_ms + Self::LOOP_INTERVAL_MS);
        actions
    }

    /// The `QTimer::timeout` slot. Does nothing while stopped or before the next
    /// deadline; otherwise flushes the queue, sends the packet (or parks it while a
    /// send is in flight) and updates the flush telemetry once a second.
    pub fn tick(&mut self, queue: &mut impl InputPacketSource, now_ms: u64) -> Vec<ChannelAction> {
        let mut actions = Vec::new();
        if !self.running {
            return actions;
        }
        match self.next_tick_due_ms {
            Some(due) if now_ms < due => return actions,
            _ => self.next_tick_due_ms = Some(now_ms + Self::LOOP_INTERVAL_MS),
        }

        if let Some(packet) = queue.flush(now_ms) {
            self.send_packet(packet, &mut actions);
        }

        if let Some(last) = self.last_send_ms {
            let interval = now_ms.saturating_sub(last);
            if interval <= Self::ACTIVE_WINDOW_MAX_INTERVAL_MS {
                if self.intervals.len() >= Self::INTERVAL_WINDOW {
                    self.intervals.pop_front();
                }
                self.intervals.push_back(interval);
            } else {
                self.intervals.clear();
            }
        }
        self.last_send_ms = Some(now_ms);

        if now_ms.saturating_sub(self.last_telemetry_ms) >= Self::TELEMETRY_PERIOD_MS
            && !self.intervals.is_empty()
        {
            self.last_telemetry_ms = now_ms;
            let count = self.intervals.len() as f64;
            let mean = self.intervals.iter().map(|&v| v as f64).sum::<f64>() / count;
            let variance = self
                .intervals
                .iter()
                .map(|&v| {
                    let delta = v as f64 - mean;
                    delta * delta
                })
                .sum::<f64>()
                / count;
            self.flush_hz = if mean > 0.0 { 1000.0 / mean } else { 0.0 };
            self.flush_jitter_ms = variance.max(0.0).sqrt();
            actions.push(ChannelAction::FlushTelemetry {
                hz: self.flush_hz,
                jitter_ms: self.flush_jitter_ms,
            });
        }
        actions
    }

    /// Completion of the `SendBinary` currently in flight (the `QFutureWatcher`
    /// callback): records success/failure and releases the parked packet, if any.
    pub fn on_send_result(&mut self, ok: bool) -> Vec<ChannelAction> {
        let mut actions = Vec::new();
        if !self.send_in_flight {
            return actions;
        }
        if ok {
            self.send_successes += 1;
            if self.send_successes == 1 || self.send_successes.is_multiple_of(Self::LOG_EVERY_N) {
                tracing::info!(
                    target: "InputChannel",
                    "Input packets sent: {}, failed: {}",
                    self.send_successes,
                    self.send_failures
                );
            }
        } else {
            self.send_failures += 1;
            if self.send_failures <= 3 || self.send_failures.is_multiple_of(Self::LOG_EVERY_N) {
                tracing::warn!(target: "InputChannel", "Input send failed ({})", self.send_failures);
            }
        }
        self.send_in_flight = false;
        if let Some(next) = self.pending.take() {
            self.send_packet(next, &mut actions);
        }
        actions
    }

    /// `onMessage`: inbound binary report. Byte 0 is the low byte of the report
    /// type: `Vibration` (128) yields [`ChannelAction::Vibration`], `ServerMetadata`
    /// (16) updates the server video size and yields [`ChannelAction::ServerMetadata`].
    pub fn on_binary(&mut self, data: &[u8]) -> Vec<ChannelAction> {
        let Some(&report_type) = data.first() else {
            return Vec::new();
        };
        // The C++ compares the first byte against the truncated enum value.
        if report_type == ReportType::VIBRATION.bits() as u8 {
            if let Some(report) = packet::parse_vibration(data) {
                return vec![ChannelAction::Vibration(report)];
            }
        } else if report_type == ReportType::SERVER_METADATA.bits() as u8
            && let Some((width, height)) = packet::parse_server_metadata(data)
        {
            self.server_video_width = width;
            self.server_video_height = height;
            return vec![ChannelAction::ServerMetadata { width, height }];
        }
        Vec::new()
    }

    /// `destroy`: stops the loop and drops the parked packet and cadence window.
    /// Counters and the last server metadata survive, as in the C++.
    ///
    /// A send still in flight stays in flight: the C++ `destroy()` does not touch
    /// `m_sendInFlight`, the still-alive `QFutureWatcher` clears it when that send
    /// completes. The driver therefore keeps delivering
    /// [`on_send_result`](Self::on_send_result) for sends issued before `destroy()`;
    /// until it does, a following [`on_open`](Self::on_open) parks its
    /// ClientMetadata in `pending` instead of putting two sends in flight.
    pub fn destroy(&mut self) {
        self.running = false;
        self.next_tick_due_ms = None;
        self.pending = None;
        self.intervals.clear();
        self.last_send_ms = None;
    }

    /// When [`tick`](Self::tick) next has work, or `None` while stopped.
    pub fn next_tick_due_ms(&self) -> Option<u64> {
        self.next_tick_due_ms
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// True while a `SendBinary` awaits its [`on_send_result`](Self::on_send_result).
    pub fn is_send_in_flight(&self) -> bool {
        self.send_in_flight
    }

    pub fn flush_hz(&self) -> f64 {
        self.flush_hz
    }

    pub fn flush_jitter_ms(&self) -> f64 {
        self.flush_jitter_ms
    }

    pub fn server_video_width(&self) -> u32 {
        self.server_video_width
    }

    pub fn server_video_height(&self) -> u32 {
        self.server_video_height
    }

    /// Packets handed to the transport (not counting ones still parked).
    pub fn packets_sent(&self) -> u32 {
        self.packets_sent
    }

    pub fn send_successes(&self) -> u64 {
        self.send_successes
    }

    pub fn send_failures(&self) -> u64 {
        self.send_failures
    }

    /// `sendPacket`: one send in flight at a time; newer packets replace the
    /// single pending slot.
    fn send_packet(&mut self, packet: Vec<u8>, actions: &mut Vec<ChannelAction>) {
        if self.send_in_flight {
            self.pending = Some(packet);
            return;
        }
        self.send_in_flight = true;
        self.packets_sent += 1;
        actions.push(ChannelAction::SendBinary {
            kind: Self::KIND,
            bytes: packet,
        });
    }
}

#[cfg(test)]
mod tests {
    //! tst_streamcontroller.cpp (inputWaitsForHandshake: exactly one metadata
    //! packet per open; decodedResolutionOverridesServerMetadata: the inbound
    //! ServerMetadata vector) plus the vibration vector from tst_inputpacket.cpp,
    //! and the send/pending state machine from InputChannel.cpp.
    use super::*;
    use crate::input::packet::VibrationReport;

    /// A scripted `InputQueue`: hands out numbered packets on demand.
    #[derive(Default)]
    struct FakeQueue {
        sequence: u32,
        /// Frames waiting to be flushed; each flush produces one packet.
        armed: bool,
        metadata_calls: usize,
        flush_calls: usize,
    }

    impl FakeQueue {
        fn arm(&mut self) {
            self.armed = true;
        }

        fn next_sequence(&mut self) -> u32 {
            let seq = self.sequence;
            self.sequence += 1;
            seq
        }
    }

    impl InputPacketSource for FakeQueue {
        fn make_initial_metadata(&mut self, now_ms: u64) -> Vec<u8> {
            self.metadata_calls += 1;
            let seq = self.next_sequence();
            packet::client_metadata(seq, 1, now_ms)
        }

        fn flush(&mut self, now_ms: u64) -> Option<Vec<u8>> {
            self.flush_calls += 1;
            let seq = self.next_sequence();
            if !self.armed {
                return None;
            }
            self.armed = false;
            // A gamepad-shaped packet is not needed; any bytes carrying the
            // sequence will do for the channel, which treats packets as opaque.
            Some(packet::client_metadata(seq, 0, now_ms))
        }
    }

    fn sent(actions: &[ChannelAction]) -> Vec<&[u8]> {
        actions
            .iter()
            .filter_map(|action| match action {
                ChannelAction::SendBinary { kind, bytes } => {
                    assert_eq!(*kind, DataChannelKind::Input);
                    Some(bytes.as_slice())
                }
                _ => None,
            })
            .collect()
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn constants_match_cpp() {
        assert_eq!(InputChannel::LABEL, "input");
        assert_eq!(InputChannel::PROTOCOL_NAME, "1.0");
        assert_eq!(InputChannel::LOOP_INTERVAL_MS, 8);
    }

    #[test]
    fn open_sends_client_metadata_once_and_starts_loop() {
        let mut queue = FakeQueue::default();
        let mut channel = InputChannel::new();
        assert!(!channel.is_running());
        assert_eq!(channel.next_tick_due_ms(), None);

        let actions = channel.on_open(&mut queue, 100);
        let packets = sent(&actions);
        assert_eq!(packets.len(), 1);
        // ClientMetadata (1 << 3) little endian, sequence 0, maxTouchPoints 1.
        assert_eq!(packets[0].len(), 15);
        assert_eq!(packet::report_type_of(packets[0]), 8);
        assert_eq!(packet::sequence_of(packets[0]), 0);
        assert_eq!(packets[0][14], 1);
        assert!(channel.is_running());
        assert!(channel.is_send_in_flight());
        assert_eq!(channel.packets_sent(), 1);
        assert_eq!(channel.next_tick_due_ms(), Some(108));

        // tst_streamcontroller: a second open does not send another metadata packet.
        assert!(channel.on_open(&mut queue, 110).is_empty());
        assert_eq!(queue.metadata_calls, 1);
    }

    #[test]
    fn tick_is_gated_by_deadline_and_flushes_queue() {
        let mut queue = FakeQueue::default();
        let mut channel = InputChannel::new();
        // Not running: nothing, queue untouched.
        assert!(channel.tick(&mut queue, 0).is_empty());
        assert_eq!(queue.flush_calls, 0);

        channel.on_open(&mut queue, 0);
        channel.on_send_result(true);
        // Before the deadline the tick is a no-op.
        assert!(channel.tick(&mut queue, 7).is_empty());
        assert_eq!(queue.flush_calls, 0);

        // Empty queue: flush called, nothing sent.
        assert!(channel.tick(&mut queue, 8).is_empty());
        assert_eq!(queue.flush_calls, 1);
        assert_eq!(channel.next_tick_due_ms(), Some(16));

        queue.arm();
        let actions = channel.tick(&mut queue, 16);
        let packets = sent(&actions);
        assert_eq!(packets.len(), 1);
        // Sequence 0 was the metadata, 1 the empty flush, 2 this one.
        assert_eq!(packet::sequence_of(packets[0]), 2);
        assert_eq!(channel.packets_sent(), 2);
    }

    #[test]
    fn send_in_flight_parks_newest_packet_until_result() {
        let mut queue = FakeQueue::default();
        let mut channel = InputChannel::new();
        channel.on_open(&mut queue, 0); // metadata in flight

        queue.arm();
        let actions = channel.tick(&mut queue, 8);
        assert!(
            sent(&actions).is_empty(),
            "parked while metadata is in flight"
        );
        assert_eq!(channel.packets_sent(), 1);

        queue.arm();
        let actions = channel.tick(&mut queue, 16);
        assert!(
            sent(&actions).is_empty(),
            "newer packet replaces the parked one"
        );

        // Completion releases only the newest parked packet (sequence 3: 0 meta,
        // 1 and 2 flushed, 2 replaced 1... sequences are 0, 1, 2 -> parked = 2).
        let actions = channel.on_send_result(true);
        let packets = sent(&actions);
        assert_eq!(packets.len(), 1);
        assert_eq!(packet::sequence_of(packets[0]), 2);
        assert!(channel.is_send_in_flight());
        assert_eq!(channel.packets_sent(), 2);
        assert_eq!(channel.send_successes(), 1);

        // Nothing parked: completion sends nothing and clears the flag.
        assert!(channel.on_send_result(false).is_empty());
        assert!(!channel.is_send_in_flight());
        assert_eq!(channel.send_failures(), 1);

        // A result without a send in flight is ignored.
        assert!(channel.on_send_result(true).is_empty());
        assert_eq!(channel.send_successes(), 1);

        // With nothing in flight the next flush goes straight out.
        queue.arm();
        let actions = channel.tick(&mut queue, 24);
        assert_eq!(sent(&actions).len(), 1);
    }

    #[test]
    fn failed_sends_release_pending_too() {
        let mut queue = FakeQueue::default();
        let mut channel = InputChannel::new();
        channel.on_open(&mut queue, 0);
        queue.arm();
        channel.tick(&mut queue, 8);
        let actions = channel.on_send_result(false);
        assert_eq!(sent(&actions).len(), 1);
        assert_eq!(channel.send_failures(), 1);
        assert_eq!(channel.send_successes(), 0);
    }

    #[test]
    fn server_metadata_vector_from_streamcontroller_test() {
        // tst_streamcontroller decodedResolutionOverridesServerMetadata:
        // ServerMetadata: type=16, height=1080, width=1920, little endian.
        let mut channel = InputChannel::new();
        assert_eq!(channel.server_video_width(), 1920);
        assert_eq!(channel.server_video_height(), 1080);
        let actions = channel.on_binary(&hex("10003804000080070000"));
        assert_eq!(
            actions,
            vec![ChannelAction::ServerMetadata {
                width: 1920,
                height: 1080
            }]
        );
        // A different size updates the cached values (works before open too).
        let actions = channel.on_binary(&hex("1000a0050000000a0000"));
        assert_eq!(
            actions,
            vec![ChannelAction::ServerMetadata {
                width: 2560,
                height: 1440
            }]
        );
        assert_eq!(channel.server_video_width(), 2560);
        assert_eq!(channel.server_video_height(), 1440);
    }

    #[test]
    fn vibration_vector_from_inputpacket_test() {
        // tst_inputpacket.cpp vibrationParsing, byte for byte.
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

        let mut channel = InputChannel::new();
        let actions = channel.on_binary(&data);
        assert_eq!(
            actions,
            vec![ChannelAction::Vibration(VibrationReport {
                gamepad_index: 2,
                left_motor_percent: 0.5,
                right_motor_percent: 1.0,
                left_trigger_motor_percent: 0.0,
                right_trigger_motor_percent: 0.25,
                duration_ms: 300,
                delay_ms: 10,
                repeat_count: 3,
            })]
        );
    }

    #[test]
    fn short_or_unknown_inbound_reports_are_ignored() {
        let mut channel = InputChannel::new();
        assert!(channel.on_binary(&[]).is_empty());
        // tst_inputpacket rejectsShortBuffers: 5 zero bytes with the vibration
        // type, 3 bytes with the metadata type.
        let mut short_vibration = vec![0u8; 5];
        short_vibration[0] = 128;
        assert!(channel.on_binary(&short_vibration).is_empty());
        let mut short_metadata = vec![0u8; 3];
        short_metadata[0] = 16;
        assert!(channel.on_binary(&short_metadata).is_empty());
        assert_eq!(channel.server_video_width(), 1920);
        // Other report types (e.g. Gamepad echo, UnreliableAck low byte) are dropped.
        assert!(
            channel
                .on_binary(&[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
                .is_empty()
        );
        assert!(
            channel
                .on_binary(&[0x00, 0x04, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
                .is_empty()
        );
    }

    #[test]
    fn telemetry_reports_cadence_once_a_second() {
        let mut queue = FakeQueue::default();
        let mut channel = InputChannel::new();
        channel.on_open(&mut queue, 0);
        channel.on_send_result(true);

        let mut telemetry = Vec::new();
        for i in 1..=125u64 {
            for action in channel.tick(&mut queue, i * 8) {
                if let ChannelAction::FlushTelemetry { hz, jitter_ms } = action {
                    telemetry.push((hz, jitter_ms));
                }
            }
        }
        // First tick seeds last_send_ms; the 1000 ms mark (tick 125) emits once.
        assert_eq!(telemetry.len(), 1);
        let (hz, jitter) = telemetry[0];
        assert!((hz - 125.0).abs() < 1e-9, "{hz}");
        assert!(jitter.abs() < 1e-9, "{jitter}");
        assert_eq!(channel.flush_hz(), hz);
        assert_eq!(channel.flush_jitter_ms(), jitter);

        // Irregular cadence shows up as jitter at the next second mark.
        let mut now = 1000;
        let mut emitted = None;
        for step in [8u64, 12, 8, 12, 8, 12, 8, 12].iter().cycle().take(100) {
            now += step;
            for action in channel.tick(&mut queue, now) {
                if let ChannelAction::FlushTelemetry { hz, jitter_ms } = action {
                    emitted = Some((hz, jitter_ms));
                }
            }
        }
        let (hz, jitter) = emitted.expect("telemetry after another second");
        assert!(hz < 125.0 && hz > 80.0, "{hz}");
        assert!(jitter > 1.0, "{jitter}");
    }

    #[test]
    fn long_gap_resets_cadence_window() {
        let mut queue = FakeQueue::default();
        let mut channel = InputChannel::new();
        channel.on_open(&mut queue, 0);
        for i in 1..=10u64 {
            channel.tick(&mut queue, i * 8);
        }
        assert_eq!(channel.intervals.len(), 9);
        // > 750 ms since the last tick clears the window.
        channel.tick(&mut queue, 80 + 751);
        assert!(channel.intervals.is_empty());
        // Exactly 750 ms is still inside the window.
        channel.tick(&mut queue, 80 + 751 + 750);
        assert_eq!(channel.intervals.len(), 1);
    }

    #[test]
    fn interval_window_is_capped_at_256() {
        let mut queue = FakeQueue::default();
        let mut channel = InputChannel::new();
        channel.on_open(&mut queue, 0);
        for i in 1..=400u64 {
            channel.tick(&mut queue, i * 8);
        }
        assert_eq!(channel.intervals.len(), 256);
    }

    #[test]
    fn destroy_stops_loop_and_allows_reopen() {
        let mut queue = FakeQueue::default();
        let mut channel = InputChannel::new();
        channel.on_open(&mut queue, 0);
        queue.arm();
        channel.tick(&mut queue, 8); // parked behind the metadata send
        channel.on_binary(&hex("1000a0050000000a0000"));

        channel.destroy();
        assert!(!channel.is_running());
        // C++ destroy() leaves m_sendInFlight alone: the metadata send is still out.
        assert!(channel.is_send_in_flight());
        assert_eq!(channel.next_tick_due_ms(), None);
        assert!(channel.tick(&mut queue, 16).is_empty());
        // The parked packet is dropped: the late completion of the metadata send
        // sends nothing, but (as the QFutureWatcher lambda does) it still counts
        // and clears the in-flight flag.
        assert!(channel.on_send_result(true).is_empty());
        assert!(!channel.is_send_in_flight());
        assert_eq!(channel.send_successes(), 1);
        // Server metadata and counters survive, as in the C++.
        assert_eq!(channel.server_video_width(), 2560);
        assert_eq!(channel.packets_sent(), 1);

        // Re-open sends metadata again (the orchestrator does this per session).
        let actions = channel.on_open(&mut queue, 1000);
        assert_eq!(sent(&actions).len(), 1);
        assert_eq!(queue.metadata_calls, 2);
        assert_eq!(channel.packets_sent(), 2);
    }

    #[test]
    fn destroy_then_open_parks_metadata_behind_outstanding_send() {
        // InputChannel.cpp:128-138 (destroy) and :54-71 (watcher lambda): a send
        // started before destroy() is still in flight after it, so the next
        // onOpen() parks its ClientMetadata in m_pending and the old send's
        // completion releases it. Never two sends in flight.
        let mut queue = FakeQueue::default();
        let mut channel = InputChannel::new();
        channel.on_open(&mut queue, 0); // metadata #1 in flight
        queue.arm();
        channel.tick(&mut queue, 8); // sequence 1 parked behind it
        channel.destroy(); // drops the parked sequence 1
        assert!(channel.is_send_in_flight());

        let actions = channel.on_open(&mut queue, 1000);
        assert!(
            sent(&actions).is_empty(),
            "metadata #2 is parked while metadata #1 is still in flight"
        );
        assert!(channel.is_running());
        assert_eq!(queue.metadata_calls, 2);
        assert_eq!(channel.packets_sent(), 1);
        assert_eq!(channel.next_tick_due_ms(), Some(1008));

        // The old send's result belongs to the old send: it is counted there and
        // releases the parked metadata (sequence 2), which is now the one in flight.
        let actions = channel.on_send_result(false);
        let packets = sent(&actions);
        assert_eq!(packets.len(), 1);
        assert_eq!(packet::report_type_of(packets[0]), 8);
        assert_eq!(packet::sequence_of(packets[0]), 2);
        assert_eq!(channel.send_failures(), 1);
        assert_eq!(channel.send_successes(), 0);
        assert!(channel.is_send_in_flight());
        assert_eq!(channel.packets_sent(), 2);

        // A flush in the new session parks behind metadata #2 until its own result.
        queue.arm();
        assert!(sent(&channel.tick(&mut queue, 1008)).is_empty());
        let actions = channel.on_send_result(true);
        assert_eq!(sent(&actions).len(), 1);
        assert_eq!(channel.send_successes(), 1);
        assert_eq!(channel.packets_sent(), 3);
    }
}
