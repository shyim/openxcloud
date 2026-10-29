//! Port of qt/src/streaming/channels/ChatChannel.{h,cpp}
//!
//! Audio chat negotiation channel ("chatV1"). The chat channel opens but sends no
//! audio (receive-only client); on open it announces `chatReady`. Mirrors
//! channel/chat.ts.

use crate::streaming_types::DataChannelKind;

pub use super::control::ChannelAction;

/// The chat channel opens but sends no audio (receive-only client).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChatChannel;

impl ChatChannel {
    pub const LABEL: &'static str = "chat";
    pub const PROTOCOL_NAME: &'static str = "chatV1";
    /// The literal the C++ sent (`{"message":"chatReady"}`).
    pub const CHAT_READY: &'static str = r#"{"message":"chatReady"}"#;

    pub fn new() -> Self {
        Self
    }

    /// `onChannelOpen`: sends `chatReady`. The C++ has no once-guard here, so
    /// every open announces again.
    pub fn on_channel_open(&self) -> Vec<ChannelAction> {
        vec![ChannelAction::SendText {
            kind: DataChannelKind::Chat,
            text: Self::CHAT_READY.to_owned(),
        }]
    }

    /// `destroy`: nothing to reset.
    pub fn destroy(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_cpp() {
        assert_eq!(ChatChannel::LABEL, "chat");
        assert_eq!(ChatChannel::PROTOCOL_NAME, "chatV1");
    }

    #[test]
    fn open_sends_chat_ready() {
        let channel = ChatChannel::new();
        assert_eq!(
            channel.on_channel_open(),
            vec![ChannelAction::SendText {
                kind: DataChannelKind::Chat,
                text: r#"{"message":"chatReady"}"#.to_owned(),
            }]
        );
        // The literal is valid compact JSON with the same shape as the control
        // channel's messages.
        let value: serde_json::Value = serde_json::from_str(ChatChannel::CHAT_READY).unwrap();
        assert_eq!(value.to_string(), ChatChannel::CHAT_READY);
    }

    #[test]
    fn open_is_not_guarded() {
        let mut channel = ChatChannel;
        assert_eq!(channel.on_channel_open().len(), 1);
        assert_eq!(channel.on_channel_open().len(), 1);
        channel.destroy();
        assert_eq!(channel.on_channel_open().len(), 1);
    }
}
