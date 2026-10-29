//! Port of qt/src/streaming/channels/ControlChannel.{h,cpp}
//!
//! JSON control channel ("controlV1"). Sends `authorizationRequest`,
//! `gamepadChanged`, `VideoPreference`, `userRequestedResolutionUpdate` and the
//! periodic `videoKeyframeRequested` messages. Mirrors channel/control.ts.
//!
//! The C++ channel is a `QObject` holding a `WebRtcBridge*` and a `QTimer`. Here it
//! is a sans-IO state machine: every entry point returns the [`ChannelAction`]s the
//! driver has to perform (send on a data channel, or react to an event), and the
//! keyframe timer is the `next_keyframe_request_due_ms` deadline driven by
//! [`ControlChannel::tick`].
//!
//! JSON byte compatibility: Qt's `QJsonObject` stores keys sorted (UTF-16 code unit
//! order) and `QJsonDocument::toJson(Compact)` writes them in that order, so the C++
//! output has alphabetically sorted keys. `serde_json::Value` without the
//! `preserve_order` feature is a `BTreeMap` and serialises in byte order, which is
//! identical for the ASCII keys used here. The tests pin the exact strings.

use serde_json::{Value, json};

use crate::input::packet::VibrationReport;
use crate::streaming_types::DataChannelKind;

/// What a channel wants the driver to do. Shared by all four channels in
/// `channels::*`: outbound traffic (`SendText`/`SendBinary`) plus the events the
/// C++ channels emitted as Qt signals.
#[derive(Debug, Clone, PartialEq)]
pub enum ChannelAction {
    /// `WebRtcBridge::sendString(kind, text)`.
    SendText { kind: DataChannelKind, text: String },
    /// `WebRtcBridge::send(kind, bytes)`.
    SendBinary {
        kind: DataChannelKind,
        bytes: Vec<u8>,
    },
    /// `MessageChannel::handshakeCompleted`.
    HandshakeCompleted,
    /// `MessageChannel::messageReceived(text)`: every inbound text on the message channel.
    MessageReceived { text: String },
    /// `MessageChannel::protocolMessage(target, payload)`.
    ProtocolMessage { target: String, payload: String },
    /// `MessageChannel::serverInitiatedDisconnect`.
    ServerInitiatedDisconnect,
    /// `InputChannel::vibrationReceived(report)`.
    Vibration(VibrationReport),
    /// `InputChannel::serverMetadataReceived(width, height)`.
    ServerMetadata { width: u32, height: u32 },
    /// `InputChannel::flushTelemetry(hz, jitterMs)`.
    FlushTelemetry { hz: f64, jitter_ms: f64 },
}

impl ChannelAction {
    /// `sendJson`: compact JSON (Qt `QJsonDocument::Compact`) on `kind`.
    pub(crate) fn send_json(kind: DataChannelKind, object: &Value) -> Self {
        ChannelAction::SendText {
            kind,
            text: compact_json(object),
        }
    }
}

/// `QJsonDocument(object).toJson(QJsonDocument::Compact)`: no whitespace, sorted keys.
pub(crate) fn compact_json(value: &Value) -> String {
    // `Display` for `Value` is the compact form and cannot fail for a `Value`.
    value.to_string()
}

/// Sends authorizationRequest, gamepadChanged, VideoPreference and periodic
/// videoKeyframeRequested messages. Mirrors channel/control.ts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlChannel {
    keyframe_interval_seconds: i32,
    preferred_width: u32,
    preferred_height: u32,
    preferred_fps: u32,
    preferred_color_range: Option<String>,
    did_run_open_sequence: bool,
    /// Replaces `m_keyframeTimer`: `Some(deadline)` while the periodic request is armed.
    next_keyframe_request_due_ms: Option<u64>,
}

impl Default for ControlChannel {
    fn default() -> Self {
        Self::new(Self::DEFAULT_KEYFRAME_INTERVAL_SECONDS)
    }
}

impl ControlChannel {
    pub const LABEL: &'static str = "control";
    pub const PROTOCOL_NAME: &'static str = "controlV1";
    /// The fixed access key used by all xCloud clients.
    pub const ACCESS_KEY: &'static str = "4BDB3609-C1F1-4195-9B37-FEFF45DA8B8E";
    /// Default `keyframeIntervalSeconds` constructor argument.
    pub const DEFAULT_KEYFRAME_INTERVAL_SECONDS: i32 = 5;

    const KIND: DataChannelKind = DataChannelKind::Control;

