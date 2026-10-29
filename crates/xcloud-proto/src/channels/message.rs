//! Port of qt/src/streaming/channels/MessageChannel.{h,cpp}
//!
//! JSON protocol message channel ("messageV1"). Handles the handshake, the initial
//! client configuration, dimension updates and transaction envelopes. Mirrors
//! channel/message.ts.
//!
//! Sans-IO: `on_channel_open`/`on_text_message`/`send_dimensions_changed` return the
//! [`ChannelAction`]s (sends and events) that the C++ performed through the bridge
//! and emitted as signals. JSON key order follows Qt's sorted `QJsonObject`, see
//! `channels::control`.
//!
//! The C++ generated a random `QUuid::createUuid()` for every outbound `Message`
//! envelope. `xcloud-proto` has no `uuid`/`rand` dependency, so the channel carries a
//! small SplitMix64 generator that formats RFC 4122 v4-shaped ids. Every
//! [`MessageChannel::new`] seeds it from `std`'s OS-seeded `RandomState` and the
//! wall clock, so, as with `QUuid::createUuid()`, the id sequence differs between
//! channels, sessions and processes without any help from the caller.
//! [`MessageChannel::with_seed`] replaces that seed when reproducible ids are
//! wanted (tests, replay).

use std::hash::{BuildHasher, Hasher, RandomState};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::models::StreamDimensions;
use crate::streaming_types::DataChannelKind;

pub use super::control::ChannelAction;
use super::control::compact_json;

/// Handles the messageV1 handshake, initial client configuration, dimension
/// updates and transaction envelopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageChannel {
    dimensions: StreamDimensions,
    capability_width: u32,
    capability_height: u32,
    capability_bitrate_kbps: u32,
    capability_fps: u32,
    handshake_started: bool,
    handshake_completed: bool,
    has_sent_dimensions: bool,
    last_sent_dimensions: StreamDimensions,
    id_state: u64,
}

impl Default for MessageChannel {
    fn default() -> Self {
        Self::new(StreamDimensions::default())
    }
}

impl MessageChannel {
    pub const LABEL: &'static str = "message";
    pub const PROTOCOL_NAME: &'static str = "messageV1";
    pub const HANDSHAKE_VERSION: &'static str = "messageV1";
    pub const FIXED_HANDSHAKE_ID: &'static str = "be0bfc6d-1e83-4c8a-90ed-fa8601c5a179";
    pub const FIXED_CLIENT_APP_INSTALL_ID: &'static str = "c97d7ee0-73b2-4239-bf1d-9d805a338429";

    pub const TARGET_SYSTEM_UI_CONFIGURATION: &'static str = "/streaming/systemUi/configuration";
    pub const TARGET_CLIENT_APP_INSTALL_ID_CHANGED: &'static str =
        "/streaming/properties/clientappinstallidchanged";
    pub const TARGET_ORIENTATION_CHANGED: &'static str =
        "/streaming/characteristics/orientationchanged";
    pub const TARGET_TOUCH_INPUT_ENABLED_CHANGED: &'static str =
        "/streaming/characteristics/touchinputenabledchanged";
    pub const TARGET_CLIENT_DEVICE_CAPABILITIES: &'static str =
        "/streaming/characteristics/clientdevicecapabilities";
    pub const TARGET_DIMENSIONS_CHANGED: &'static str =
        "/streaming/characteristics/dimensionschanged";
    pub const TARGET_SERVER_INITIATED_DISCONNECT: &'static str =
        "/streaming/sessionLifetimeManagement/serverInitiatedDisconnect";
    pub const TARGET_SHOW_MESSAGE_DIALOG: &'static str =
        "/streaming/systemUi/messages/ShowMessageDialog";

    const KIND: DataChannelKind = DataChannelKind::Message;

    /// `MessageChannel(bridge, initialDimensions)`. The envelope id generator is
    /// seeded from process entropy (`RandomState` plus the wall clock).
    pub fn new(initial_dimensions: StreamDimensions) -> Self {
        Self {
            dimensions: initial_dimensions,
            capability_width: 1920,
            capability_height: 1080,
            capability_bitrate_kbps: 20000,
            capability_fps: 60,
            handshake_started: false,
            handshake_completed: false,
            has_sent_dimensions: false,
            last_sent_dimensions: StreamDimensions::default(),
            id_state: entropy_seed(),
        }
    }

