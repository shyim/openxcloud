//! Port of qt/src/streaming/StreamingConfig.{h,cpp}
//!
//! Negotiated media parameters, quality tier profiles and channel version ranges.

use serde::{Deserialize, Serialize};

use crate::preferences::StreamResolutionMode;

/// Client capability profile for one quality tier, from green-nx's
/// tier_profile(): 720p / 1080p / 1080p HQ (30 Mbps).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityTierProfile {
    pub width: u32,
    pub height: u32,
    pub max_bitrate_kbps: u32,
    pub fps: u32,
}

impl Default for QualityTierProfile {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            max_bitrate_kbps: 20000,
            fps: 60,
        }
    }
}

/// `qualityTierProfile`: the capability profile advertised for a resolution mode.
pub fn quality_tier_profile(mode: StreamResolutionMode) -> QualityTierProfile {
    let (width, height, max_bitrate_kbps) = match mode {
        StreamResolutionMode::P720 => (1280, 720, 10000),
        StreamResolutionMode::P1080HQ => (1920, 1080, 30000),
        StreamResolutionMode::P1440 => (2560, 1440, 30000),
        StreamResolutionMode::Auto | StreamResolutionMode::P1080 => (1920, 1080, 20000),
    };
    QualityTierProfile {
        width,
        height,
        max_bitrate_kbps,
        fps: 60,
    }
}

/// `resolutionAlias`: the official client's resolution alias used by
/// userRequestedResolutionUpdate.
pub fn resolution_alias(mode: StreamResolutionMode) -> &'static str {
    match mode {
        StreamResolutionMode::Auto => "Auto",
        StreamResolutionMode::P720 => "720",
        StreamResolutionMode::P1080 => "1080",
        StreamResolutionMode::P1440 => "1440",
        StreamResolutionMode::P1080HQ => "1080HQ",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VideoCodecPreference {
    H264High,
    H264Main,
    H264Baseline,
    Vp9,
    Vp8,
    UlpFec,
    FlexFec,
    Other,
}

impl VideoCodecPreference {
    /// The `preferredVideoCodecOrder` spelling used by `StreamingConfig::defaults()`.
    pub fn name(self) -> &'static str {
        match self {
            VideoCodecPreference::H264High => "h264High",
            VideoCodecPreference::H264Main => "h264Main",
            VideoCodecPreference::H264Baseline => "h264Baseline",
            VideoCodecPreference::Vp9 => "vp9",
            VideoCodecPreference::Vp8 => "vp8",
            VideoCodecPreference::UlpFec => "ulpfec",
            VideoCodecPreference::FlexFec => "flexfec",
            VideoCodecPreference::Other => "other",
        }
    }
}

/// Media tuning applied to the local SDP offer before it is sent to the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StreamingConfig {
    pub preferred_video_codec_order: Vec<String>,
    /// SDP codec name to move to the front of the video section. Empty keeps H264 first.
    pub preferred_sdp_codec: String,
    pub max_video_bitrate_kbps: u32,
    pub max_audio_bitrate_kbps: u32,
    pub stereo_audio_enabled: bool,
    pub keyframe_request_interval_seconds: u32,
    pub video_width_hint: u32,
    pub video_height_hint: u32,
    pub preferred_frame_rate: u32,
    pub low_latency_mode_enabled: bool,
    pub enable_chat_channel: bool,
    pub preferred_color_range: String,
    pub log_local_sdp_offer: bool,
    pub requested_video_max_frame_rate: u32,
    pub h264_fallback_profile_level_id: String,

    /// Max bitrate advertised on clientdevicecapabilities. This is the quality
    /// lever green-nx uses instead of `b=AS` lines in the offer.
    pub max_bitrate_kbps: u32,
    /// When false, no `b=AS` line is injected (the native client path).
    pub inject_bitrate_lines: bool,
    /// Resolution alias for userRequestedResolutionUpdate.
    pub resolution_alias: String,
    /// Scale the 720p decode capability in the offer to 1080p60.
    pub scale_video_caps_to_1080: bool,
    /// Override the H.264 profile-level-id (home streams want 42e020).
    pub h264_profile_level_id: String,
}