    /// `ControlChannel(bridge, keyframeIntervalSeconds)`. A non-positive interval
    /// disables the periodic keyframe request.
    pub fn new(keyframe_interval_seconds: i32) -> Self {
        Self {
            keyframe_interval_seconds,
            preferred_width: 1920,
            preferred_height: 1080,
            preferred_fps: 60,
            preferred_color_range: None,
            did_run_open_sequence: false,
            next_keyframe_request_due_ms: None,
        }
    }

    /// Must be called before [`on_open`](Self::on_open) so the VideoPreference
    /// carries the right values. `color_range` is `None`/empty to omit the key.
    pub fn configure_video_preference(
        &mut self,
        width: u32,
        height: u32,
        frames_per_second: u32,
        color_range: Option<&str>,
    ) {
        self.preferred_width = width;
        self.preferred_height = height;
        self.preferred_fps = frames_per_second;
        self.preferred_color_range = color_range
            .filter(|range| !range.is_empty())
            .map(str::to_owned);
    }

    /// `onOpen`: the authorization/startup handshake. Runs once per lifecycle
    /// (until [`destroy`](Self::destroy)); repeated calls return nothing.
    pub fn on_open(&mut self, now_ms: u64) -> Vec<ChannelAction> {
        if self.did_run_open_sequence {
            return Vec::new();
        }
        self.did_run_open_sequence = true;
        tracing::info!(
            target: "StreamingControl",
            "Control channel onOpen: sending authorization/startup handshake"
        );

        let mut actions = vec![ChannelAction::send_json(
            Self::KIND,
            &json!({
                "message": "authorizationRequest",
                "accessKey": Self::ACCESS_KEY,
            }),
        )];
        // The orchestrator announces the desired controllers after authorization.

        let mut preference = json!({
            "message": "VideoPreference",
            "type": "VideoPreference",
            "resolution": {"width": self.preferred_width, "height": self.preferred_height},
            "dimensions": {"width": self.preferred_width, "height": self.preferred_height},
            "maxFPS": self.preferred_fps,
        });
        if let (Some(range), Some(object)) =
            (&self.preferred_color_range, preference.as_object_mut())
        {
            object.insert("colorRange".to_owned(), Value::String(range.to_lowercase()));
        }
        actions.push(ChannelAction::send_json(Self::KIND, &preference));

        if self.keyframe_interval_seconds > 0 {
            self.next_keyframe_request_due_ms = Some(now_ms + self.keyframe_interval_ms());
        }
        actions
    }

    /// `sendGamepadChanged(index, wasAdded)`.
    pub fn send_gamepad_changed(&self, index: i32, was_added: bool) -> Vec<ChannelAction> {
        tracing::info!(
            target: "StreamingControl",
            "Control channel send gamepadChanged index={index} wasAdded={was_added}"
        );
        vec![ChannelAction::send_json(
            Self::KIND,
            &json!({
                "message": "gamepadChanged",
                "gamepadIndex": index,
                "wasAdded": was_added,
            }),
        )]
    }

    /// `sendUserRequestedResolution(alias)`: asks the service for a specific
    /// resolution tier (official picker), e.g. `"1080"` or `"1440"`.
    pub fn send_user_requested_resolution(&self, alias: &str) -> Vec<ChannelAction> {
        tracing::info!(target: "StreamingControl", "Requesting resolution: {alias}");
        vec![ChannelAction::send_json(
            Self::KIND,
            &json!({
                "message": "userRequestedResolutionUpdate",
                "resolutionAlias": alias,
            }),
        )]
    }

    /// `requestKeyframe(ifrRequested = true)`.
    pub fn request_keyframe(&self, ifr_requested: bool) -> Vec<ChannelAction> {
        vec![ChannelAction::send_json(
            Self::KIND,
            &json!({
                "message": "videoKeyframeRequested",
                "ifrRequested": ifr_requested,
            }),
        )]
    }

    /// The `QTimer::timeout` replacement: when the keyframe deadline has passed,
    /// emits `videoKeyframeRequested` and re-arms the deadline. Returns nothing
    /// when the timer is not armed or not due yet.
    pub fn tick(&mut self, now_ms: u64) -> Vec<ChannelAction> {
        let Some(due) = self.next_keyframe_request_due_ms else {
            return Vec::new();
        };
        if now_ms < due {
            return Vec::new();
        }
        self.next_keyframe_request_due_ms = Some(now_ms + self.keyframe_interval_ms());
        self.request_keyframe(true)
    }

    /// When [`tick`](Self::tick) next has work, or `None` while the keyframe timer
    /// is stopped.
    pub fn next_keyframe_request_due_ms(&self) -> Option<u64> {
        self.next_keyframe_request_due_ms
    }

