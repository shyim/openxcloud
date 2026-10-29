//! Configurable media pipeline adapter shared by portable transports.
use crate::MediaSink;
use xcloud_proto::streaming_config::StreamingConfig;
use xcloud_proto::streaming_types::StreamingStatsSnapshot;

/// A media sink configured for each new streaming session.
pub trait ConfigurableMediaSink: MediaSink {
    /// Applies the streaming config (`NativeMediaPipeline::configure`).
    fn configure(&mut self, config: &StreamingConfig);
}

/// Adapter for a plain [`MediaSink`]: `configure` is a no-op.
pub(crate) struct PlainSink(pub(crate) Box<dyn MediaSink>);

impl MediaSink for PlainSink {
    fn on_video_rtp(&mut self, packet: &[u8], now_ms: u64) {
        self.0.on_video_rtp(packet, now_ms);
    }

    fn on_audio_rtp(&mut self, packet: &[u8], now_ms: u64) {
        self.0.on_audio_rtp(packet, now_ms);
    }

    fn tick(&mut self, now_ms: u64) {
        self.0.tick(now_ms);
    }

    fn reset(&mut self) {
        self.0.reset();
    }

    fn stats(&self) -> StreamingStatsSnapshot {
        self.0.stats()
    }
}

impl ConfigurableMediaSink for PlainSink {
    fn configure(&mut self, _config: &StreamingConfig) {}
}