impl Default for StreamingConfig {
    /// Equivalent to `StreamingConfig::defaults()` in the C++: the field defaults
    /// plus the preferred codec order.
    fn default() -> Self {
        Self::defaults()
    }
}

impl StreamingConfig {
    /// `StreamingConfig::defaults()`.
    pub fn defaults() -> Self {
        Self {
            preferred_video_codec_order: [
                VideoCodecPreference::H264High,
                VideoCodecPreference::H264Main,
                VideoCodecPreference::H264Baseline,
                VideoCodecPreference::Vp9,
                VideoCodecPreference::Vp8,
                VideoCodecPreference::UlpFec,
                VideoCodecPreference::FlexFec,
            ]
            .iter()
            .map(|codec| codec.name().to_owned())
            .collect(),
            preferred_sdp_codec: String::new(),
            max_video_bitrate_kbps: 120000,
            max_audio_bitrate_kbps: 48000,
            stereo_audio_enabled: true,
            keyframe_request_interval_seconds: 5,
            video_width_hint: 1920,
            video_height_hint: 1080,
            preferred_frame_rate: 60,
            low_latency_mode_enabled: true,
            enable_chat_channel: false,
            preferred_color_range: String::new(),
            log_local_sdp_offer: false,
            requested_video_max_frame_rate: 60,
            h264_fallback_profile_level_id: "640029".to_owned(),
            max_bitrate_kbps: 20000,
            inject_bitrate_lines: true,
            resolution_alias: "1080".to_owned(),
            scale_video_caps_to_1080: false,
            h264_profile_level_id: String::new(),
        }
    }

    /// Applies a quality tier the way `SignalingOrchestrator::installChannels`
    /// does (SignalingOrchestrator.cpp:135-142): the capability dimension hints,
    /// both bitrate levers (`maxBitrateKbps` for clientdevicecapabilities and
    /// `maxVideoBitrateKbps`, which drives the injected `b=AS` line), the frame
    /// rate clamped with `qBound(1, preferredFrameRate, profile.fps)` and
    /// mirrored into `requestedVideoMaxFrameRate`, and the resolution alias.
    pub fn apply_quality_tier(&mut self, mode: StreamResolutionMode) {
        let profile = quality_tier_profile(mode);
        self.video_width_hint = profile.width;
        self.video_height_hint = profile.height;
        self.max_bitrate_kbps = profile.max_bitrate_kbps;
        self.max_video_bitrate_kbps = profile.max_bitrate_kbps;
        self.preferred_frame_rate = self.preferred_frame_rate.clamp(1, profile.fps.max(1));
        self.requested_video_max_frame_rate = self.preferred_frame_rate;
        self.resolution_alias = resolution_alias(mode).to_owned();
    }

    /// Builder form of [`StreamingConfig::apply_quality_tier`].
    pub fn with_resolution(mut self, mode: StreamResolutionMode) -> Self {
        self.apply_quality_tier(mode);
        self
    }
}

#[cfg(test)]
mod tests {
    //! Port of qt/tests/tst_datachannelconfig.cpp (qualityTierProfiles, resolutionAliases).
    //! channelSpecsMatchNativeClient lives in streaming_types.rs; sdpOptionsCanOmitBitrateLines
    //! and sdpOptionsScaleVideoCaps exercise SdpProcessor and belong to sdp::processor.
    use super::*;

    #[test]
    fn quality_tier_profiles() {
        let p720 = quality_tier_profile(StreamResolutionMode::P720);
        assert_eq!(p720.width, 1280);
        assert_eq!(p720.height, 720);
        assert_eq!(p720.max_bitrate_kbps, 10000);

        let p1080 = quality_tier_profile(StreamResolutionMode::P1080);
        assert_eq!(p1080.max_bitrate_kbps, 20000);

        let hq = quality_tier_profile(StreamResolutionMode::P1080HQ);
        assert_eq!(hq.max_bitrate_kbps, 30000);
        assert_eq!(hq.width, 1920);
    }

    #[test]
    fn resolution_aliases() {
        assert_eq!(resolution_alias(StreamResolutionMode::Auto), "Auto");
        assert_eq!(resolution_alias(StreamResolutionMode::P720), "720");
        assert_eq!(resolution_alias(StreamResolutionMode::P1080), "1080");
        assert_eq!(resolution_alias(StreamResolutionMode::P1080HQ), "1080HQ");
    }

