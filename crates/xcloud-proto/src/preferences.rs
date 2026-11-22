//! Port of qt/src/xcloud/StreamPreferences.{h,cpp}
//!
//! Launch-time stream tuning and the spoofed device profile sent in the
//! `X-MS-Device-Info` header and the start-stream `settings` block.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Which service a session is started against (`/v5/sessions/<kind>/play`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    Cloud,
    Home,
}

/// `streamKindName`: the path segment used by the session endpoints.
pub fn stream_kind_name(kind: StreamKind) -> &'static str {
    match kind {
        StreamKind::Cloud => "cloud",
        StreamKind::Home => "home",
    }
}

/// Resolution ladder sent to the server. The server decides the actual tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum StreamResolutionMode {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "p720")]
    P720,
    #[default]
    #[serde(rename = "p1080")]
    P1080,
    #[serde(rename = "p1440")]
    P1440,
    #[serde(rename = "p1080HQ")]
    P1080HQ,
}

/// `streamResolutionFromString`: case-insensitive, whitespace-tolerant parse
/// that falls back to `fallback` for anything unknown.
pub fn stream_resolution_from_string(
    value: &str,
    fallback: StreamResolutionMode,
) -> StreamResolutionMode {
    match value.trim().to_lowercase().as_str() {
        "auto" => StreamResolutionMode::Auto,
        "p720" => StreamResolutionMode::P720,
        "p1080" => StreamResolutionMode::P1080,
        "p1440" => StreamResolutionMode::P1440,
        "p1080hq" => StreamResolutionMode::P1080HQ,
        _ => fallback,
    }
}

/// `streamResolutionToString`: the persisted settings spelling.
pub fn stream_resolution_to_string(mode: StreamResolutionMode) -> &'static str {
    match mode {
        StreamResolutionMode::Auto => "auto",
        StreamResolutionMode::P720 => "p720",
        StreamResolutionMode::P1080 => "p1080",
        StreamResolutionMode::P1440 => "p1440",
        StreamResolutionMode::P1080HQ => "p1080HQ",
    }
}

/// Spoofed client OS name sent by default: the client presents itself as Edge on an Xbox One.
pub const DEFAULT_OS_NAME: &str = "xbox";

/// User-Agent of Microsoft Edge on Xbox One, used wherever the client sends a browser UA.
pub const XBOX_ONE_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; Xbox; Xbox One) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/119.0.0.0 Safari/537.36 Edg/119.0.0.0";

/// `streamResolutionDisplayWidth`: advertised display width in pixels.
pub fn stream_resolution_display_width(mode: StreamResolutionMode) -> u32 {
    match mode {
        StreamResolutionMode::Auto | StreamResolutionMode::P720 => 1280,
        StreamResolutionMode::P1080 => 1920,
        StreamResolutionMode::P1440 => 2560,
        StreamResolutionMode::P1080HQ => 3840,
    }
}

/// `streamResolutionDisplayHeight`: advertised display height in pixels.
pub fn stream_resolution_display_height(mode: StreamResolutionMode) -> u32 {
    match mode {
        StreamResolutionMode::Auto | StreamResolutionMode::P720 => 720,
        StreamResolutionMode::P1080 => 1080,
        StreamResolutionMode::P1440 => 1440,
        StreamResolutionMode::P1080HQ => 2160,
    }
}

/// `normalizedOsNameOverride`: accepts only the known OS names, case-insensitively.
/// Returns `None` for anything else so the caller falls back to [`DEFAULT_OS_NAME`].
pub fn normalized_os_name_override(raw: &str) -> Option<&'static str> {
    match raw.trim().to_lowercase().as_str() {
        "android" => Some("android"),
        "windows" => Some("windows"),
        "tizen" => Some("tizen"),
        "xbox" => Some("xbox"),
        _ => None,
    }
}

/// Launch preferences for a cloud or home session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamPreferences {
    pub resolution: StreamResolutionMode,
    pub locale: String,
    #[serde(rename = "preferIPv6")]
    pub prefer_ipv6: bool,
    #[serde(rename = "preferredRegionId")]
    pub preferred_region_id: String,
    #[serde(rename = "fallbackRegionNames")]
    pub fallback_region_names: Vec<String>,
    /// Explicit client osName override ("android", "windows", "tizen", "xbox"); empty uses "xbox".
    #[serde(rename = "osNameOverride")]
    pub os_name_override: String,
}

impl Default for StreamPreferences {
    fn default() -> Self {
        Self {
            resolution: StreamResolutionMode::P1080,
            locale: "en-US".to_owned(),
            prefer_ipv6: false,
            preferred_region_id: String::new(),
            fallback_region_names: Vec::new(),
            os_name_override: String::new(),
        }
    }
}

impl StreamPreferences {
    /// Resolved OS name after normalising the override.
    pub fn effective_os_name(&self) -> &'static str {
        normalized_os_name_override(&self.os_name_override).unwrap_or(DEFAULT_OS_NAME)
    }
}

