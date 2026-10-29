//! Port of qt/src/xcloud/XCloudModels.{h,cpp}
//! (plus the `CloudLibraryItem` shaping from qt/src/app/LibraryController.cpp:
//! `libraryItemToVariant`, the title-DTO projection and the catalog enrichment).
//!
//! DTOs and browse models for the xCloud/xHome HTTP surface. Every `from_json`
//! is a hand-written reader with `QJsonValue::toString()/toInt()/toBool()`
//! semantics: a missing, null or mistyped field reads as `""`/`0`/`false` and
//! parsing never fails, exactly like the C++. The `Deserialize` impls route
//! through the same readers so `serde_json::from_value` is equally tolerant;
//! `Serialize` stays derived for the wire spelling.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::library_filter;
use crate::preferences::{StreamKind, StreamPreferences, make_stream_settings};

// ---------------------------------------------------------------------------
// Tolerant JSON accessors (QJsonValue semantics: wrong type reads as empty/0/false).
// Shared with auth_types.rs.

pub(crate) fn str_at<'a>(object: &'a Value, key: &str) -> &'a str {
    object.get(key).and_then(Value::as_str).unwrap_or("")
}

pub(crate) fn string_at(object: &Value, key: &str) -> String {
    str_at(object, key).to_owned()
}

/// `value(key).toString(default)`: the string when the field is one, else `default`.
pub(crate) fn string_at_or(object: &Value, key: &str, default: &str) -> String {
    object
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_owned()
}

/// `value(a).toString(value(b).toString(...))`: the first key whose value *is a
/// string* wins, even when that string is empty; only an absent or non-string
/// value falls through to the next key.
fn string_fallback(object: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| object.get(key).and_then(Value::as_str))
        .unwrap_or("")
        .to_owned()
}

/// `value(key).toInt(default)`: whole numbers that fit an `int`; anything else
/// (missing, null, string, fractional or out of range) reads as `default`.
pub(crate) fn int_at_or(object: &Value, key: &str, default: i32) -> i32 {
    let Some(Value::Number(number)) = object.get(key) else {
        return default;
    };
    if let Some(value) = number.as_i64() {
        return i32::try_from(value).unwrap_or(default);
    }
    if let Some(value) = number.as_u64() {
        return i32::try_from(value).unwrap_or(default);
    }
    match number.as_f64() {
        Some(value)
            if value.fract() == 0.0
                && value >= f64::from(i32::MIN)
                && value <= f64::from(i32::MAX) =>
        {
            value as i32
        }
        _ => default,
    }
}

pub(crate) fn int_at(object: &Value, key: &str) -> i32 {
    int_at_or(object, key, 0)
}

pub(crate) fn bool_at(object: &Value, key: &str) -> bool {
    object.get(key).and_then(Value::as_bool).unwrap_or(false)
}

pub(crate) fn array_at<'a>(object: &'a Value, key: &str) -> &'a [Value] {
    object
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Implements `Deserialize` by reading a `serde_json::Value` through the type's
/// tolerant `from_json`, so serde callers get QJsonValue semantics too.
macro_rules! deserialize_via_from_json {
    ($($ty:ty),* $(,)?) => {$(
        impl<'de> ::serde::Deserialize<'de> for $ty {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> ::std::result::Result<Self, D::Error> {
                ::serde_json::Value::deserialize(deserializer).map(|value| Self::from_json(&value))
            }
        }
    )*};
}
pub(crate) use deserialize_via_from_json;

deserialize_via_from_json!(
    RemoteConsole,
    ConsoleListResponse,
    StreamSessionStartResponse,
    WaitTimeResponse,
    IceCandidatePayload,
);

/// `stringList`: non-empty strings of a JSON array.
fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Library models

/// Small display attribute attached to a library item (capability or genre).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LibraryAttribute {
    pub name: String,
    pub localized_name: String,
}

impl LibraryAttribute {
    pub fn new(name: impl Into<String>, localized_name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            localized_name: localized_name.into(),
        }
    }
}

/// `parseAttributes`: accepts both the catalog's PascalCase keys and the app's
/// camelCase keys; the localized name falls back to the raw name.
pub fn parse_attributes(value: Option<&Value>) -> Vec<LibraryAttribute> {
    let Some(array) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|entry| {
            let name = string_fallback(entry, &["Name", "name"]);
            let mut localized_name = string_fallback(entry, &["LocalizedName", "localizedName"]);
            if localized_name.is_empty() {
                localized_name = name.clone();
            }
            (!name.is_empty() || !localized_name.is_empty()).then_some(LibraryAttribute {
                name,
                localized_name,
            })
        })
        .collect()
}

/// App-facing library item built from catalog and title data for browse surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CloudLibraryItem {
    pub title_id: String,
    pub product_id: String,
    pub name: String,
    pub short_description: String,
    pub art_url: String,
    pub poster_image_url: String,
    pub hero_image_url: String,
    /// Key art with the game's logo baked in (`Image_TitledHero`), for featured cards.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub titled_hero_url: String,
    pub gallery_image_urls: Vec<String>,
    pub publisher_name: String,
    pub developer_name: String,
    pub release_date: String,
    /// The long store description (`ProductDescription`), for the game page.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Store genres (`LocalizedCategories`), e.g. "Action & adventure".
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    pub attributes: Vec<LibraryAttribute>,
    pub supported_input_types: Vec<String>,
    #[serde(rename = "isInMRU")]
    pub is_in_mru: bool,
}

/// `normalizeArtUrl`: protocol-relative catalog URLs become https.
pub fn normalize_art_url(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("//") {
        return format!("https://{rest}");
    }
    raw.to_owned()
}

