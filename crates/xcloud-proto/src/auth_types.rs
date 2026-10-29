//! Port of qt/src/auth/AuthTypes.h (and the `StreamTokens` JSON codec plus
//! `preferredRegion` from qt/src/auth/TokenStore.cpp).
//!
//! Value types shared by the Microsoft/Xbox token exchange.
//!
//! The `from_json` readers follow `QJsonValue::toString()/toInt()/toBool()`:
//! a missing, null or mistyped field reads as its default and parsing never
//! fails. `Deserialize` routes through the same readers.

use serde::Serialize;
use serde_json::Value;

use crate::models::{array_at, bool_at, deserialize_via_from_json, int_at_or, string_at};

deserialize_via_from_json!(DeviceCodeInfo, LoginRegion, StreamTokens);

/// Human-facing and polling-facing values produced by the device-code endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceCodeInfo {
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: i32,
    pub interval: i32,
    /// Opaque polling token, not the human readable code.
    pub device_code: String,
}

impl Default for DeviceCodeInfo {
    fn default() -> Self {
        Self {
            user_code: String::new(),
            verification_uri: String::new(),
            verification_uri_complete: String::new(),
            expires_in: 900,
            interval: 5,
            device_code: String::new(),
        }
    }
}

impl DeviceCodeInfo {
    /// The device-code endpoint body as MicrosoftAuthService reads it:
    /// `expires_in`/`interval` fall back to 900/5 (`toInt(900)`, `toInt(5)`)
    /// when missing, null or not a whole number. Whether an empty
    /// `device_code` is an error is the caller's decision, as in the C++.
    pub fn from_json(object: &Value) -> Self {
        Self {
            user_code: string_at(object, "user_code"),
            verification_uri: string_at(object, "verification_uri"),
            verification_uri_complete: string_at(object, "verification_uri_complete"),
            expires_in: int_at_or(object, "expires_in", 900),
            interval: int_at_or(object, "interval", 5),
            device_code: string_at(object, "device_code"),
        }
    }

    /// URL the user should open, preferring the pre-filled complete variant.
    /// Empty when the endpoint gave no verification URI at all.
    pub fn sign_in_url(&self) -> String {
        if !self.verification_uri_complete.is_empty() {
            return self.verification_uri_complete.clone();
        }
        if self.verification_uri.is_empty() {
            return String::new();
        }
        format!("{}?otc={}", self.verification_uri, self.user_code)
    }
}

/// An Xbox Live authentication token together with the user hash claim it carries.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, serde::Deserialize)]
pub struct XboxToken {
    pub token: String,
    pub user_hash: String,
}

/// One cloud streaming region advertised by the GSSV login offering.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct LoginRegion {
    pub name: String,
    #[serde(rename = "baseUri")]
    pub base_uri: String,
    #[serde(rename = "isDefault")]
    pub is_default: bool,
}

impl LoginRegion {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// `LoginRegion::fromJson`: missing, null or mistyped fields read as `""`/`false`.
    pub fn from_json(object: &Value) -> Self {
        Self {
            name: string_at(object, "name"),
            base_uri: string_at(object, "baseUri"),
            is_default: bool_at(object, "isDefault"),
        }
    }
}

/// The authenticated token bundle used by stream launch and Xbox web requests.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct StreamTokens {
    #[serde(rename = "xhomeToken")]
    pub xhome_token: String,
    #[serde(rename = "xhomeHost")]
    pub xhome_host: String,
    #[serde(rename = "xcloudToken")]
    pub xcloud_token: String,
    #[serde(rename = "xcloudHost")]
    pub xcloud_host: String,
    #[serde(rename = "xcloudF2PToken")]
    pub xcloud_f2p_token: String,
    #[serde(rename = "xcloudF2PHost")]
    pub xcloud_f2p_host: String,
    #[serde(rename = "webToken")]
    pub web_token: String,
    #[serde(rename = "webTokenUHS")]
    pub web_token_uhs: String,
    #[serde(rename = "xcloudRegions")]
    pub xcloud_regions: Vec<LoginRegion>,
}

