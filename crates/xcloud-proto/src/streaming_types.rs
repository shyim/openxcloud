//! Port of qt/src/streaming/StreamingTypes.h (+ the .cpp parts of StreamingConfig.cpp that implement it)
//!
//! Shared streaming protocol value types: data-channel specs, peer connection
//! and stream lifecycle states, typed stream errors and stats snapshots.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The four data channels negotiated with the xCloud service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DataChannelKind {
    #[default]
    Control,
    Input,
    Message,
    Chat,
}

/// `dataChannelKindName`: the channel label.
pub fn data_channel_kind_name(kind: DataChannelKind) -> &'static str {
    match kind {
        DataChannelKind::Control => "control",
        DataChannelKind::Input => "input",
        DataChannelKind::Message => "message",
        DataChannelKind::Chat => "chat",
    }
}

/// Data-channel reliability, matching the green-nx table. The input channel is
/// unordered and unreliable (max retransmits 0) so a lost input packet never
/// blocks the 125 Hz stream behind a retransmission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DataChannelSpec {
    pub kind: DataChannelKind,
    pub label: &'static str,
    pub protocol: &'static str,
    pub ordered: bool,
    /// -1 means reliable (same convention as the original C++ client).
    pub max_retransmits: i32,
}

impl DataChannelSpec {
    /// True when the channel has no retransmit cap.
    pub fn is_reliable(&self) -> bool {
        self.max_retransmits < 0
    }
}

const DATA_CHANNEL_SPECS: [DataChannelSpec; 4] = [
    DataChannelSpec {
        kind: DataChannelKind::Control,
        label: "control",
        protocol: "controlV1",
        ordered: true,
        max_retransmits: -1,
    },
    DataChannelSpec {
        kind: DataChannelKind::Input,
        label: "input",
        protocol: "1.0",
        ordered: false,
        max_retransmits: 0,
    },
    DataChannelSpec {
        kind: DataChannelKind::Message,
        label: "message",
        protocol: "messageV1",
        ordered: true,
        max_retransmits: -1,
    },
    DataChannelSpec {
        kind: DataChannelKind::Chat,
        label: "chat",
        protocol: "chatV1",
        ordered: true,
        max_retransmits: -1,
    },
];

/// `dataChannelSpecs`: the negotiated channel table, in creation order.
pub fn data_channel_specs() -> &'static [DataChannelSpec] {
    &DATA_CHANNEL_SPECS
}

/// The spec for one channel kind.
pub fn data_channel_spec(kind: DataChannelKind) -> &'static DataChannelSpec {
    // Every variant has exactly one row in the table.
    DATA_CHANNEL_SPECS
        .iter()
        .find(|spec| spec.kind == kind)
        .unwrap_or(&DATA_CHANNEL_SPECS[0])
}

/// Peer connection lifecycle, reduced to the states the UI cares about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerConnectionState {
    #[default]
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
}

/// `peerConnectionStateName`.
pub fn peer_connection_state_name(state: PeerConnectionState) -> &'static str {
    match state {
        PeerConnectionState::New => "new",
        PeerConnectionState::Connecting => "connecting",
        PeerConnectionState::Connected => "connected",
        PeerConnectionState::Disconnected => "disconnected",
        PeerConnectionState::Failed => "failed",
        PeerConnectionState::Closed => "closed",
    }
}

/// A local or remote session description.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionDescription {
    /// "offer" or "answer".
    #[serde(rename = "type")]
    pub kind: String,
    pub sdp: String,
}

/// Which layer surfaced a stream failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamErrorCode {
    Network,
    Signaling,
    Authentication,
    WebRtc,
    ProtocolViolation,
    Unsupported,
    NotImplemented,
    Cancelled,
    #[default]
    Unknown,
}

impl StreamErrorCode {
    /// `StreamError::codeName`.
    pub fn name(self) -> &'static str {
        match self {
            StreamErrorCode::Network => "network",
            StreamErrorCode::Signaling => "signaling",
            StreamErrorCode::Authentication => "authentication",
            StreamErrorCode::WebRtc => "webrtc",
            StreamErrorCode::ProtocolViolation => "protocolViolation",
            StreamErrorCode::Unsupported => "unsupported",
            StreamErrorCode::NotImplemented => "notImplemented",
            StreamErrorCode::Cancelled => "cancelled",
            StreamErrorCode::Unknown => "unknown",
        }
    }
}

/// Typed stream failure bucketed by the layer that surfaced it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StreamError {
    pub code: StreamErrorCode,
    pub message: String,
}

impl StreamError {
    pub fn new(code: StreamErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// `codeName`: the wire/log spelling of the code.
    pub fn code_name(&self) -> &'static str {
        self.code.name()
    }

    /// `description`: `"<code>: <message>"`.
    pub fn description(&self) -> String {
        format!("{}: {}", self.code_name(), self.message)
    }
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code_name(), self.message)
    }
}

impl std::error::Error for StreamError {}

/// High-level stream lifecycle used by controllers and views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamLifecycleState {
    #[default]
    Idle,
    StartingSession,
    Provisioning,
    WaitingForResources,
    ReadyToConnect,
    #[serde(rename = "connectingWebRTC")]
    ConnectingWebRTC,
    Connected,
    Disconnecting,
    Disconnected,
    Failed,
}