/// The QML-facing card map (`libraryItemToVariant`) as a typed value. Art URLs
/// are normalised and `display_art_url` picks poster, then tile, then hero.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LibraryCard {
    pub title_id: String,
    pub product_id: String,
    pub name: String,
    pub short_description: String,
    pub art_url: String,
    pub poster_image_url: String,
    pub hero_image_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub titled_hero_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    pub gallery_image_urls: Vec<String>,
    pub publisher_name: String,
    pub developer_name: String,
    pub release_date: String,
    #[serde(rename = "isInMRU")]
    pub is_in_mru: bool,
    pub attributes: Vec<LibraryAttribute>,
    pub supported_input_types: Vec<String>,
    pub max_players: u32,
    pub display_art_url: String,
}

impl CloudLibraryItem {
    /// Projection of a raw title DTO before catalog enrichment
    /// (LibraryController::refresh): the name falls back to the product id.
    pub fn from_title_dto(dto: &XCloudTitleDTO, is_in_mru: bool) -> Self {
        Self {
            title_id: dto.title_id.clone(),
            product_id: dto.product_id.clone(),
            name: if dto.name.is_empty() {
                dto.product_id.clone()
            } else {
                dto.name.clone()
            },
            supported_input_types: dto.supported_input_types.clone(),
            is_in_mru,
            ..Default::default()
        }
    }

    /// Catalog enrichment (LibraryController::applyCatalog): copies the product's
    /// display fields onto the item. The tile image becomes `art_url`, falling
    /// back to the poster.
    pub fn apply_catalog_product(&mut self, product: &CatalogProduct) {
        if !product.product_title.is_empty() {
            self.name = product.product_title.clone();
        }
        self.short_description = product.product_description_short.clone();
        self.publisher_name = product.publisher_name.clone();
        self.developer_name = product.developer_name.clone();
        self.release_date = product.original_release_date.clone();
        self.attributes = product.attributes.clone();
        self.art_url = if product.image_tile.url.is_empty() {
            product.image_poster.url.clone()
        } else {
            product.image_tile.url.clone()
        };
        self.poster_image_url = product.image_poster.url.clone();
        self.hero_image_url = product.image_hero.url.clone();
        self.titled_hero_url = product.image_titled_hero.url.clone();
        self.description = product.product_description.clone();
        self.categories = product.categories.clone();
        self.gallery_image_urls = product.gallery_image_urls.clone();
    }

    /// `libraryItemToVariant`: the card shape consumed by the browse views.
    pub fn to_card(&self) -> LibraryCard {
        let display_source = if !self.poster_image_url.is_empty() {
            &self.poster_image_url
        } else if !self.art_url.is_empty() {
            &self.art_url
        } else {
            &self.hero_image_url
        };
        LibraryCard {
            title_id: self.title_id.clone(),
            product_id: self.product_id.clone(),
            name: self.name.clone(),
            short_description: self.short_description.clone(),
            art_url: normalize_art_url(&self.art_url),
            poster_image_url: normalize_art_url(&self.poster_image_url),
            hero_image_url: normalize_art_url(&self.hero_image_url),
            titled_hero_url: normalize_art_url(&self.titled_hero_url),
            description: self.description.clone(),
            categories: self.categories.clone(),
            gallery_image_urls: self
                .gallery_image_urls
                .iter()
                .map(|url| normalize_art_url(url))
                .collect(),
            publisher_name: self.publisher_name.clone(),
            developer_name: self.developer_name.clone(),
            release_date: self.release_date.clone(),
            is_in_mru: self.is_in_mru,
            attributes: self.attributes.clone(),
            supported_input_types: self.supported_input_types.clone(),
            max_players: library_filter::max_players_for(&self.attributes),
            display_art_url: normalize_art_url(display_source),
        }
    }

    /// `to_card()` as a JSON object, the direct `QVariantMap` equivalent.
    pub fn to_card_json(&self) -> Value {
        serde_json::to_value(self.to_card()).unwrap_or(Value::Null)
    }
}

/// Raw title DTO from the xCloud library endpoint before projection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct XCloudTitleDTO {
    pub title_id: String,
    pub product_id: String,
    pub name: String,
    pub has_entitlement: bool,
    pub supported_input_types: Vec<String>,
}