impl StreamTokens {
    /// At least one streaming token is present.
    pub fn is_valid(&self) -> bool {
        !self.xhome_token.is_empty() || !self.xcloud_token.is_empty()
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// `StreamTokens::fromJson` (TokenStore.cpp): a null or non-array
    /// `xcloudRegions` reads as no regions; non-object entries read as empty regions.
    pub fn from_json(object: &Value) -> Self {
        Self {
            xhome_token: string_at(object, "xhomeToken"),
            xhome_host: string_at(object, "xhomeHost"),
            xcloud_token: string_at(object, "xcloudToken"),
            xcloud_host: string_at(object, "xcloudHost"),
            xcloud_f2p_token: string_at(object, "xcloudF2PToken"),
            xcloud_f2p_host: string_at(object, "xcloudF2PHost"),
            web_token: string_at(object, "webToken"),
            web_token_uhs: string_at(object, "webTokenUHS"),
            xcloud_regions: array_at(object, "xcloudRegions")
                .iter()
                .map(LoginRegion::from_json)
                .collect(),
        }
    }
}

/// `preferredRegion`: picks the preferred region, honouring a pinned name when it
/// is present, then the advertised default, then the first entry.
/// Returns an empty `LoginRegion` when `regions` is empty.
pub fn preferred_region(regions: &[LoginRegion], preference: &str) -> LoginRegion {
    if !preference.is_empty()
        && let Some(region) = regions.iter().find(|region| region.name == preference)
    {
        return region.clone();
    }
    regions
        .iter()
        .find(|region| region.is_default)
        .or_else(|| regions.first())
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(name: &str, is_default: bool) -> LoginRegion {
        LoginRegion {
            name: name.to_owned(),
            base_uri: format!("https://{name}.gssv-play-prod.xboxlive.com"),
            is_default,
        }
    }

    #[test]
    fn sign_in_url_prefers_complete_variant() {
        // From tst_authservice.cpp deviceCodeFlow: the complete URI wins when present.
        let info = DeviceCodeInfo {
            user_code: "ABCD-EFGH".to_owned(),
            verification_uri: "https://microsoft.com/devicelogin".to_owned(),
            verification_uri_complete: String::new(),
            ..Default::default()
        };
        assert_eq!(
            info.sign_in_url(),
            "https://microsoft.com/devicelogin?otc=ABCD-EFGH"
        );

        let complete = DeviceCodeInfo {
            verification_uri_complete: "https://microsoft.com/link?otc=X".to_owned(),
            ..info.clone()
        };
        assert_eq!(complete.sign_in_url(), "https://microsoft.com/link?otc=X");

        let empty = DeviceCodeInfo::default();
        assert_eq!(empty.sign_in_url(), "");
        assert_eq!(empty.expires_in, 900);
        assert_eq!(empty.interval, 5);
    }

    #[test]
    fn device_code_info_parses_wire_names() {
        let info: DeviceCodeInfo = serde_json::from_str(
            r#"{"user_code":"ABCD-EFGH","device_code":"DC","verification_uri":"https://microsoft.com/devicelogin","expires_in":600,"interval":2}"#,
        )
        .unwrap();
        assert_eq!(info.user_code, "ABCD-EFGH");
        assert_eq!(info.device_code, "DC");
        assert_eq!(info.expires_in, 600);
        assert_eq!(info.interval, 2);
        assert_eq!(info.verification_uri_complete, "");
        assert_eq!(
            DeviceCodeInfo::from_json(&serde_json::to_value(&info).unwrap()),
            info
        );
    }

    #[test]
    fn device_code_info_tolerates_null_and_mistyped_fields() {
        // MicrosoftAuthService.cpp: toInt(900) / toInt(5) defaults, toString() for
        // the rest. Nothing here can fail.
        let info = DeviceCodeInfo::from_json(&serde_json::json!({
            "user_code": null, "device_code": "DC", "verification_uri": 7,
            "expires_in": null, "interval": "5"
        }));
        assert_eq!(info.user_code, "");
        assert_eq!(info.device_code, "DC");
        assert_eq!(info.verification_uri, "");
        assert_eq!(info.expires_in, 900);
        assert_eq!(info.interval, 5);
        let info = DeviceCodeInfo::from_json(&serde_json::json!({
            "expires_in": 600.0, "interval": 2.5
        }));
        assert_eq!(info.expires_in, 600);
        assert_eq!(info.interval, 5);
        let via_serde: DeviceCodeInfo =
            serde_json::from_str(r#"{"expires_in":"900","interval":null,"device_code":null}"#)
                .unwrap();
        assert_eq!(via_serde, DeviceCodeInfo::default());
    }

    #[test]
    fn stream_tokens_round_trip() {
        let tokens = StreamTokens {
            xhome_token: "XHOMEGS".to_owned(),
            xhome_host: "https://home".to_owned(),
            xcloud_token: "XCLOUDGS".to_owned(),
            xcloud_host: "https://cloud".to_owned(),
            xcloud_f2p_token: "F2P".to_owned(),
            xcloud_f2p_host: "https://f2p".to_owned(),
            web_token: "WEB".to_owned(),
            web_token_uhs: "UHS".to_owned(),
            xcloud_regions: vec![region("eastus", true), region("westeurope", false)],
        };
        let json = tokens.to_json();
        assert_eq!(json["webTokenUHS"], "UHS");
        assert_eq!(json["xcloudF2PToken"], "F2P");
        assert_eq!(
            json["xcloudRegions"][0]["baseUri"],
            tokens.xcloud_regions[0].base_uri
        );
        assert_eq!(json["xcloudRegions"][0]["isDefault"], true);
        let parsed = StreamTokens::from_json(&json);
        assert_eq!(parsed, tokens);
        assert_eq!(
            serde_json::from_value::<StreamTokens>(json).unwrap(),
            tokens
        );
        assert!(parsed.is_valid());
        assert!(!StreamTokens::default().is_valid());
        assert!(StreamTokens::from_json(&serde_json::json!({})) == StreamTokens::default());
    }

    #[test]
    fn stream_tokens_tolerate_null_and_mistyped_fields() {
        // TokenStore.cpp StreamTokens::fromJson / AuthTypes.h LoginRegion::fromJson:
        // toString()/toBool()/toArray() read null or mistyped values as empty.
        let tokens = StreamTokens::from_json(&serde_json::json!({
            "xhomeToken": "H", "xcloudToken": null, "xcloudHost": 1, "xcloudRegions": null
        }));
        assert_eq!(tokens.xhome_token, "H");
        assert_eq!(tokens.xcloud_token, "");
        assert_eq!(tokens.xcloud_host, "");
        assert!(tokens.xcloud_regions.is_empty());
        assert!(tokens.is_valid());

        let tokens = StreamTokens::from_json(&serde_json::json!({
            "xcloudRegions": [{"name": "eastus", "baseUri": null, "isDefault": "true"}, null, "x"]
        }));
        assert_eq!(tokens.xcloud_regions.len(), 3);
        assert_eq!(tokens.xcloud_regions[0].name, "eastus");
        assert_eq!(tokens.xcloud_regions[0].base_uri, "");
        assert!(!tokens.xcloud_regions[0].is_default);
        assert_eq!(tokens.xcloud_regions[1], LoginRegion::default());
        assert_eq!(tokens.xcloud_regions[2], LoginRegion::default());
        assert_eq!(
            StreamTokens::from_json(&serde_json::json!({"xcloudRegions": "none"})),
            StreamTokens::default()
        );

        let region = LoginRegion::from_json(&serde_json::json!({"name": 5, "isDefault": true}));
        assert_eq!(region.name, "");
        assert!(region.is_default);
        let via_serde: StreamTokens =
            serde_json::from_str(r#"{"xcloudRegions":null,"webTokenUHS":null}"#).unwrap();
        assert_eq!(via_serde, StreamTokens::default());
    }

    #[test]
    fn preferred_region_honours_pin_then_default_then_first() {
        let regions = vec![
            region("westeurope", false),
            region("eastus", true),
            region("australiaeast", false),
        ];
        assert_eq!(
            preferred_region(&regions, "australiaeast").name,
            "australiaeast"
        );
        assert_eq!(preferred_region(&regions, "").name, "eastus");
        assert_eq!(preferred_region(&regions, "nowhere").name, "eastus");
        let no_default = vec![region("westeurope", false), region("eastus", false)];
        assert_eq!(preferred_region(&no_default, "").name, "westeurope");
        assert_eq!(preferred_region(&[], "eastus"), LoginRegion::default());
    }
}