    /// `destroy`: stops the keyframe timer and allows `on_open` to run again.
    pub fn destroy(&mut self) {
        self.next_keyframe_request_due_ms = None;
        self.did_run_open_sequence = false;
    }

    /// Whether the open sequence has run since the last `destroy`.
    pub fn did_run_open_sequence(&self) -> bool {
        self.did_run_open_sequence
    }

    fn keyframe_interval_ms(&self) -> u64 {
        u64::try_from(self.keyframe_interval_seconds).unwrap_or(0) * 1000
    }
}

#[cfg(test)]
mod tests {
    //! The named Qt tests (tst_streamcontroller.cpp) only exercise the control
    //! channel through the orchestrator; these tests pin the exact bytes the C++
    //! `QJsonDocument::toJson(Compact)` produced (sorted keys), using
    //! ControlChannel.cpp as the oracle.
    use super::*;

    fn texts(actions: &[ChannelAction]) -> Vec<(DataChannelKind, &str)> {
        actions
            .iter()
            .map(|action| match action {
                ChannelAction::SendText { kind, text } => (*kind, text.as_str()),
                other => panic!("unexpected action {other:?}"),
            })
            .collect()
    }

    #[test]
    fn constants_match_cpp() {
        assert_eq!(ControlChannel::LABEL, "control");
        assert_eq!(ControlChannel::PROTOCOL_NAME, "controlV1");
        assert_eq!(
            ControlChannel::ACCESS_KEY,
            "4BDB3609-C1F1-4195-9B37-FEFF45DA8B8E"
        );
        assert_eq!(ControlChannel::DEFAULT_KEYFRAME_INTERVAL_SECONDS, 5);
    }

    #[test]
    fn on_open_sends_authorization_then_video_preference_defaults() {
        let mut channel = ControlChannel::default();
        let actions = channel.on_open(1_000);
        assert_eq!(
            texts(&actions),
            vec![
                (
                    DataChannelKind::Control,
                    r#"{"accessKey":"4BDB3609-C1F1-4195-9B37-FEFF45DA8B8E","message":"authorizationRequest"}"#
                ),
                (
                    DataChannelKind::Control,
                    r#"{"dimensions":{"height":1080,"width":1920},"maxFPS":60,"message":"VideoPreference","resolution":{"height":1080,"width":1920},"type":"VideoPreference"}"#
                ),
            ]
        );
        // Keyframe timer armed at 5 s (default interval).
        assert_eq!(channel.next_keyframe_request_due_ms(), Some(6_000));
        assert!(channel.did_run_open_sequence());
    }

    #[test]
    fn video_preference_carries_configured_values_and_lowercased_color_range() {
        let mut channel = ControlChannel::new(5);
        channel.configure_video_preference(2560, 1440, 120, Some("Limited"));
        let actions = channel.on_open(0);
        assert_eq!(
            texts(&actions)[1].1,
            r#"{"colorRange":"limited","dimensions":{"height":1440,"width":2560},"maxFPS":120,"message":"VideoPreference","resolution":{"height":1440,"width":2560},"type":"VideoPreference"}"#
        );
    }