impl XCloudTitleDTO {
    /// `{"titleId": ..., "details": {"productId", "name", "hasEntitlement", "supportedInputTypes"}}`.
    pub fn from_json(object: &Value) -> Self {
        let details = object.get("details").unwrap_or(&Value::Null);
        Self {
            title_id: string_at(object, "titleId"),
            product_id: string_at(details, "productId"),
            name: string_at(details, "name"),
            has_entitlement: bool_at(details, "hasEntitlement"),
            supported_input_types: string_list(details.get("supportedInputTypes")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct XCloudTitlesResponse {
    pub results: Vec<XCloudTitleDTO>,
}

impl XCloudTitlesResponse {
    pub fn from_json(object: &Value) -> Self {
        Self {
            results: array_at(object, "results")
                .iter()
                .map(XCloudTitleDTO::from_json)
                .collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Home streaming / session models

/// Remote console inventory entry used by home streaming surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteConsole {
    pub device_name: String,
    pub server_id: String,
    pub power_state: String,
    pub console_type: String,
    pub play_path: String,
    pub out_of_home_warning: bool,
    pub wireless_warning: bool,
    pub is_dev_kit: bool,
}

impl RemoteConsole {
    /// `RemoteConsole::fromJson`: missing, null or mistyped fields read as empty/false.
    pub fn from_json(object: &Value) -> Self {
        Self {
            device_name: string_at(object, "deviceName"),
            server_id: string_at(object, "serverId"),
            power_state: string_at(object, "powerState"),
            console_type: string_at(object, "consoleType"),
            play_path: string_at(object, "playPath"),
            out_of_home_warning: bool_at(object, "outOfHomeWarning"),
            wireless_warning: bool_at(object, "wirelessWarning"),
            is_dev_kit: bool_at(object, "isDevKit"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleListResponse {
    pub total_items: i32,
    pub results: Vec<RemoteConsole>,
    pub continuation_token: String,
}

impl ConsoleListResponse {
    /// `ConsoleListResponse::fromJson`: a null `continuationToken` or a
    /// non-numeric `totalItems` reads as `""`/`0`, never an error.
    pub fn from_json(object: &Value) -> Self {
        Self {
            total_items: int_at(object, "totalItems"),
            results: array_at(object, "results")
                .iter()
                .map(RemoteConsole::from_json)
                .collect(),
            continuation_token: string_at(object, "continuationToken"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamSessionStartResponse {
    pub session_path: String,
    pub session_id: String,
    pub state: String,
}

impl StreamSessionStartResponse {
    /// `StreamSessionStartResponse::fromJson`.
    pub fn from_json(object: &Value) -> Self {
        Self {
            session_path: string_at(object, "sessionPath"),
            session_id: string_at(object, "sessionId"),
            state: string_at(object, "state"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamStateResponse {
    pub state: String,
    pub error_code: String,
    pub error_message: String,
}

impl StreamStateResponse {
    pub fn has_error(&self) -> bool {
        !self.error_code.is_empty() || !self.error_message.is_empty()
    }

    /// `{"state": ..., "errorDetails": {"code", "message"}}`.
    pub fn from_json(object: &Value) -> Self {
        let error = object.get("errorDetails").unwrap_or(&Value::Null);
        Self {
            state: string_at(object, "state"),
            error_code: string_at(error, "code"),
            error_message: string_at(error, "message"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitTimeResponse {
    pub estimated_total_wait_time_in_seconds: i32,
}

impl WaitTimeResponse {
    /// `WaitTimeResponse::fromJson`: `toInt()` semantics, so `12.0` reads as 12
    /// and a string or fractional value as 0.
    pub fn from_json(object: &Value) -> Self {
        Self {
            estimated_total_wait_time_in_seconds: int_at(object, "estimatedTotalWaitTimeInSeconds"),
        }
    }
}

/// One ICE candidate as exchanged with the `/ice` endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct IceCandidatePayload {
    pub candidate: String,
    #[serde(rename = "sdpMLineIndex")]
    pub sdp_mline_index: i32,
    #[serde(rename = "sdpMid")]
    pub sdp_mid: String,
    #[serde(rename = "usernameFragment", skip_serializing_if = "String::is_empty")]
    pub username_fragment: String,
}

impl Default for IceCandidatePayload {
    fn default() -> Self {
        Self {
            candidate: String::new(),
            sdp_mline_index: 0,
            sdp_mid: "0".to_owned(),
            username_fragment: String::new(),
        }
    }
}

impl IceCandidatePayload {
    pub fn new(candidate: impl Into<String>) -> Self {
        Self {
            candidate: candidate.into(),
            ..Default::default()
        }
    }

    /// `toJson`: `usernameFragment` is omitted when empty.
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// Field reader mirroring `decodeCandidate` in XCloudApiClient.cpp: a
    /// missing or null `sdpMid` keeps the struct default `"0"`
    /// (`toString(QStringLiteral("0"))`), `sdpMLineIndex` accepts a number or a
    /// numeric string and otherwise reads as 0. Validation of the candidate line
    /// itself (end-of-candidates, `a=` prefix) stays with the API client.
    pub fn from_json(object: &Value) -> Self {
        let sdp_mline_index = match object.get("sdpMLineIndex") {
            Some(Value::String(text)) => text.trim().parse().unwrap_or(0),
            _ => int_at(object, "sdpMLineIndex"),
        };
        Self {
            candidate: string_at(object, "candidate"),
            sdp_mline_index,
            sdp_mid: string_at_or(object, "sdpMid", "0"),
            username_fragment: string_at(object, "usernameFragment"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StreamDimensions {
    pub width: u32,
    pub height: u32,
}

impl Default for StreamDimensions {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
        }
    }
}

// ---------------------------------------------------------------------------
// Game Pass catalog

/// One image slot of a catalog product.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CatalogImage {
    pub url: String,
    pub width: i32,
    pub height: i32,
}

/// `parseImage`: the catalog spells the URL as `URL`, `Url` or `Uri`. The
/// first spelling present as a string wins, even when it is empty.
pub fn parse_catalog_image(value: Option<&Value>) -> CatalogImage {
    let Some(object) = value else {
        return CatalogImage::default();
    };
    CatalogImage {
        url: string_fallback(object, &["URL", "Url", "Uri"]),
        width: int_at(object, "Width"),
        height: int_at(object, "Height"),
    }
}

/// One Game Pass catalog product with the media fields the UI consumes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CatalogProduct {
    pub product_id: String,
    pub store_id: String,
    pub xcloud_title_id: String,
    pub xbox_title_id: String,
    pub product_title: String,
    pub publisher_name: String,
    pub developer_name: String,
    pub original_release_date: String,
    pub product_description_short: String,
    /// `ProductDescription`: the long store description.
    pub product_description: String,
    pub image_tile: CatalogImage,
    pub image_poster: CatalogImage,
    pub image_hero: CatalogImage,
    /// `Image_TitledHero`: hero art with the logo baked in.
    pub image_titled_hero: CatalogImage,
    /// `LocalizedCategories` (falling back to `Categories`): the store genres.
    pub categories: Vec<String>,
    pub screenshots: Vec<CatalogImage>,
    pub attributes: Vec<LibraryAttribute>,
    pub gallery_image_urls: Vec<String>,
}

impl CatalogProduct {
    /// Parses one `Products[<id>]` entry. Top-level fields win; the first
    /// `LocalizedProperties` entry fills the title, description and gallery gaps.
    pub fn from_json(object: &Value) -> Self {
        let mut product = Self {
            product_id: string_fallback(object, &["ProductId", "StoreId"]),
            store_id: string_at(object, "StoreId"),
            xcloud_title_id: string_at(object, "XCloudTitleId"),
            xbox_title_id: string_at(object, "XboxTitleId"),
            product_title: string_at(object, "ProductTitle"),
            publisher_name: string_at(object, "PublisherName"),
            developer_name: string_at(object, "DeveloperName"),
            original_release_date: string_at(object, "OriginalReleaseDate"),
            product_description_short: string_at(object, "ProductDescriptionShort"),
            product_description: string_at(object, "ProductDescription"),
            image_tile: parse_catalog_image(object.get("Image_Tile")),
            image_poster: parse_catalog_image(object.get("Image_Poster")),
            image_hero: parse_catalog_image(object.get("Image_Hero")),
            image_titled_hero: parse_catalog_image(object.get("Image_TitledHero")),
            categories: {
                let clean = |key: &str| -> Vec<String> {
                    string_list(object.get(key))
                        .into_iter()
                        .map(|c| c.trim().to_owned())
                        .filter(|c| !c.is_empty())
                        .collect()
                };
                let localized = clean("LocalizedCategories");
                if localized.is_empty() {
                    clean("Categories")
                } else {
                    localized
                }
            },
            attributes: parse_attributes(object.get("Attributes")),
            ..Default::default()
        };
        for value in array_at(object, "Screenshots") {
            let image = parse_catalog_image(Some(value));
            if !image.url.is_empty() {
                product.gallery_image_urls.push(image.url.clone());
                product.screenshots.push(image);
            }
        }

        // Localized overrides fill any gaps.
        let localized = match object.get("LocalizedProperties") {
            Some(Value::Array(array)) => array.first().cloned().unwrap_or(Value::Null),
            Some(value @ Value::Object(_)) => value.clone(),
            _ => Value::Null,
        };
        if product.product_title.is_empty() {
            product.product_title = string_at(&localized, "ProductTitle");
        }
        if product.product_description_short.is_empty() {
            product.product_description_short = string_at(&localized, "ShortDescription");
        }
        if product.product_description.is_empty() {
            product.product_description = string_at(&localized, "ProductDescription");
        }
        if product.gallery_image_urls.is_empty() {
            let images: Vec<&Value> = match localized.get("Images") {
                Some(Value::Array(array)) => array.iter().collect(),
                Some(Value::Object(map)) => map.values().collect(),
                _ => Vec::new(),
            };
            for image in images {
                let uri = string_fallback(image, &["Uri", "URL"]);
                if !uri.is_empty() {
                    product.gallery_image_urls.push(uri);
                }
            }
        }
        product
    }
}

// ---------------------------------------------------------------------------
// Request bodies

/// `makeStartStreamBody`: builds the `/v5/sessions/<kind>/play` request body.
pub fn make_start_stream_body(
    kind: StreamKind,
    target_id: &str,
    preferences: &StreamPreferences,
) -> Value {
    let (title_id, server_id) = match kind {
        StreamKind::Cloud => (target_id, ""),
        StreamKind::Home => ("", target_id),
    };
    json!({
        "clientSessionId": "",
        "titleId": title_id,
        "systemUpdateGroup": "",
        "settings": make_stream_settings(preferences),
        "serverId": server_id,
        "fallbackRegionNames": preferences.fallback_region_names,
    })
}

/// `makeSdpOfferBody`: builds the `/sdp` request body including the channel version ranges.
pub fn make_sdp_offer_body(sdp: &str) -> Value {
    let version_range = |min: u32, max: u32| json!({ "minVersion": min, "maxVersion": max });

    let configuration = json!({
        "chatConfiguration": {
            "bytesPerSample": 2,
            "expectedClipDurationMs": 20,
            "format": { "codec": "opus", "container": "webm" },
            "numChannels": 1,
            "sampleFrequencyHz": 24000,
        },
        "chat": version_range(1, 1),
        "control": version_range(1, 3),
        "input": version_range(1, 9),
        "message": version_range(1, 1),
        "reliableinput": version_range(9, 9),
        "unreliableinput": version_range(9, 9),
    });

    json!({
        "messageType": "offer",
        "sdp": sdp,
        "requestId": "1",
        "configuration": configuration,
    })
}

// ---------------------------------------------------------------------------
// Outbound ICE candidate handling

/// `isLiteralIPv4`: four dotted decimal octets, each 0..=255.
fn is_literal_ipv4(address: &str) -> bool {
    let octets: Vec<&str> = address.split('.').collect();
    octets.len() == 4 && octets.iter().all(|octet| octet.parse::<u8>().is_ok())
}

/// Space-separated candidate fields, skipping empty parts.
fn candidate_parts(candidate: &str) -> Vec<&str> {
    candidate
        .split(' ')
        .filter(|part| !part.is_empty())
        .collect()
}

/// `iceSortScore`: UDP first (preferred address family, then the other, then
/// unknown), TCP after, unparsable last.
fn ice_sort_score(candidate: &str, prefer_ipv6: bool) -> u8 {
    let parts = candidate_parts(candidate);
    if parts.len() < 8 {
        return 100;
    }
    let is_udp = parts[2].eq_ignore_ascii_case("udp");
    let address = parts[4];
    let ipv4 = is_literal_ipv4(address);
    let ipv6 = address.contains(':');

    if !is_udp {
        return 3;
    }
    match (prefer_ipv6, ipv6, ipv4) {
        (true, true, _) | (false, _, true) => 0,
        (true, _, true) | (false, true, _) => 1,
        _ => 2,
    }
}

/// `normalizeOutboundIceCandidate`: strips a leading `a=` and surrounding
/// whitespace. Returns `None` when the line should be dropped (empty,
/// `end-of-candidates`, or not a `candidate:` line).
pub fn normalize_outbound_ice_candidate(candidate: &str) -> Option<String> {
    let mut normalized = candidate.trim();
    if let Some(rest) = normalized.strip_prefix("a=") {
        normalized = rest;
    }
    if normalized.is_empty() || normalized == "end-of-candidates" {
        return None;
    }
    normalized
        .starts_with("candidate:")
        .then(|| normalized.to_owned())
}

/// `isPreferredOutboundIceCandidate`: a UDP candidate whose address matches the
/// preferred family.
pub fn is_preferred_outbound_ice_candidate(candidate: &str, prefer_ipv6: bool) -> bool {
    let parts = candidate_parts(candidate);
    if parts.len() < 8 || !parts[2].eq_ignore_ascii_case("udp") {
        return false;
    }
    let address = parts[4];
    if prefer_ipv6 {
        address.contains(':')
    } else {
        is_literal_ipv4(address)
    }
}

/// `dedupeIceCandidates`: keeps the first of each `(sdpMid, sdpMLineIndex, candidate)`.
pub fn dedupe_ice_candidates(candidates: &[IceCandidatePayload]) -> Vec<IceCandidatePayload> {
    let mut seen = HashSet::new();
    candidates
        .iter()
        .filter(|candidate| {
            seen.insert((
                candidate.sdp_mid.as_str(),
                candidate.sdp_mline_index,
                candidate.candidate.as_str(),
            ))
        })
        .cloned()
        .collect()
}

/// `sortIceCandidates`: stable sort by [`ice_sort_score`].
pub fn sort_ice_candidates(
    candidates: &[IceCandidatePayload],
    prefer_ipv6: bool,
) -> Vec<IceCandidatePayload> {
    let mut sorted = candidates.to_vec();
    sorted.sort_by_key(|candidate| ice_sort_score(&candidate.candidate, prefer_ipv6));
    sorted
}

/// Convenience for building a JSON object literal from pairs in code that
/// mirrors `QJsonObject{{k, v}, ...}`.
pub fn json_object<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    let mut map = Map::with_capacity(N);
    for (key, value) in pairs {
        map.insert(key.to_owned(), value);
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    //! Model-level cases from qt/tests/tst_xcloudapi.cpp (sdpOfferBodyShape,
    //! iceCandidateNormalization, the `/play` body assertions) and the item shaping
    //! asserted by qt/tests/tst_librarycontroller.cpp.
    use super::*;
    use crate::preferences::StreamResolutionMode;

    #[test]
    fn title_dto_from_nested_details() {
        let response: Value = serde_json::from_str(
            r#"{"results":[
              {"titleId":"T1","details":{"productId":"P1","hasEntitlement":true,"supportedInputTypes":["Controller"]}},
              {"titleId":"T2","details":{"productId":"P2","name":"Beta","supportedInputTypes":["Touch"]}}
            ]}"#,
        )
        .unwrap();
        let titles = XCloudTitlesResponse::from_json(&response);
        assert_eq!(titles.results.len(), 2);
        let first = &titles.results[0];
        assert_eq!(first.title_id, "T1");
        assert_eq!(first.product_id, "P1");
        assert!(first.has_entitlement);
        assert_eq!(first.supported_input_types, vec!["Controller"]);
        assert_eq!(first.name, "");
        let second = &titles.results[1];
        assert_eq!(second.name, "Beta");
        assert!(!second.has_entitlement);

        // Missing details object reads as empty, like QJsonValue::toObject().
        let bare = XCloudTitleDTO::from_json(&json!({"titleId": "T3"}));
        assert_eq!(bare.title_id, "T3");
        assert_eq!(bare.product_id, "");
        assert!(bare.supported_input_types.is_empty());
    }

    #[test]
    fn catalog_product_parsing_and_enrichment() {
        let products: Value = serde_json::from_str(
            r#"{"Products":{
              "P1":{"ProductId":"P1","XCloudTitleId":"T1","ProductTitle":"Alpha Deluxe","PublisherName":"Pub",
                    "ProductDescriptionShort":"desc","Image_Poster":{"URL":"https://img.example/p1.png"},
                    "Attributes":[{"Name":"capability","LocalizedName":"Online co-op (2-12)"}]},
              "P2":{"ProductId":"P2","XCloudTitleId":"T2","ProductTitle":"Beta",
                    "Image_Hero":{"URL":"//img.example/p2.png"}}
            }}"#,
        )
        .unwrap();
        let p1 = CatalogProduct::from_json(&products["Products"]["P1"]);
        assert_eq!(p1.product_id, "P1");
        assert_eq!(p1.xcloud_title_id, "T1");
        assert_eq!(p1.product_title, "Alpha Deluxe");
        assert_eq!(p1.publisher_name, "Pub");
        assert_eq!(p1.image_poster.url, "https://img.example/p1.png");
        assert_eq!(
            p1.attributes,
            vec![LibraryAttribute::new("capability", "Online co-op (2-12)")]
        );

        // Shape the item the way LibraryController does, then check the card.
        let dto = XCloudTitleDTO::from_json(&json!({
            "titleId":"T1","details":{"productId":"P1","hasEntitlement":true,"supportedInputTypes":["Controller"]}
        }));
        let mut item = CloudLibraryItem::from_title_dto(&dto, true);
        assert_eq!(item.name, "P1");
        item.apply_catalog_product(&p1);
        let card = item.to_card();
        assert_eq!(card.name, "Alpha Deluxe");
        assert_eq!(card.publisher_name, "Pub");
        assert_eq!(card.poster_image_url, "https://img.example/p1.png");
        assert_eq!(card.max_players, 12);
        assert!(card.is_in_mru);
        // Tile is empty, so artUrl falls back to the poster.
        assert_eq!(card.art_url, "https://img.example/p1.png");
        assert_eq!(card.display_art_url, "https://img.example/p1.png");
        let json = item.to_card_json();
        assert_eq!(json["maxPlayers"], 12);
        assert_eq!(json["isInMRU"], true);
        assert_eq!(
            json["attributes"][0]["localizedName"],
            "Online co-op (2-12)"
        );

        let p2 = CatalogProduct::from_json(&products["Products"]["P2"]);
        let mut second = CloudLibraryItem::from_title_dto(
            &XCloudTitleDTO::from_json(
                &json!({"titleId":"T2","details":{"productId":"P2","name":"Beta"}}),
            ),
            false,
        );
        second.apply_catalog_product(&p2);
        let card = second.to_card();
        assert_eq!(card.hero_image_url, "https://img.example/p2.png");
        assert_eq!(card.display_art_url, "https://img.example/p2.png");
        assert_eq!(card.max_players, 1);
    }

    #[test]
    fn catalog_localized_fallbacks() {
        let product = CatalogProduct::from_json(&json!({
            "StoreId": "S1",
            "Screenshots": [{"Uri": "https://a"}, {"Url": ""}, {"URL": "https://b", "Width": 1920, "Height": 1080}],
            "LocalizedProperties": [{"ProductTitle": "Local", "ShortDescription": "short"}],
            "Attributes": [{"name": "genre"}, {}]
        }));
        assert_eq!(product.product_id, "S1");
        assert_eq!(product.store_id, "S1");
        assert_eq!(product.product_title, "Local");
        assert_eq!(product.product_description_short, "short");
        assert_eq!(product.gallery_image_urls, vec!["https://a", "https://b"]);
        assert_eq!(product.screenshots.len(), 2);
        assert_eq!(product.screenshots[1].width, 1920);
        assert_eq!(
            product.attributes,
            vec![LibraryAttribute::new("genre", "genre")]
        );

        let from_map = CatalogProduct::from_json(&json!({
            "ProductId": "P",
            "LocalizedProperties": {"Images": {"a": {"Uri": "https://x"}, "b": {"URL": "https://y"}, "c": {}}}
        }));
        let mut urls = from_map.gallery_image_urls.clone();
        urls.sort();
        assert_eq!(urls, vec!["https://x", "https://y"]);
    }

    #[test]
    fn string_fallback_only_skips_absent_or_non_string_keys() {
        // value(a).toString(value(b).toString()): an `a` present as "" wins
        // and yields "", exactly like the C++. Only a missing, null or
        // mistyped `a` falls through to `b`.
        let product = CatalogProduct::from_json(&json!({"ProductId": "", "StoreId": "S1"}));
        assert_eq!(product.product_id, "");
        assert_eq!(product.store_id, "S1");
        let product = CatalogProduct::from_json(&json!({"ProductId": null, "StoreId": "S1"}));
        assert_eq!(product.product_id, "S1");
        let product = CatalogProduct::from_json(&json!({"ProductId": 7, "StoreId": "S1"}));
        assert_eq!(product.product_id, "S1");

        let image = parse_catalog_image(Some(&json!({"URL": "", "Uri": "u"})));
        assert_eq!(image.url, "");
        let image = parse_catalog_image(Some(&json!({"URL": null, "Url": "", "Uri": "u"})));
        assert_eq!(image.url, "");
        let image = parse_catalog_image(Some(&json!({"Url": 1, "Uri": "u"})));
        assert_eq!(image.url, "u");
        // A `URL: ""` screenshot is dropped even when `Uri` is set.
        let product = CatalogProduct::from_json(&json!({
            "Screenshots": [{"URL": "", "Uri": "https://a"}, {"Uri": "https://b"}]
        }));
        assert_eq!(product.gallery_image_urls, vec!["https://b"]);

        // Both names present but empty: the attribute is dropped, like the C++.
        assert!(parse_attributes(Some(&json!([{"Name": "", "name": "n"}]))).is_empty());
        assert_eq!(
            parse_attributes(Some(
                &json!([{"Name": "n", "LocalizedName": "", "localizedName": "loc"}])
            )),
            vec![LibraryAttribute::new("n", "n")]
        );
        assert_eq!(
            parse_attributes(Some(
                &json!([{"Name": null, "name": "n", "localizedName": "loc"}])
            )),
            vec![LibraryAttribute::new("n", "loc")]
        );

        // Localized Images: `Uri: ""` does not fall back to `URL`.
        let product = CatalogProduct::from_json(&json!({
            "LocalizedProperties": [{"Images": [{"Uri": "", "URL": "https://y"}, {"URL": "https://z"}]}]
        }));
        assert_eq!(product.gallery_image_urls, vec!["https://z"]);
    }

    #[test]
    fn flat_responses_parse() {
        let consoles = ConsoleListResponse::from_json(&json!({
            "totalItems": 1,
            "continuationToken": "",
            "results": [{"deviceName": "Living room", "serverId": "S", "powerState": "On",
                         "consoleType": "XboxSeriesX", "playPath": "/p", "outOfHomeWarning": true}]
        }));
        assert_eq!(consoles.total_items, 1);
        assert_eq!(consoles.results[0].device_name, "Living room");
        assert!(consoles.results[0].out_of_home_warning);
        assert!(!consoles.results[0].is_dev_kit);

        let start = StreamSessionStartResponse::from_json(&json!({
            "sessionPath": "/v5/sessions/cloud/ABC", "sessionId": "ABC", "state": "Provisioning"
        }));
        assert_eq!(start.session_path, "/v5/sessions/cloud/ABC");

        let state = StreamStateResponse::from_json(&json!({"state": "Provisioned"}));
        assert!(!state.has_error());
        let failed = StreamStateResponse::from_json(&json!({
            "state": "Failed", "errorDetails": {"code": "NoResources", "message": "busy"}
        }));
        assert!(failed.has_error());
        assert_eq!(failed.error_code, "NoResources");
        assert_eq!(failed.error_message, "busy");

        let wait = WaitTimeResponse::from_json(&json!({"estimatedTotalWaitTimeInSeconds": 42}));
        assert_eq!(wait.estimated_total_wait_time_in_seconds, 42);
        assert_eq!(
            WaitTimeResponse::from_json(&json!({})),
            WaitTimeResponse::default()
        );
        assert_eq!(
            StreamDimensions::default(),
            StreamDimensions {
                width: 1920,
                height: 1080
            }
        );
    }

    #[test]
    fn flat_responses_tolerate_null_and_mistyped_fields() {
        // QJsonValue::toString()/toInt()/toBool() never fail: null or wrongly
        // typed values read as ""/0/false. The C++ fromJson cannot reject.
        let consoles = ConsoleListResponse::from_json(&json!({
            "totalItems": 1, "continuationToken": null, "results": []
        }));
        assert_eq!(consoles.total_items, 1);
        assert_eq!(consoles.continuation_token, "");
        assert!(consoles.results.is_empty());
        assert_eq!(
            ConsoleListResponse::from_json(&json!({"totalItems": "1"})).total_items,
            0
        );
        assert_eq!(
            ConsoleListResponse::from_json(&json!({"totalItems": 2.0, "results": "nope"})),
            ConsoleListResponse {
                total_items: 2,
                ..Default::default()
            }
        );
        // toInt(): fractional and out-of-range doubles read as the default.
        assert_eq!(
            ConsoleListResponse::from_json(&json!({"totalItems": 1.5})).total_items,
            0
        );
        assert_eq!(
            ConsoleListResponse::from_json(&json!({"totalItems": 4294967296i64})).total_items,
            0
        );

        let console = RemoteConsole::from_json(&json!({
            "deviceName": null, "serverId": 5, "outOfHomeWarning": "true", "isDevKit": 1
        }));
        assert_eq!(console, RemoteConsole::default());
        assert!(!console.out_of_home_warning);
        assert!(!console.is_dev_kit);
        // Non-object entries in `results` read as empty consoles, like toObject().
        let consoles = ConsoleListResponse::from_json(&json!({"results": [null, "x", {}]}));
        assert_eq!(consoles.results.len(), 3);
        assert!(
            consoles
                .results
                .iter()
                .all(|c| *c == RemoteConsole::default())
        );

        let start = StreamSessionStartResponse::from_json(&json!({
            "sessionPath": null, "sessionId": 12, "state": ["Provisioning"]
        }));
        assert_eq!(start, StreamSessionStartResponse::default());

        assert_eq!(
            WaitTimeResponse::from_json(&json!({"estimatedTotalWaitTimeInSeconds": 12.0}))
                .estimated_total_wait_time_in_seconds,
            12
        );
        assert_eq!(
            WaitTimeResponse::from_json(&json!({"estimatedTotalWaitTimeInSeconds": "12"}))
                .estimated_total_wait_time_in_seconds,
            0
        );
        assert_eq!(
            WaitTimeResponse::from_json(&json!({"estimatedTotalWaitTimeInSeconds": null}))
                .estimated_total_wait_time_in_seconds,
            0
        );

        let ice = IceCandidatePayload::from_json(&json!({
            "candidate": "candidate:1 1 UDP 1 1.2.3.4 9 typ host", "sdpMLineIndex": 0, "sdpMid": null
        }));
        assert_eq!(ice.sdp_mid, "0");
        assert_eq!(ice.candidate, "candidate:1 1 UDP 1 1.2.3.4 9 typ host");
        let ice = IceCandidatePayload::from_json(&json!({
            "candidate": "candidate:1 1 UDP 1 1.2.3.4 9 typ host", "sdpMLineIndex": "1",
            "sdpMid": "", "usernameFragment": null
        }));
        assert_eq!(ice.sdp_mline_index, 1);
        assert_eq!(ice.sdp_mid, "");
        assert_eq!(ice.username_fragment, "");
        let ice = IceCandidatePayload::from_json(&json!({"sdpMLineIndex": "x", "candidate": 3}));
        assert_eq!(ice, IceCandidatePayload::default());

        // The serde path shares the tolerant readers.
        let via_serde: ConsoleListResponse =
            serde_json::from_str(r#"{"totalItems":"1","continuationToken":null,"results":[]}"#)
                .unwrap();
        assert_eq!(via_serde, ConsoleListResponse::default());
        let via_serde: IceCandidatePayload =
            serde_json::from_str(r#"{"candidate":"c","sdpMLineIndex":null,"sdpMid":null}"#)
                .unwrap();
        assert_eq!(via_serde, IceCandidatePayload::new("c"));
    }

    #[test]
    fn ice_candidate_payload_json() {
        let candidate = IceCandidatePayload::new("candidate:1 1 UDP 1 1.2.3.4 9 typ host");
        let json = candidate.to_json();
        assert_eq!(json["candidate"], "candidate:1 1 UDP 1 1.2.3.4 9 typ host");
        assert_eq!(json["sdpMLineIndex"], 0);
        assert_eq!(json["sdpMid"], "0");
        assert!(json.get("usernameFragment").is_none());

        let with_ufrag = IceCandidatePayload {
            username_fragment: "abcd".to_owned(),
            ..candidate.clone()
        };
        assert_eq!(with_ufrag.to_json()["usernameFragment"], "abcd");
        assert_eq!(IceCandidatePayload::from_json(&json), candidate);
    }

    #[test]
    fn start_stream_body_shape() {
        let preferences = StreamPreferences {
            resolution: StreamResolutionMode::P720,
            fallback_region_names: vec!["eastus".to_owned()],
            ..Default::default()
        };
        let body = make_start_stream_body(StreamKind::Cloud, "TITLE", &preferences);
        assert_eq!(body["titleId"], "TITLE");
        assert_eq!(body["serverId"], "");
        assert_eq!(body["clientSessionId"], "");
        assert_eq!(body["systemUpdateGroup"], "");
        assert_eq!(body["settings"]["osName"], "android");
        assert_eq!(body["settings"]["nanoVersion"], "V3;WebrtcTransport.dll");
        assert_eq!(body["fallbackRegionNames"][0], "eastus");

        let home = make_start_stream_body(StreamKind::Home, "SERVER", &preferences);
        assert_eq!(home["titleId"], "");
        assert_eq!(home["serverId"], "SERVER");
    }

    #[test]
    fn sdp_offer_body_shape() {
        let body = make_sdp_offer_body("v=0");
        assert_eq!(body["messageType"], "offer");
        assert_eq!(body["sdp"], "v=0");
        assert_eq!(body["requestId"], "1");
        let configuration = &body["configuration"];
        assert_eq!(configuration["input"]["maxVersion"], 9);
        assert_eq!(configuration["reliableinput"]["minVersion"], 9);
        assert_eq!(configuration["unreliableinput"]["maxVersion"], 9);
        assert_eq!(configuration["control"]["maxVersion"], 3);
        let chat = &configuration["chatConfiguration"];
        assert_eq!(chat["sampleFrequencyHz"], 24000);
        assert_eq!(chat["format"]["codec"], "opus");
        assert_eq!(chat["format"]["container"], "webm");
    }

    #[test]
    fn ice_candidate_normalization() {
        assert_eq!(
            normalize_outbound_ice_candidate("a=candidate:1 1 UDP 1 1.2.3.4 9 typ host").as_deref(),
            Some("candidate:1 1 UDP 1 1.2.3.4 9 typ host")
        );
        assert_eq!(normalize_outbound_ice_candidate("end-of-candidates"), None);
        assert_eq!(normalize_outbound_ice_candidate("not-a-candidate"), None);
        assert_eq!(
            normalize_outbound_ice_candidate("  a=end-of-candidates\r\n"),
            None
        );
        assert_eq!(normalize_outbound_ice_candidate(""), None);

        assert!(is_preferred_outbound_ice_candidate(
            "candidate:1 1 UDP 1 1.2.3.4 9 typ host",
            false
        ));
        assert!(!is_preferred_outbound_ice_candidate(
            "candidate:1 1 UDP 1 2001:db8::1 9 typ host",
            false
        ));
        assert!(is_preferred_outbound_ice_candidate(
            "candidate:1 1 UDP 1 2001:db8::1 9 typ host",
            true
        ));
        assert!(!is_preferred_outbound_ice_candidate(
            "candidate:1 1 TCP 1 1.2.3.4 9 typ host",
            false
        ));
        assert!(!is_preferred_outbound_ice_candidate(
            "candidate:1 1 UDP 1 1.2.3.4",
            false
        ));

        let first = IceCandidatePayload::new("candidate:1 1 UDP 1 1.2.3.4 9 typ host");
        let duplicate = first.clone();
        let second = IceCandidatePayload::new("candidate:2 1 UDP 1 5.6.7.8 9 typ host");
        let deduped = dedupe_ice_candidates(&[first.clone(), duplicate, second]);
        assert_eq!(deduped.len(), 2);

        let tcp_candidate = IceCandidatePayload::new("candidate:3 1 TCP 1 9.9.9.9 9 typ host");
        let sorted = sort_ice_candidates(&[tcp_candidate, first], false);
        assert_eq!(sorted.len(), 2);
        // UDP candidates sort ahead of TCP ones.
        assert!(sorted[0].candidate.contains("1.2.3.4"));
    }

    #[test]
    fn ice_sort_prefers_address_family_and_is_stable() {
        let v4 = IceCandidatePayload::new("candidate:1 1 udp 1 1.2.3.4 9 typ host");
        let v6 = IceCandidatePayload::new("candidate:2 1 udp 1 2001:db8::1 9 typ host");
        let host = IceCandidatePayload::new("candidate:3 1 udp 1 example.local 9 typ host");
        let tcp = IceCandidatePayload::new("candidate:4 1 tcp 1 1.2.3.4 9 typ host");
        let junk = IceCandidatePayload::new("candidate:5");
        let input = [
            junk.clone(),
            tcp.clone(),
            host.clone(),
            v6.clone(),
            v4.clone(),
        ];

        let v4_first = sort_ice_candidates(&input, false);
        assert_eq!(
            v4_first,
            vec![
                v4.clone(),
                v6.clone(),
                host.clone(),
                tcp.clone(),
                junk.clone()
            ]
        );
        let v6_first = sort_ice_candidates(&input, true);
        assert_eq!(v6_first, vec![v6, v4, host, tcp, junk]);

        assert!(is_literal_ipv4("255.255.255.255"));
        assert!(!is_literal_ipv4("256.1.1.1"));
        assert!(!is_literal_ipv4("1.2.3"));
        assert!(!is_literal_ipv4("a.b.c.d"));
    }

    #[test]
    fn art_url_normalisation() {
        assert_eq!(
            normalize_art_url("//img.example/p2.png"),
            "https://img.example/p2.png"
        );
        assert_eq!(
            normalize_art_url("https://img.example/p1.png"),
            "https://img.example/p1.png"
        );
        assert_eq!(normalize_art_url(""), "");
        let item = CloudLibraryItem {
            art_url: "//tile".to_owned(),
            gallery_image_urls: vec!["//g1".to_owned(), "https://g2".to_owned()],
            ..Default::default()
        };
        let card = item.to_card();
        assert_eq!(card.display_art_url, "https://tile");
        assert_eq!(card.gallery_image_urls, vec!["https://g1", "https://g2"]);
        let object = json_object([("a", json!(1)), ("b", json!("x"))]);
        assert_eq!(object["a"], 1);
        assert_eq!(object["b"], "x");
    }

    #[test]
    fn catalog_categories_prefer_localized_names() {
        let product = CatalogProduct::from_json(&serde_json::json!({
            "ProductId": "P1",
            "Categories": ["Racing & flying"],
            "LocalizedCategories": ["Rennspiele & Fliegen", 3, " "],
        }));
        assert_eq!(product.categories, ["Rennspiele & Fliegen"]);
        let product = CatalogProduct::from_json(&serde_json::json!({
            "ProductId": "P1",
            "Categories": ["Shooter"],
        }));
        assert_eq!(product.categories, ["Shooter"]);
    }
}