    #[test]
    fn defaults_match_cpp() {
        let config = StreamingConfig::defaults();
        assert_eq!(config, StreamingConfig::default());
        assert_eq!(
            config.preferred_video_codec_order,
            vec![
                "h264High",
                "h264Main",
                "h264Baseline",
                "vp9",
                "vp8",
                "ulpfec",
                "flexfec"
            ]
        );
        assert_eq!(config.max_video_bitrate_kbps, 120000);
        assert_eq!(config.max_audio_bitrate_kbps, 48000);
        assert!(config.stereo_audio_enabled);
        assert_eq!(config.h264_fallback_profile_level_id, "640029");
        assert_eq!(config.max_bitrate_kbps, 20000);
        assert!(config.inject_bitrate_lines);
        assert_eq!(config.resolution_alias, "1080");
        assert!(!config.scale_video_caps_to_1080);
        assert!(config.h264_profile_level_id.is_empty());
        assert_eq!(
            quality_tier_profile(StreamResolutionMode::Auto),
            QualityTierProfile::default()
        );
    }

    #[test]
    fn with_resolution_applies_tier() {
        // Mirrors SignalingOrchestrator::installChannels: both bitrate levers
        // take the profile value, the frame rate is clamped (not overwritten)
        // and copied into requestedVideoMaxFrameRate.
        let config = StreamingConfig::defaults().with_resolution(StreamResolutionMode::P1080HQ);
        assert_eq!(config.max_bitrate_kbps, 30000);
        assert_eq!(config.max_video_bitrate_kbps, 30000);
        assert_eq!(config.resolution_alias, "1080HQ");
        assert_eq!(
            (config.video_width_hint, config.video_height_hint),
            (1920, 1080)
        );
        assert_eq!(config.preferred_frame_rate, 60);
        assert_eq!(config.requested_video_max_frame_rate, 60);

        let p720 = StreamingConfig::defaults().with_resolution(StreamResolutionMode::P720);
        assert_eq!(p720.max_bitrate_kbps, 10000);
        assert_eq!(p720.max_video_bitrate_kbps, 10000);
        assert_eq!(p720.requested_video_max_frame_rate, 60);
        assert_eq!((p720.video_width_hint, p720.video_height_hint), (1280, 720));
        assert_eq!(p720.resolution_alias, "720");

        // qBound(1, preferredFrameRate, profile.fps): a user setting below the
        // tier cap survives, above it is clamped, and 0 becomes 1.
        let thirty = StreamingConfig {
            preferred_frame_rate: 30,
            requested_video_max_frame_rate: 30,
            ..StreamingConfig::defaults()
        }
        .with_resolution(StreamResolutionMode::P1080);
        assert_eq!(thirty.preferred_frame_rate, 30);
        assert_eq!(thirty.requested_video_max_frame_rate, 30);
        let too_fast = StreamingConfig {
            preferred_frame_rate: 120,
            requested_video_max_frame_rate: 120,
            ..StreamingConfig::defaults()
        }
        .with_resolution(StreamResolutionMode::P1080);
        assert_eq!(too_fast.preferred_frame_rate, 60);
        assert_eq!(too_fast.requested_video_max_frame_rate, 60);
        let zero = StreamingConfig {
            preferred_frame_rate: 0,
            ..StreamingConfig::defaults()
        }
        .with_resolution(StreamResolutionMode::P720);
        assert_eq!(zero.preferred_frame_rate, 1);
        assert_eq!(zero.requested_video_max_frame_rate, 1);

        // Everything the orchestrator leaves alone stays at its default.
        let untouched = StreamingConfig {
            max_bitrate_kbps: 20000,
            max_video_bitrate_kbps: 120000,
            video_width_hint: 1920,
            video_height_hint: 1080,
            resolution_alias: "1080".to_owned(),
            ..p720.clone()
        };
        assert_eq!(untouched, StreamingConfig::defaults());

        let mut in_place = StreamingConfig::defaults();
        in_place.apply_quality_tier(StreamResolutionMode::P720);
        assert_eq!(in_place, p720);
    }
}