    #[test]
    fn empty_color_range_is_omitted() {
        // `if (!m_preferredColorRange.isEmpty())` in the C++.
        let mut channel = ControlChannel::new(5);
        channel.configure_video_preference(1280, 720, 60, Some(""));
        let actions = channel.on_open(0);
        assert_eq!(
            texts(&actions)[1].1,
            r#"{"dimensions":{"height":720,"width":1280},"maxFPS":60,"message":"VideoPreference","resolution":{"height":720,"width":1280},"type":"VideoPreference"}"#
        );
        let mut channel = ControlChannel::new(5);
        channel.configure_video_preference(1280, 720, 60, Some("Full"));
        let actions = channel.on_open(0);
        assert!(texts(&actions)[1].1.contains(r#""colorRange":"full""#));
    }

    #[test]
    fn on_open_runs_once_until_destroy() {
        let mut channel = ControlChannel::new(5);
        assert_eq!(channel.on_open(0).len(), 2);
        assert!(channel.on_open(10).is_empty());
        assert!(channel.on_open(20).is_empty());
        channel.destroy();
        assert!(!channel.did_run_open_sequence());
        assert_eq!(channel.next_keyframe_request_due_ms(), None);
        assert_eq!(channel.on_open(30).len(), 2);
        assert_eq!(channel.next_keyframe_request_due_ms(), Some(5_030));
    }

    #[test]
    fn gamepad_changed_json() {
        let channel = ControlChannel::default();
        assert_eq!(
            texts(&channel.send_gamepad_changed(0, true)),
            vec![(
                DataChannelKind::Control,
                r#"{"gamepadIndex":0,"message":"gamepadChanged","wasAdded":true}"#
            )]
        );
        assert_eq!(
            texts(&channel.send_gamepad_changed(3, false))[0].1,
            r#"{"gamepadIndex":3,"message":"gamepadChanged","wasAdded":false}"#
        );
    }

    #[test]
    fn user_requested_resolution_json() {
        // tst_streamcontroller: alias "1440" for P1440, "1080" for P1080.
        let channel = ControlChannel::default();
        assert_eq!(
            texts(&channel.send_user_requested_resolution("1440"))[0].1,
            r#"{"message":"userRequestedResolutionUpdate","resolutionAlias":"1440"}"#
        );
        assert_eq!(
            texts(&channel.send_user_requested_resolution("1080"))[0].1,
            r#"{"message":"userRequestedResolutionUpdate","resolutionAlias":"1080"}"#
        );
    }

    #[test]
    fn request_keyframe_json() {
        let channel = ControlChannel::default();
        assert_eq!(
            texts(&channel.request_keyframe(true))[0].1,
            r#"{"ifrRequested":true,"message":"videoKeyframeRequested"}"#
        );
        assert_eq!(
            texts(&channel.request_keyframe(false))[0].1,
            r#"{"ifrRequested":false,"message":"videoKeyframeRequested"}"#
        );
    }

    #[test]
    fn keyframe_timer_fires_every_interval_after_open() {
        let mut channel = ControlChannel::new(2);
        // Not armed before open.
        assert!(channel.tick(10_000).is_empty());
        assert_eq!(channel.next_keyframe_request_due_ms(), None);

        channel.on_open(1_000);
        assert_eq!(channel.next_keyframe_request_due_ms(), Some(3_000));
        assert!(channel.tick(2_999).is_empty());
        let fired = channel.tick(3_000);
        assert_eq!(
            texts(&fired),
            vec![(
                DataChannelKind::Control,
                r#"{"ifrRequested":true,"message":"videoKeyframeRequested"}"#
            )]
        );
        assert_eq!(channel.next_keyframe_request_due_ms(), Some(5_000));
        assert!(channel.tick(4_000).is_empty());
        // A late tick fires once and re-arms relative to now.
        assert_eq!(channel.tick(5_700).len(), 1);
        assert_eq!(channel.next_keyframe_request_due_ms(), Some(7_700));

        channel.destroy();
        assert!(channel.tick(100_000).is_empty());
    }

    #[test]
    fn non_positive_interval_disables_keyframe_timer() {
        for interval in [0, -1] {
            let mut channel = ControlChannel::new(interval);
            assert_eq!(channel.on_open(0).len(), 2);
            assert_eq!(channel.next_keyframe_request_due_ms(), None);
            assert!(channel.tick(60_000).is_empty());
        }
    }

    #[test]
    fn streamcontroller_control_sequence_matches_orchestrator_expectations() {
        // tst_streamcontroller.cpp inputWaitsForHandshake, control-channel part:
        // first message is authorizationRequest, exactly one gamepadChanged with
        // wasAdded=true, exactly one userRequestedResolutionUpdate after it.
        for (use_1440, alias) in [(false, "1080"), (true, "1440")] {
            let mut channel = ControlChannel::new(5);
            let (width, height) = if use_1440 { (2560, 1440) } else { (1920, 1080) };
            channel.configure_video_preference(width, height, 60, None);
            let mut actions = channel.on_open(0);
            actions.extend(channel.send_gamepad_changed(0, true));
            actions.extend(channel.send_user_requested_resolution(alias));

            let messages: Vec<String> = texts(&actions)
                .iter()
                .map(|(kind, text)| {
                    assert_eq!(*kind, DataChannelKind::Control);
                    let object: Value = serde_json::from_str(text).unwrap();
                    if object["message"] == "userRequestedResolutionUpdate" {
                        assert_eq!(object["resolutionAlias"], alias);
                    }
                    if object["message"] == "gamepadChanged" {
                        assert_eq!(object["wasAdded"], true);
                    }
                    object["message"].as_str().unwrap().to_owned()
                })
                .collect();
            assert_eq!(messages[0], "authorizationRequest");
            assert_eq!(
                messages.iter().filter(|m| *m == "gamepadChanged").count(),
                1
            );
            assert_eq!(
                messages
                    .iter()
                    .filter(|m| *m == "userRequestedResolutionUpdate")
                    .count(),
                1
            );
            let gamepad = messages.iter().position(|m| m == "gamepadChanged").unwrap();
            let resolution = messages
                .iter()
                .position(|m| m == "userRequestedResolutionUpdate")
                .unwrap();
            assert!(resolution > gamepad);
        }
    }
}