    /// Replaces the entropy seed of the outbound `Message` envelope id generator
    /// (`QUuid::createUuid` in the C++) with a fixed one, for reproducible ids.
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.id_state = seed;
        self
    }

    /// Declares the client's decode capability on the message channel. This is the
    /// quality lever green-nx uses: xCloud reads maxWidth/maxHeight/maxBitrateKbps/
    /// supportsFps and sizes the encoder accordingly. Call before the HandshakeAck.
    pub fn configure_device_capabilities(
        &mut self,
        width: u32,
        height: u32,
        max_bitrate_kbps: u32,
        fps: u32,
    ) {
        self.capability_width = width;
        self.capability_height = height;
        self.capability_bitrate_kbps = max_bitrate_kbps;
        self.capability_fps = fps;
    }

    /// `onChannelOpen`: sends the Handshake once per lifecycle.
    pub fn on_channel_open(&mut self) -> Vec<ChannelAction> {
        if self.handshake_started {
            return Vec::new();
        }
        self.handshake_started = true;
        vec![ChannelAction::send_json(
            Self::KIND,
            &json!({
                "type": "Handshake",
                "version": Self::HANDSHAKE_VERSION,
                "id": Self::FIXED_HANDSHAKE_ID,
                "cv": "0",
            }),
        )]
    }

    /// `onTextMessage`: every inbound text yields `MessageReceived`; a
    /// `HandshakeAck` completes the handshake (once) and triggers the initial
    /// client configuration; `TransactionStart`/`Message` with a target yield
    /// `ProtocolMessage` and are answered with `TransactionComplete` where the
    /// C++ did so.
    pub fn on_text_message(&mut self, text: &str) -> Vec<ChannelAction> {
        let mut actions = vec![ChannelAction::MessageReceived {
            text: text.to_owned(),
        }];

        // `QJsonDocument::fromJson(...).isObject()`: anything else is ignored.
        let Some(object) = serde_json::from_str::<Value>(text)
            .ok()
            .filter(Value::is_object)
        else {
            return actions;
        };
        // `QJsonValue::toString()` reads a missing or non-string value as "".
        let kind = json_string(&object, "type");

        if kind == "HandshakeAck" {
            if self.handshake_completed {
                return actions;
            }
            self.handshake_completed = true;
            actions.push(ChannelAction::HandshakeCompleted);
            self.send_initial_client_config(&mut actions);
        } else if kind == "TransactionStart" || kind == "Message" {
            let target = json_string(&object, "target");
            if !target.is_empty() {
                actions.push(ChannelAction::ProtocolMessage {
                    target: target.to_owned(),
                    payload: text.to_owned(),
                });
                self.handle_transaction_if_needed(&object, target, &mut actions);
            }
        }
        actions
    }

    /// `sendDimensionsChanged`: remembers the dimensions and, once the handshake is
    /// complete, sends `dimensionschanged` when they differ from the last sent.
    pub fn send_dimensions_changed(&mut self, dimensions: StreamDimensions) -> Vec<ChannelAction> {
        let mut actions = Vec::new();
        self.push_dimensions_changed(dimensions, &mut actions);
        actions
    }

    /// `destroy`: resets the handshake so `on_channel_open` can run again.
    pub fn destroy(&mut self) {
        self.handshake_started = false;
        self.handshake_completed = false;
        self.has_sent_dimensions = false;
    }

    /// `isHandshakeComplete`.
    pub fn is_handshake_complete(&self) -> bool {
        self.handshake_completed
    }

    /// The dimensions last passed to `new`/`send_dimensions_changed`.
    pub fn dimensions(&self) -> StreamDimensions {
        self.dimensions
    }

    fn send_initial_client_config(&mut self, actions: &mut Vec<ChannelAction>) {
        tracing::info!(
            target: "Streaming",
            "Client capabilities: {}x{} {}fps maxBitrate={}kbps HEVC=false HDR=false",
            self.capability_width,
            self.capability_height,
            self.capability_fps,
            self.capability_bitrate_kbps
        );
        self.push_generated_message(
            Self::TARGET_SYSTEM_UI_CONFIGURATION,
            &json!({"version": [0, 2, 0], "systemUis": []}),
            actions,
        );
        self.push_generated_message(
            Self::TARGET_CLIENT_APP_INSTALL_ID_CHANGED,
            &json!({"clientAppInstallId": Self::FIXED_CLIENT_APP_INSTALL_ID}),
            actions,
        );
        self.push_generated_message(
            Self::TARGET_ORIENTATION_CHANGED,
            &json!({"orientation": 0}),
            actions,
        );
        self.push_generated_message(
            Self::TARGET_TOUCH_INPUT_ENABLED_CHANGED,
            &json!({"touchInputEnabled": false}),
            actions,
        );
        // Rich capability block: without it the service falls back to minimal
        // capabilities and pins the encoder to a starvation bitrate.
        self.push_generated_message(
            Self::TARGET_CLIENT_DEVICE_CAPABILITIES,
            &json!({
                "supportsCustomResolution": true,
                "supportsHevc": false,
                "supportsHdr": false,
                "supportsFps": self.capability_fps,
                "maxWidth": self.capability_width,
                "maxHeight": self.capability_height,
                "maxBitrateKbps": self.capability_bitrate_kbps,
                "video": {
                    "width": self.capability_width,
                    "height": self.capability_height,
                    "maxWidth": self.capability_width,
                    "maxHeight": self.capability_height,
                    "maxBitrateKbps": self.capability_bitrate_kbps,
                },
            }),
            actions,
        );
        let dimensions = self.dimensions;
        self.push_dimensions_changed(dimensions, actions);
    }

    fn push_dimensions_changed(
        &mut self,
        dimensions: StreamDimensions,
        actions: &mut Vec<ChannelAction>,
    ) {
        self.dimensions = dimensions;
        if !self.handshake_completed {
            return;
        }
        if self.has_sent_dimensions && self.last_sent_dimensions == dimensions {
            return;
        }
        self.push_generated_message(
            Self::TARGET_DIMENSIONS_CHANGED,
            &json!({
                "horizontal": dimensions.width,
                "vertical": dimensions.height,
                "preferredWidth": dimensions.width,
                "preferredHeight": dimensions.height,
                "safeAreaLeft": 0,
                "safeAreaTop": 0,
                "safeAreaRight": dimensions.width,
                "safeAreaBottom": dimensions.height,
                "supportsCustomResolution": true,
            }),
            actions,
        );
        self.last_sent_dimensions = dimensions;
        self.has_sent_dimensions = true;
    }

    /// `sendGeneratedMessage(target, content)`: a `Message` envelope whose
    /// `content` is the compact JSON of `content` as a string.
    fn push_generated_message(
        &mut self,
        target: &str,
        content: &Value,
        actions: &mut Vec<ChannelAction>,
    ) {
        let id = self.next_message_id();
        actions.push(ChannelAction::send_json(
            Self::KIND,
            &json!({
                "type": "Message",
                "content": compact_json(content),
                "id": id,
                "target": target,
                "cv": "",
            }),
        ));
    }

    /// `sendTransactionComplete(id, content)`: strings go through verbatim,
    /// objects/arrays as compact JSON, anything else as "".
    fn push_transaction_complete(id: &str, content: &Value, actions: &mut Vec<ChannelAction>) {
        let content_text = match content {
            Value::String(text) => text.clone(),
            Value::Object(_) | Value::Array(_) => compact_json(content),
            _ => String::new(),
        };
        actions.push(ChannelAction::send_json(
            Self::KIND,
            &json!({
                "type": "TransactionComplete",
                "content": content_text,
                "id": id,
                "cv": "",
            }),
        ));
    }

    fn handle_transaction_if_needed(
        &self,
        object: &Value,
        target: &str,
        actions: &mut Vec<ChannelAction>,
    ) {
        let id = json_string(object, "id");
        if id.is_empty() {
            return;
        }
        if target == Self::TARGET_SERVER_INITIATED_DISCONNECT {
            Self::push_transaction_complete(id, &Value::String(String::new()), actions);
            actions.push(ChannelAction::ServerInitiatedDisconnect);
        } else if target == Self::TARGET_SHOW_MESSAGE_DIALOG {
            Self::push_transaction_complete(id, &json!({"Result": 0}), actions);
        }
    }

    /// `QUuid::createUuid().toString(QUuid::WithoutBraces).toLower()`: a v4-shaped
    /// lowercase UUID from the channel's SplitMix64 state.
    fn next_message_id(&mut self) -> String {
        let hi = self.next_u64();
        let lo = self.next_u64();
        format_uuid_v4(hi, lo)
    }

    fn next_u64(&mut self) -> u64 {
        // SplitMix64 (Steele, Lea, Flood 2014).
        self.id_state = self.id_state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.id_state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// A fresh seed for the envelope id generator, standing in for the OS entropy
/// behind `QUuid::createUuid()` without a `rand`/`uuid` dependency: `RandomState`
/// is seeded by the OS once per thread and yields a different key on every
/// `new()`, and the wall clock separates processes further.
fn entropy_seed() -> u64 {
    let mut hasher = RandomState::new().build_hasher();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64);
    hasher.write_u64(nanos);
    hasher.finish()
}