/// `streamLifecycleName`.
pub fn stream_lifecycle_name(state: StreamLifecycleState) -> &'static str {
    match state {
        StreamLifecycleState::Idle => "idle",
        StreamLifecycleState::StartingSession => "startingSession",
        StreamLifecycleState::Provisioning => "provisioning",
        StreamLifecycleState::WaitingForResources => "waitingForResources",
        StreamLifecycleState::ReadyToConnect => "readyToConnect",
        StreamLifecycleState::ConnectingWebRTC => "connectingWebRTC",
        StreamLifecycleState::Connected => "connected",
        StreamLifecycleState::Disconnecting => "disconnecting",
        StreamLifecycleState::Disconnected => "disconnected",
        StreamLifecycleState::Failed => "failed",
    }
}

/// Periodic transport/renderer health used by the performance overlay.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StreamingStatsSnapshot {
    pub bitrate_kbps: i32,
    pub frames_per_second: f64,
    pub round_trip_time_ms: f64,
    pub jitter_ms: f64,
    pub decode_time_ms: f64,
    pub packets_lost: i32,
    pub frames_lost: i32,
    pub audio_jitter_ms: f64,
    pub audio_packets_lost: i32,
    pub audio_bitrate_kbps: i32,
    pub negotiated_width: i32,
    pub negotiated_height: i32,
    pub renderer_mode: String,
    pub renderer_frames_received: i32,
    pub renderer_frames_drawn: i32,
    pub renderer_frames_dropped: i32,
    pub render_latency_ms: f64,
    pub active_region: String,
    pub has_latency: bool,
}

/// Runtime stats for a single data channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DataChannelRuntimeStats {
    pub ready_state: String,
    pub buffered_amount_bytes: u64,
}

impl Default for DataChannelRuntimeStats {
    fn default() -> Self {
        Self {
            ready_state: "closed".to_owned(),
            buffered_amount_bytes: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Port of qt/tests/tst_datachannelconfig.cpp (channelSpecsMatchNativeClient; the
    //! quality tier and alias cases live in streaming_config.rs, the SdpProcessor
    //! cases belong to sdp::processor).
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn channel_specs_match_native_client() {
        let specs = data_channel_specs();
        assert_eq!(specs.len(), 4);

        let by_label: HashMap<&str, &DataChannelSpec> =
            specs.iter().map(|spec| (spec.label, spec)).collect();

        // The input channel is unordered and unreliable, so a lost input packet
        // never blocks the 125 Hz stream behind a retransmission.
        let input = by_label["input"];
        assert_eq!(input.protocol, "1.0");
        assert!(!input.ordered);
        assert_eq!(input.max_retransmits, 0);
        assert!(!input.is_reliable());

        let control = by_label["control"];
        assert_eq!(control.protocol, "controlV1");
        assert!(control.ordered);
        assert_eq!(control.max_retransmits, -1);
        assert!(control.is_reliable());

        assert_eq!(by_label["message"].protocol, "messageV1");
        assert_eq!(by_label["chat"].protocol, "chatV1");
    }

    #[test]
    fn spec_lookup_and_kind_names() {
        for spec in data_channel_specs() {
            assert_eq!(data_channel_kind_name(spec.kind), spec.label);
            assert_eq!(data_channel_spec(spec.kind), spec);
        }
    }

    #[test]
    fn state_names() {
        assert_eq!(peer_connection_state_name(PeerConnectionState::New), "new");
        assert_eq!(
            peer_connection_state_name(PeerConnectionState::Closed),
            "closed"
        );
        assert_eq!(stream_lifecycle_name(StreamLifecycleState::Idle), "idle");
        assert_eq!(
            stream_lifecycle_name(StreamLifecycleState::ConnectingWebRTC),
            "connectingWebRTC"
        );
        assert_eq!(
            stream_lifecycle_name(StreamLifecycleState::WaitingForResources),
            "waitingForResources"
        );
        // serde spelling matches the name functions.
        let json = serde_json::to_string(&StreamLifecycleState::ConnectingWebRTC).unwrap();
        assert_eq!(json, "\"connectingWebRTC\"");
        let json = serde_json::to_string(&StreamLifecycleState::StartingSession).unwrap();
        assert_eq!(json, "\"startingSession\"");
    }

    #[test]
    fn stream_error_description() {
        let error = StreamError::new(StreamErrorCode::ProtocolViolation, "bad answer");
        assert_eq!(error.code_name(), "protocolViolation");
        assert_eq!(error.description(), "protocolViolation: bad answer");
        assert_eq!(error.to_string(), "protocolViolation: bad answer");
        assert_eq!(StreamError::default().code_name(), "unknown");
        assert_eq!(StreamErrorCode::WebRtc.name(), "webrtc");
        assert_eq!(StreamErrorCode::NotImplemented.name(), "notImplemented");
    }

    #[test]
    fn runtime_stats_default_closed() {
        let stats = DataChannelRuntimeStats::default();
        assert_eq!(stats.ready_state, "closed");
        assert_eq!(stats.buffered_amount_bytes, 0);
        let description = SessionDescription {
            kind: "offer".to_owned(),
            sdp: "v=0".to_owned(),
        };
        let json = serde_json::to_value(&description).unwrap();
        assert_eq!(json["type"], "offer");
    }
}