/// `makeDeviceInfo`: the `X-MS-Device-Info` payload that advertises the spoofed
/// device profile (Microsoft Edge on an Xbox One). This header is what makes the service return 1080p instead
/// of 720p, so the shape must match the reference clients exactly (the string
/// is built by hand to keep key order and formatting byte-identical).
///
/// `locale` is accepted for signature parity with the C++ but is not part of the payload.
pub fn make_device_info(
    _locale: &str,
    resolution: StreamResolutionMode,
    os_name_override: &str,
) -> String {
    let os_name = normalized_os_name_override(os_name_override).unwrap_or(DEFAULT_OS_NAME);
    let width = stream_resolution_display_width(resolution);
    let height = stream_resolution_display_height(resolution);

    format!(
        "{{\"appInfo\":{{\"env\":{{\"clientAppId\":\"www.xbox.com\",\"clientAppType\":\"browser\",\
         \"clientAppVersion\":\"21.1.98\",\"clientSdkVersion\":\"8.5.3\",\"httpEnvironment\":\"prod\",\
         \"sdkInstallId\":\"\"}}}},\"dev\":{{\"hw\":{{\"make\":\"Microsoft\",\"model\":\"Xbox One\",\
         \"sdktype\":\"web\"}},\"os\":{{\"name\":\"{os_name}\",\"ver\":\"10.0.25398.4909\",\"platform\":\"console\"}},\
         \"displayInfo\":{{\"dimensions\":{{\"widthInPixels\":{width},\"heightInPixels\":{height}}},\
         \"pixelDensity\":{{\"dpiX\":2,\"dpiY\":2}}}}}},\"browser\":{{\"browserName\":\"edge\",\
         \"browserVersion\":\"119.0\"}}}}"
    )
}

/// `makeStreamSettings`: settings block embedded in the start-stream body.
pub fn make_stream_settings(preferences: &StreamPreferences) -> Value {
    json!({
        "nanoVersion": "V3;WebrtcTransport.dll",
        "enableOptionalDataCollection": false,
        "enableTextToSpeech": false,
        "highContrast": 0,
        "locale": preferences.locale,
        "useIceConnection": false,
        "timezoneOffsetMinutes": 120,
        "sdkType": "web",
        "osName": preferences.effective_os_name(),
    })
}

#[cfg(test)]
mod tests {
    //! Port of qt/tests/tst_streampreferences.cpp
    use super::*;

    #[test]
    fn display_dimensions() {
        assert_eq!(
            stream_resolution_display_width(StreamResolutionMode::P1080),
            1920
        );
        assert_eq!(
            stream_resolution_display_height(StreamResolutionMode::P1080),
            1080
        );
        assert_eq!(
            stream_resolution_display_width(StreamResolutionMode::P1080HQ),
            3840
        );
        assert_eq!(
            stream_resolution_display_height(StreamResolutionMode::P1080HQ),
            2160
        );
    }

    #[test]
    fn string_round_trip() {
        assert_eq!(
            stream_resolution_to_string(StreamResolutionMode::P1080HQ),
            "p1080HQ"
        );
        assert_eq!(
            stream_resolution_from_string("p1440", StreamResolutionMode::P1080),
            StreamResolutionMode::P1440
        );
        assert_eq!(
            stream_resolution_from_string("AUTO", StreamResolutionMode::P1080),
            StreamResolutionMode::Auto
        );
        assert_eq!(
            stream_resolution_from_string("bogus", StreamResolutionMode::P720),
            StreamResolutionMode::P720
        );
    }

    #[test]
    fn normalization() {
        let mut preferences = StreamPreferences {
            resolution: StreamResolutionMode::P720,
            ..Default::default()
        };
        assert_eq!(preferences.effective_os_name(), "xbox");

        preferences.os_name_override = "  Tizen ".to_owned();
        assert_eq!(preferences.effective_os_name(), "tizen");

        preferences.os_name_override = "bogus".to_owned();
        assert_eq!(preferences.effective_os_name(), "xbox");
    }

    #[test]
    fn device_info_matches_reference() {
        let device_info = make_device_info("en-US", StreamResolutionMode::P1080, "");
        assert!(device_info.starts_with("{\"appInfo\":"));
        assert!(device_info.contains("\"clientAppId\":\"www.xbox.com\""));
        assert!(device_info.contains(
            "\"os\":{\"name\":\"xbox\",\"ver\":\"10.0.25398.4909\",\"platform\":\"console\"}"
        ));
        assert!(device_info.contains("\"widthInPixels\":1920,\"heightInPixels\":1080"));
        assert!(device_info.contains("\"model\":\"Xbox One\""));
        assert!(device_info.contains("\"browserName\":\"edge\",\"browserVersion\":\"119.0\""));

        // Must be valid JSON.
        let document: Value = serde_json::from_str(&device_info).unwrap();
        assert!(document.is_object());

        let overridden = make_device_info("en-US", StreamResolutionMode::P720, "tizen");
        assert!(overridden.contains("\"name\":\"tizen\""));
        assert!(overridden.contains("\"widthInPixels\":1280"));
    }

    #[test]
    fn settings_block() {
        let preferences = StreamPreferences {
            locale: "de-DE".to_owned(),
            resolution: StreamResolutionMode::P1440,
            ..Default::default()
        };
        let settings = make_stream_settings(&preferences);
        assert_eq!(settings["nanoVersion"], "V3;WebrtcTransport.dll");
        assert_eq!(settings["locale"], "de-DE");
        assert_eq!(settings["osName"], "xbox");
        assert_eq!(settings["sdkType"], "web");
        assert_eq!(settings["timezoneOffsetMinutes"], 120);
    }

    #[test]
    fn resolution_serde_uses_wire_spelling() {
        let json = serde_json::to_string(&StreamResolutionMode::P1080HQ).unwrap();
        assert_eq!(json, "\"p1080HQ\"");
        let parsed: StreamResolutionMode = serde_json::from_str("\"p720\"").unwrap();
        assert_eq!(parsed, StreamResolutionMode::P720);
        assert_eq!(stream_kind_name(StreamKind::Cloud), "cloud");
        assert_eq!(stream_kind_name(StreamKind::Home), "home");
    }
}