/// `object.value(key).toString()`: "" for a missing or non-string value.
fn json_string<'a>(object: &'a Value, key: &str) -> &'a str {
    object.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Formats 128 random bits as an RFC 4122 version 4 UUID (`8-4-4-4-12`, lowercase).
fn format_uuid_v4(hi: u64, lo: u64) -> String {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&hi.to_be_bytes());
    bytes[8..].copy_from_slice(&lo.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0F) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3F) | 0x80; // RFC 4122 variant
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    //! tst_streamcontroller.cpp (inputWaitsForHandshake) only checks the message
    //! channel through the orchestrator; its assertions on the
    //! clientdevicecapabilities and dimensionschanged envelopes are ported here, and
    //! every outbound/inbound shape is pinned against MessageChannel.cpp.
    use super::*;

    const UUID_V4_LEN: usize = 36;

    fn texts(actions: &[ChannelAction]) -> Vec<&str> {
        actions
            .iter()
            .filter_map(|action| match action {
                ChannelAction::SendText { kind, text } => {
                    assert_eq!(*kind, DataChannelKind::Message);
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect()
    }

    fn envelope(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    fn content_of(text: &str) -> Value {
        serde_json::from_str(envelope(text)["content"].as_str().unwrap()).unwrap()
    }

    fn assert_uuid_v4(id: &str) {
        assert_eq!(id.len(), UUID_V4_LEN, "{id}");
        assert_eq!(id, id.to_lowercase());
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'), "{id}");
        assert!(
            matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'),
            "{id}"
        );
        assert!(id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
    }

    /// Replaces the random envelope id with `<id>` so the rest can be compared exactly.
    fn with_placeholder_id(text: &str) -> String {
        let object = envelope(text);
        let id = object["id"].as_str().unwrap();
        assert_uuid_v4(id);
        text.replacen(id, "<id>", 1)
    }

    fn handshaken(dims: StreamDimensions) -> (MessageChannel, Vec<ChannelAction>) {
        let mut channel = MessageChannel::new(dims);
        channel.on_channel_open();
        let actions = channel.on_text_message(r#"{"type":"HandshakeAck"}"#);
        (channel, actions)
    }

    #[test]
    fn constants_match_cpp() {
        assert_eq!(MessageChannel::LABEL, "message");
        assert_eq!(MessageChannel::PROTOCOL_NAME, "messageV1");
        assert_eq!(MessageChannel::HANDSHAKE_VERSION, "messageV1");
        assert_eq!(
            MessageChannel::FIXED_HANDSHAKE_ID,
            "be0bfc6d-1e83-4c8a-90ed-fa8601c5a179"
        );
        assert_eq!(
            MessageChannel::FIXED_CLIENT_APP_INSTALL_ID,
            "c97d7ee0-73b2-4239-bf1d-9d805a338429"
        );
    }

    #[test]
    fn channel_open_sends_handshake_once() {
        let mut channel = MessageChannel::default();
        let actions = channel.on_channel_open();
        assert_eq!(
            texts(&actions),
            vec![
                r#"{"cv":"0","id":"be0bfc6d-1e83-4c8a-90ed-fa8601c5a179","type":"Handshake","version":"messageV1"}"#
            ]
        );
        assert!(channel.on_channel_open().is_empty());
        assert!(!channel.is_handshake_complete());
        channel.destroy();
        assert_eq!(channel.on_channel_open().len(), 1);
    }

    #[test]
    fn handshake_ack_emits_event_then_initial_client_config() {
        let (channel, actions) = handshaken(StreamDimensions::default());
        assert!(channel.is_handshake_complete());

        assert_eq!(
            actions[0],
            ChannelAction::MessageReceived {
                text: r#"{"type":"HandshakeAck"}"#.to_owned()
            }
        );
        assert_eq!(actions[1], ChannelAction::HandshakeCompleted);
        let sent = texts(&actions);
        assert_eq!(sent.len(), 6);

        assert_eq!(
            with_placeholder_id(sent[0]),
            r#"{"content":"{\"systemUis\":[],\"version\":[0,2,0]}","cv":"","id":"<id>","target":"/streaming/systemUi/configuration","type":"Message"}"#
        );
        assert_eq!(
            with_placeholder_id(sent[1]),
            r#"{"content":"{\"clientAppInstallId\":\"c97d7ee0-73b2-4239-bf1d-9d805a338429\"}","cv":"","id":"<id>","target":"/streaming/properties/clientappinstallidchanged","type":"Message"}"#
        );
        assert_eq!(
            with_placeholder_id(sent[2]),
            r#"{"content":"{\"orientation\":0}","cv":"","id":"<id>","target":"/streaming/characteristics/orientationchanged","type":"Message"}"#
        );
        assert_eq!(
            with_placeholder_id(sent[3]),
            r#"{"content":"{\"touchInputEnabled\":false}","cv":"","id":"<id>","target":"/streaming/characteristics/touchinputenabledchanged","type":"Message"}"#
        );
        assert_eq!(
            with_placeholder_id(sent[4]),
            r#"{"content":"{\"maxBitrateKbps\":20000,\"maxHeight\":1080,\"maxWidth\":1920,\"supportsCustomResolution\":true,\"supportsFps\":60,\"supportsHdr\":false,\"supportsHevc\":false,\"video\":{\"height\":1080,\"maxBitrateKbps\":20000,\"maxHeight\":1080,\"maxWidth\":1920,\"width\":1920}}","cv":"","id":"<id>","target":"/streaming/characteristics/clientdevicecapabilities","type":"Message"}"#
        );
        assert_eq!(
            with_placeholder_id(sent[5]),
            r#"{"content":"{\"horizontal\":1920,\"preferredHeight\":1080,\"preferredWidth\":1920,\"safeAreaBottom\":1080,\"safeAreaLeft\":0,\"safeAreaRight\":1920,\"safeAreaTop\":0,\"supportsCustomResolution\":true,\"vertical\":1080}","cv":"","id":"<id>","target":"/streaming/characteristics/dimensionschanged","type":"Message"}"#
        );

        // Every envelope gets a distinct id.
        let ids: Vec<String> = sent
            .iter()
            .map(|t| envelope(t)["id"].as_str().unwrap().to_owned())
            .collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len());
    }

    #[test]
    fn second_handshake_ack_is_ignored() {
        let (mut channel, _) = handshaken(StreamDimensions::default());
        let actions = channel.on_text_message(r#"{"type":"HandshakeAck"}"#);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], ChannelAction::MessageReceived { .. }));
    }

    #[test]
    fn streamcontroller_capabilities_and_dimensions_for_1080_and_1440() {
        // tst_streamcontroller.cpp inputWaitsForHandshake: message-channel part.
        for use_1440 in [false, true] {
            let (width, height) = if use_1440 { (2560, 1440) } else { (1920, 1080) };
            let bitrate = if use_1440 { 30000 } else { 20000 };
            let mut channel = MessageChannel::new(StreamDimensions { width, height });
            channel.configure_device_capabilities(width, height, bitrate, 60);
            channel.on_channel_open();
            let actions = channel.on_text_message(r#"{"type":"HandshakeAck"}"#);

            let mut capabilities = 0;
            let mut dimensions = 0;
            for text in texts(&actions) {
                let env = envelope(text);
                let content = content_of(text);
                if env["target"] == MessageChannel::TARGET_CLIENT_DEVICE_CAPABILITIES {
                    capabilities += 1;
                    assert_eq!(content["maxWidth"], width);
                    assert_eq!(content["maxHeight"], height);
                    assert_eq!(content["maxBitrateKbps"], bitrate);
                    assert_eq!(content["supportsFps"], 60);
                    assert_eq!(content["supportsHevc"], false);
                    assert_eq!(content["supportsHdr"], false);
                    assert_eq!(content["video"]["width"], width);
                    assert_eq!(content["video"]["maxHeight"], height);
                }
                if env["target"] == MessageChannel::TARGET_DIMENSIONS_CHANGED {
                    dimensions += 1;
                    assert_eq!(content["horizontal"], width);
                    assert_eq!(content["vertical"], height);
                }
                assert!(!text.contains("userRequestedResolutionUpdate"));
            }
            assert_eq!(capabilities, 1);
            assert_eq!(dimensions, 1);

            // A repeated ack sends nothing more.
            let again = channel.on_text_message(r#"{"type":"HandshakeAck"}"#);
            assert!(texts(&again).is_empty());
        }
    }

    #[test]
    fn dimensions_changed_waits_for_handshake_and_dedupes() {
        let mut channel = MessageChannel::new(StreamDimensions {
            width: 1920,
            height: 1080,
        });
        // Before the handshake: remembered, not sent.
        let early = channel.send_dimensions_changed(StreamDimensions {
            width: 1280,
            height: 720,
        });
        assert!(early.is_empty());
        assert_eq!(channel.dimensions().width, 1280);

        channel.on_channel_open();
        let actions = channel.on_text_message(r#"{"type":"HandshakeAck"}"#);
        let sent = texts(&actions);
        // The initial config carries the latest remembered dimensions.
        assert_eq!(
            with_placeholder_id(sent[5]),
            r#"{"content":"{\"horizontal\":1280,\"preferredHeight\":720,\"preferredWidth\":1280,\"safeAreaBottom\":720,\"safeAreaLeft\":0,\"safeAreaRight\":1280,\"safeAreaTop\":0,\"supportsCustomResolution\":true,\"vertical\":720}","cv":"","id":"<id>","target":"/streaming/characteristics/dimensionschanged","type":"Message"}"#
        );

        // Same dimensions again: deduplicated.
        assert!(
            channel
                .send_dimensions_changed(StreamDimensions {
                    width: 1280,
                    height: 720,
                })
                .is_empty()
        );
        // New dimensions: sent.
        let changed = channel.send_dimensions_changed(StreamDimensions {
            width: 2560,
            height: 1440,
        });
        assert_eq!(
            with_placeholder_id(texts(&changed)[0]),
            r#"{"content":"{\"horizontal\":2560,\"preferredHeight\":1440,\"preferredWidth\":2560,\"safeAreaBottom\":1440,\"safeAreaLeft\":0,\"safeAreaRight\":2560,\"safeAreaTop\":0,\"supportsCustomResolution\":true,\"vertical\":1440}","cv":"","id":"<id>","target":"/streaming/characteristics/dimensionschanged","type":"Message"}"#
        );
        // destroy() forgets what was sent, so the same dimensions go out again
        // after the next handshake.
        channel.destroy();
        assert!(!channel.is_handshake_complete());
        channel.on_channel_open();
        let actions = channel.on_text_message(r#"{"type":"HandshakeAck"}"#);
        assert!(texts(&actions)[5].contains(r#"\"horizontal\":2560"#));
    }

    #[test]
    fn server_initiated_disconnect_transaction() {
        let (mut channel, _) = handshaken(StreamDimensions::default());
        let inbound = r#"{"type":"TransactionStart","id":"tx-1","target":"/streaming/sessionLifetimeManagement/serverInitiatedDisconnect","content":"{}","cv":""}"#;
        let actions = channel.on_text_message(inbound);
        assert_eq!(
            actions,
            vec![
                ChannelAction::MessageReceived {
                    text: inbound.to_owned()
                },
                ChannelAction::ProtocolMessage {
                    target: MessageChannel::TARGET_SERVER_INITIATED_DISCONNECT.to_owned(),
                    payload: inbound.to_owned(),
                },
                ChannelAction::SendText {
                    kind: DataChannelKind::Message,
                    text: r#"{"content":"","cv":"","id":"tx-1","type":"TransactionComplete"}"#
                        .to_owned(),
                },
                ChannelAction::ServerInitiatedDisconnect,
            ]
        );
    }

    #[test]
    fn show_message_dialog_transaction_answers_result_zero() {
        let (mut channel, _) = handshaken(StreamDimensions::default());
        let inbound = r#"{"type":"Message","id":"dialog-7","target":"/streaming/systemUi/messages/ShowMessageDialog","content":"{\"Title\":\"x\"}"}"#;
        let actions = channel.on_text_message(inbound);
        assert_eq!(actions.len(), 3);
        assert_eq!(
            actions[1],
            ChannelAction::ProtocolMessage {
                target: MessageChannel::TARGET_SHOW_MESSAGE_DIALOG.to_owned(),
                payload: inbound.to_owned(),
            }
        );
        assert_eq!(
            texts(&actions),
            vec![
                r#"{"content":"{\"Result\":0}","cv":"","id":"dialog-7","type":"TransactionComplete"}"#
            ]
        );
    }

    #[test]
    fn protocol_messages_without_known_target_are_only_forwarded() {
        let (mut channel, _) = handshaken(StreamDimensions::default());
        let inbound =
            r#"{"type":"Message","id":"m1","target":"/streaming/title/something","content":"{}"}"#;
        let actions = channel.on_text_message(inbound);
        assert_eq!(actions.len(), 2);
        assert_eq!(
            actions[1],
            ChannelAction::ProtocolMessage {
                target: "/streaming/title/something".to_owned(),
                payload: inbound.to_owned(),
            }
        );
        // TransactionStart with a known target but no id: forwarded, not answered.
        let no_id = r#"{"type":"TransactionStart","target":"/streaming/systemUi/messages/ShowMessageDialog"}"#;
        let actions = channel.on_text_message(no_id);
        assert_eq!(actions.len(), 2);
        assert!(texts(&actions).is_empty());
        // Empty id is treated like a missing one.
        let empty_id = r#"{"type":"TransactionStart","id":"","target":"/streaming/systemUi/messages/ShowMessageDialog"}"#;
        assert_eq!(channel.on_text_message(empty_id).len(), 2);
    }

    #[test]
    fn transactions_are_handled_even_before_handshake() {
        // The C++ does not gate TransactionStart on m_handshakeCompleted.
        let mut channel = MessageChannel::default();
        let inbound = r#"{"type":"TransactionStart","id":"early","target":"/streaming/sessionLifetimeManagement/serverInitiatedDisconnect"}"#;
        let actions = channel.on_text_message(inbound);
        assert!(actions.contains(&ChannelAction::ServerInitiatedDisconnect));
    }

    #[test]
    fn messages_without_target_or_unknown_type_only_emit_message_received() {
        let mut channel = MessageChannel::default();
        for inbound in [
            r#"{"type":"Message","id":"x"}"#,
            r#"{"type":"Message","target":""}"#,
            r#"{"type":"Heartbeat"}"#,
            r#"{"type":42}"#,
            r#"{"target":"/streaming/x"}"#,
            r#"{}"#,
        ] {
            let actions = channel.on_text_message(inbound);
            assert_eq!(
                actions,
                vec![ChannelAction::MessageReceived {
                    text: inbound.to_owned()
                }],
                "{inbound}"
            );
        }
    }

    #[test]
    fn non_object_or_invalid_json_only_emits_message_received() {
        let mut channel = MessageChannel::default();
        for inbound in ["", "not json", "[1,2]", "\"HandshakeAck\"", "42", "null"] {
            let actions = channel.on_text_message(inbound);
            assert_eq!(
                actions,
                vec![ChannelAction::MessageReceived {
                    text: inbound.to_owned()
                }],
                "{inbound:?}"
            );
            assert!(!channel.is_handshake_complete());
        }
    }

    #[test]
    fn message_ids_are_v4_uuids_and_seed_dependent() {
        let mut a = MessageChannel::default().with_seed(1);
        let mut b = MessageChannel::default().with_seed(1);
        let mut c = MessageChannel::default().with_seed(2);
        let (ia, ib, ic) = (
            a.next_message_id(),
            b.next_message_id(),
            c.next_message_id(),
        );
        assert_uuid_v4(&ia);
        assert_uuid_v4(&ic);
        assert_eq!(ia, ib);
        assert_ne!(ia, ic);
        assert_ne!(a.next_message_id(), ia);
        // Version/variant nibbles are forced regardless of input bits.
        assert_eq!(format_uuid_v4(0, 0), "00000000-0000-4000-8000-000000000000");
        assert_eq!(
            format_uuid_v4(u64::MAX, u64::MAX),
            "ffffffff-ffff-4fff-bfff-ffffffffffff"
        );
    }

    #[test]
    fn default_channels_draw_distinct_id_sequences() {
        // MessageChannel.cpp:48 draws a fresh QUuid::createUuid() per envelope, so
        // two channels (two sessions, two processes) never share an id sequence.
        // Without with_seed() every channel must get its own seed.
        let mut a = MessageChannel::default();
        let mut b = MessageChannel::new(StreamDimensions::default());
        let first_a = a.next_message_id();
        let first_b = b.next_message_id();
        assert_uuid_v4(&first_a);
        assert_uuid_v4(&first_b);
        assert_ne!(first_a, first_b);
        // The previous fixed seed would have made every channel start
        // with this id.
        let mut fixed = MessageChannel::default().with_seed(0x5265_6179_7843_6c64);
        let fixed_first = fixed.next_message_id();
        assert_ne!(first_a, fixed_first);
        assert_ne!(first_b, fixed_first);

        // The handshake path uses the same generator: two sessions send the same
        // envelopes with different ids.
        let (_, actions_a) = handshaken(StreamDimensions::default());
        let (_, actions_b) = handshaken(StreamDimensions::default());
        let ids = |actions: &[ChannelAction]| -> Vec<String> {
            texts(actions)
                .iter()
                .map(|t| envelope(t)["id"].as_str().unwrap().to_owned())
                .collect()
        };
        let (ids_a, ids_b) = (ids(&actions_a), ids(&actions_b));
        assert_eq!(ids_a.len(), 6);
        assert!(ids_a.iter().zip(&ids_b).all(|(x, y)| x != y), "{ids_a:?}");
        // ... and the rest of each envelope is identical.
        let strip = |actions: &[ChannelAction]| -> Vec<String> {
            texts(actions)
                .iter()
                .map(|t| with_placeholder_id(t))
                .collect()
        };
        assert_eq!(strip(&actions_a), strip(&actions_b));
    }

    #[test]
    fn transaction_complete_content_formatting() {
        // Strings verbatim, objects/arrays compact, other values as "".
        let mut actions = Vec::new();
        MessageChannel::push_transaction_complete("1", &json!("raw"), &mut actions);
        MessageChannel::push_transaction_complete("2", &json!({"b": 1, "a": [true]}), &mut actions);
        MessageChannel::push_transaction_complete("3", &json!([1, "x"]), &mut actions);
        MessageChannel::push_transaction_complete("4", &json!(5), &mut actions);
        MessageChannel::push_transaction_complete("5", &Value::Null, &mut actions);
        assert_eq!(
            texts(&actions),
            vec![
                r#"{"content":"raw","cv":"","id":"1","type":"TransactionComplete"}"#,
                r#"{"content":"{\"a\":[true],\"b\":1}","cv":"","id":"2","type":"TransactionComplete"}"#,
                r#"{"content":"[1,\"x\"]","cv":"","id":"3","type":"TransactionComplete"}"#,
                r#"{"content":"","cv":"","id":"4","type":"TransactionComplete"}"#,
                r#"{"content":"","cv":"","id":"5","type":"TransactionComplete"}"#,
            ]
        );
    }
}
