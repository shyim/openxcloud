//! Port of qt/src/xcloud/XCloudApiClient.{h,cpp} (`XCloudApiClient`).
//!
//! HTTP client for the xCloud/xHome streaming APIs: console discovery, the
//! title library, session start/state/wait-time, the SDP and ICE signaling
//! exchanges with their polling loops, MSAL auth, keepalive and session
//! teardown. Mirrors xbox-xcloud-player's `apiclient.ts` plus the pieces the
//! Qt app added. The Game Pass catalog client lives in [`crate::catalog`].
//!
//! Differences from the C++:
//! - `QFuture<Result<T>>` becomes `async fn -> Result<T>`; the hand-rolled
//!   `QTimer::singleShot` polling loops are plain `loop { tokio::time::sleep }`.
//!   Attempt counts and intervals are passed as a [`PollPolicy`] so tests can
//!   use a 1 ms interval.
//! - Credentials and preferences live behind a `Mutex` so one client can be
//!   shared (`Arc<XCloudApiClient>`) between the session, the keepalive task
//!   and the UI.
//! - A transport failure propagates as the transport's `Error::Http`/`Timeout`;
//!   a non-2xx status becomes `Error::Status { status, body }`; signaling error
//!   envelopes and malformed ICE payloads become `Error::Other` with the C++
//!   message text.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::Value;
use xcloud_proto::models::{
    ConsoleListResponse, IceCandidatePayload, StreamSessionStartResponse, StreamStateResponse,
    WaitTimeResponse, XCloudTitlesResponse, dedupe_ice_candidates,
    is_preferred_outbound_ice_candidate, make_sdp_offer_body, make_start_stream_body,
    normalize_outbound_ice_candidate, sort_ice_candidates,
};
use xcloud_proto::preferences::{
    StreamKind, StreamPreferences, make_device_info, stream_kind_name,
};

use crate::transport::{Headers, HttpRequest, HttpResponse, HttpTransport};
use crate::{Error, Result};

const LOG: &str = "XCloudAPI";

/// Default `getCloudTitlesMru(limit = 25)`.
pub const DEFAULT_MRU_LIMIT: u32 = 25;

/// Attempt count and delay between polls of the `/sdp` and `/ice` endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollPolicy {
    pub max_attempts: u32,
    pub interval: Duration,
}

impl PollPolicy {
    /// `sendSdpOffer(maxAttempts = 60, pollIntervalMs = 500)`.
    pub const SDP: PollPolicy = PollPolicy {
        max_attempts: 60,
        interval: Duration::from_millis(500),
    };

    /// `sendIceCandidates(maxAttempts = 30, pollIntervalMs = 1000)`.
    pub const ICE: PollPolicy = PollPolicy {
        max_attempts: 30,
        interval: Duration::from_millis(1000),
    };

    pub const fn new(max_attempts: u32, interval: Duration) -> Self {
        Self {
            max_attempts,
            interval,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Credentials {
    base_host: String,
    gs_token: String,
    preferences: StreamPreferences,
    /// Cached `X-MS-Device-Info` built from preferences at credential update time.
    device_info: String,
}

/// HTTP client for the xCloud/xHome streaming surface.
pub struct XCloudApiClient {
    transport: Arc<dyn HttpTransport>,
    credentials: Mutex<Credentials>,
}

impl std::fmt::Debug for XCloudApiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let credentials = self.lock();
        f.debug_struct("XCloudApiClient")
            .field("base_host", &credentials.base_host)
            .field("preferences", &credentials.preferences)
            .finish_non_exhaustive()
    }
}

impl XCloudApiClient {
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            transport,
            credentials: Mutex::new(Credentials::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Credentials> {
        self.credentials
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `updateCredentials`: stores the regional host (trailing slashes removed)
    /// and the GSSV token, and refreshes the cached device-info header.
    pub fn update_credentials(&self, base_host: &str, gs_token: &str) {
        let mut credentials = self.lock();
        credentials.base_host = base_host.trim_end_matches('/').to_owned();
        credentials.gs_token = gs_token.to_owned();
        credentials.device_info = device_info_for(&credentials.preferences);
    }

    pub fn base_host(&self) -> String {
        self.lock().base_host.clone()
    }

    pub fn gs_token(&self) -> String {
        self.lock().gs_token.clone()
    }

    pub fn set_preferences(&self, preferences: &StreamPreferences) {
        let mut credentials = self.lock();
        credentials.preferences = preferences.clone();
        credentials.device_info = device_info_for(&credentials.preferences);
    }

    pub fn preferences(&self) -> StreamPreferences {
        self.lock().preferences.clone()
    }

    /// `authHeaders`: the header set every streaming-API request carries.
    pub fn auth_headers(&self) -> Headers {
        let credentials = self.lock();
        let device_info = if credentials.device_info.is_empty() {
            device_info_for(&credentials.preferences)
        } else {
            credentials.device_info.clone()
        };
        vec![
            ("Accept".to_owned(), "application/json".to_owned()),
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("X-Gssv-Client".to_owned(), "XboxComBrowser".to_owned()),
            ("X-MS-Device-Info".to_owned(), device_info),
            (
                "Authorization".to_owned(),
                format!("Bearer {}", credentials.gs_token),
            ),
        ]
    }

    /// `authorizedRequest`: `method` on `base_host + path` with the auth headers
    /// (or the slimmer `Accept`/`Authorization` pair the library endpoints get).
    async fn authorized_request(
        &self,
        method: &str,
        path: &str,
        body: Vec<u8>,
        library_style_headers: bool,
    ) -> Result<HttpResponse> {
        let (url, headers) = {
            let credentials = self.lock();
            let url = format!("{}{}", credentials.base_host, path);
            let headers = if library_style_headers {
                vec![
                    ("Accept".to_owned(), "application/json".to_owned()),
                    (
                        "Authorization".to_owned(),
                        format!("Bearer {}", credentials.gs_token),
                    ),
                ]
            } else {
                drop(credentials);
                self.auth_headers()
            };
            (url, headers)
        };
        self.transport
            .send(HttpRequest {
                method: method.to_owned(),
                url,
                headers,
                body,
                ..HttpRequest::default()
            })
            .await
    }

    // ------------------------------------------------------------------
    // Console discovery (home streaming).

    /// `GET /v6/servers/home`.
    pub async fn get_consoles(&self) -> Result<ConsoleListResponse> {
        let response = self
            .authorized_request("GET", "/v6/servers/home", Vec::new(), false)
            .await?;
        let object = success_json(&response)?;
        Ok(ConsoleListResponse::from_json(&object))
    }

    // ------------------------------------------------------------------
    // Library.

    /// `GET /v2/titles`.
    pub async fn get_cloud_titles(&self) -> Result<XCloudTitlesResponse> {
        let response = self
            .authorized_request("GET", "/v2/titles", Vec::new(), true)
            .await?;
        let object = success_json(&response)?;
        Ok(XCloudTitlesResponse::from_json(&object))
    }

    /// `GET /v2/titles/mru?mr=<limit>`: most recently used titles.
    pub async fn get_cloud_titles_mru(&self, limit: u32) -> Result<XCloudTitlesResponse> {
        let path = format!("/v2/titles/mru?mr={limit}");
        let response = self
            .authorized_request("GET", &path, Vec::new(), true)
            .await?;
        let object = success_json(&response)?;
        Ok(XCloudTitlesResponse::from_json(&object))
    }

    // ------------------------------------------------------------------
    // Stream lifecycle.

    /// `POST /v5/sessions/<kind>/play`. Also adopts `preferences` for the
    /// device-info header of every later request.
    pub async fn start_stream(
        &self,
        kind: StreamKind,
        target_id: &str,
        preferences: &StreamPreferences,
    ) -> Result<StreamSessionStartResponse> {
        self.set_preferences(preferences);
        let path = format!("/v5/sessions/{}/play", stream_kind_name(kind));
        let body = make_start_stream_body(kind, target_id, preferences).to_string();
        tracing::info!(target: LOG, "startStream path={path} body={body}");

        let response = self
            .authorized_request("POST", &path, body.into_bytes(), false)
            .await?;
        let object = success_json(&response)?;
        Ok(StreamSessionStartResponse::from_json(&object))
    }

    /// `GET <sessionPath>/state`.
    pub async fn get_session_state(&self, session_path: &str) -> Result<StreamStateResponse> {
        let path = format!("{session_path}/state");
        let response = self
            .authorized_request("GET", &path, Vec::new(), false)
            .await?;
        let object = success_json(&response)?;
        Ok(StreamStateResponse::from_json(&object))
    }

    /// `GET <sessionPath>/waittime/<titleId>`. A 204, a 404 or an empty body
    /// means "no estimate" and yields the default response.
    pub async fn get_wait_time(
        &self,
        session_path: &str,
        title_id: &str,
    ) -> Result<WaitTimeResponse> {
        let path = format!("{session_path}/waittime/{title_id}");
        let response = self
            .authorized_request("GET", &path, Vec::new(), false)
            .await?;
        if response.status == 204 || response.status == 404 || response.body.is_empty() {
            return Ok(WaitTimeResponse::default());
        }
        let object = success_json(&response)?;
        Ok(WaitTimeResponse::from_json(&object))
    }

    // ------------------------------------------------------------------
    // Signaling.

    /// `sendSdpOffer`: posts the offer to `<sessionPath>/sdp` and, when the
    /// answer is not returned inline, polls `GET <sessionPath>/sdp` up to
    /// `policy.max_attempts` times, `policy.interval` apart.
    pub async fn send_sdp_offer(
        &self,
        session_path: &str,
        sdp: &str,
        policy: PollPolicy,
    ) -> Result<String> {
        let path = format!("{session_path}/sdp");
        let body = make_sdp_offer_body(sdp).to_string();

        let response = self
            .authorized_request("POST", &path, body.into_bytes(), false)
            .await?;
        if let Some(answer) = sdp_answer_from_response(&response)? {
            return Ok(answer);
        }

        // Poll until the server publishes the answer.
        let mut remaining = policy.max_attempts;
        loop {
            if remaining == 0 {
                return Err(Error::Timeout("Request timed out".to_owned()));
            }
            tokio::time::sleep(policy.interval).await;
            let poll = self
                .authorized_request("GET", &path, Vec::new(), false)
                .await?;
            if let Some(answer) = sdp_answer_from_response(&poll)? {
                return Ok(answer);
            }
            remaining -= 1;
        }
    }

    /// `sendIceCandidates`: normalises, dedupes and sorts the local candidates,
    /// posts them in the official wire shape and polls `GET <sessionPath>/ice`
    /// for the remote candidates. Running out of attempts is not fatal and
    /// yields an empty list.
    pub async fn send_ice_candidates(
        &self,
        session_path: &str,
        candidates: &[IceCandidatePayload],
        prefer_ipv6: bool,
        policy: PollPolicy,
    ) -> Result<Vec<IceCandidatePayload>> {
        // Normalise, dedupe, then prefer the family the caller asked for.
        let normalized: Vec<IceCandidatePayload> = candidates
            .iter()
            .filter_map(|candidate| {
                normalize_outbound_ice_candidate(&candidate.candidate).map(|line| {
                    IceCandidatePayload {
                        candidate: line,
                        ..candidate.clone()
                    }
                })
            })
            .collect();
        let deduped = dedupe_ice_candidates(&normalized);
        let sorted = sort_ice_candidates(&deduped, prefer_ipv6);
        let preferred: Vec<IceCandidatePayload> = sorted
            .iter()
            .filter(|candidate| {
                is_preferred_outbound_ice_candidate(&candidate.candidate, prefer_ipv6)
            })
            .cloned()
            .collect();
        let to_send = if preferred.is_empty() {
            &sorted
        } else {
            &preferred
        };

        // The official web client posts messageType "iceCandidate" with a singular
        // "candidate" array of *stringified* candidate objects, terminated by an
        // a=end-of-candidates entry. Getting this shape wrong makes xCloud withhold
        // its real (Teredo) candidate and ignore our connectivity checks.
        let mut wire: Vec<Value> = to_send
            .iter()
            .map(|candidate| Value::String(candidate.to_json().to_string()))
            .collect();
        wire.push(Value::String(
            serde_json::json!({
                "candidate": "a=end-of-candidates",
                "sdpMid": "0",
                "sdpMLineIndex": 0,
            })
            .to_string(),
        ));
        let body = serde_json::json!({
            "messageType": "iceCandidate",
            "candidate": wire,
        })
        .to_string();

        tracing::info!(
            target: LOG,
            "ICE exchange posting {} candidate(s) (normalized {} / gathered {})",
            to_send.len(),
            deduped.len(),
            candidates.len()
        );

        let path = format!("{session_path}/ice");
        self.authorized_request("POST", &path, body.into_bytes(), false)
            .await?;

        let max_attempts = policy.max_attempts;
        let mut remaining = max_attempts;
        loop {
            if remaining == 0 {
                // No remote candidates published; not fatal.
                return Ok(Vec::new());
            }
            tokio::time::sleep(policy.interval).await;
            let poll = self
                .authorized_request("GET", &path, Vec::new(), false)
                .await?;
            if poll.status == 204 || poll.body.is_empty() {
                remaining -= 1;
                continue;
            }
            let root = json_root(&poll.body);
            if let Some((code, message)) = extract_signaling_error(&root) {
                return Err(Error::Other(format!("ICE server error {code}: {message}")));
            }
            let Some(remote) = extract_ice(&root) else {
                return Err(Error::Other(format!(
                    "Malformed ICE envelope: {}",
                    truncated_body(&poll.body, 256)
                )));
            };
            // Give a placeholder one extra poll to be replaced. Priority is
            // a preference, not proof of reachability: low-priority
            // candidates must still be tried instead of waiting 30s.
            let high_priority = has_high_priority_remote_candidate(&remote);
            tracing::info!(
                target: LOG,
                "ICE poll {}: HTTP {}, {} candidate(s), high priority: {}",
                max_attempts - remaining + 1,
                poll.status,
                remote.len(),
                high_priority
            );
            if !remote.is_empty() && (high_priority || remaining < max_attempts || remaining <= 1) {
                return Ok(remote);
            }
            remaining -= 1;
        }
    }

    /// `POST <sessionPath>/connect` with the MSAL user token.
    pub async fn send_msal_auth(&self, session_path: &str, user_token: &str) -> Result<()> {
        let path = format!("{session_path}/connect");
        let body = serde_json::json!({ "userToken": user_token }).to_string();
        let response = self
            .authorized_request("POST", &path, body.into_bytes(), false)
            .await?;
        expect_success(&response).map(|_| ())
    }

    /// `POST <sessionPath>/keepalive`.
    pub async fn send_keepalive(&self, session_path: &str) -> Result<()> {
        let path = format!("{session_path}/keepalive");
        let response = self
            .authorized_request("POST", &path, b"{}".to_vec(), false)
            .await?;
        expect_success(&response).map(|_| ())
    }

    /// `DELETE <sessionPath>`. A failed delete is best effort (the session
    /// expires server side anyway), so only a transport failure is an error.
    pub async fn stop_session(&self, session_path: &str) -> Result<()> {
        self.authorized_request("DELETE", session_path, Vec::new(), false)
            .await?;
        Ok(())
    }

    /// `cleanupStaleSessions`: closes any sessions left on the account (the
    /// service allows only one). Mirrors green-nx's cleanup_stale_sessions;
    /// failures are ignored.
    pub async fn cleanup_stale_sessions(&self, platform: StreamKind) -> Result<()> {
        let path = format!("/v5/sessions/{}/active", stream_kind_name(platform));
        // A 404 means there is no active-session endpoint.
        let response = match self
            .authorized_request("GET", &path, Vec::new(), false)
            .await
        {
            Ok(response) if response.is_success() => response,
            _ => return Ok(()),
        };
        let mut paths = Vec::new();
        collect_session_paths(&json_root(&response.body), &mut paths);
        for raw in paths {
            let full = if raw.starts_with('/') {
                raw
            } else {
                format!("/{raw}")
            };
            let _ = self
                .authorized_request("DELETE", &full, Vec::new(), false)
                .await;
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------
// Helpers

fn device_info_for(preferences: &StreamPreferences) -> String {
    make_device_info(
        &preferences.locale,
        preferences.resolution,
        &preferences.os_name_override,
    )
}

/// `HTTP %1: %2` failure for a non-2xx response.
fn expect_success(response: &HttpResponse) -> Result<&HttpResponse> {
    if response.is_success() {
        Ok(response)
    } else {
        Err(Error::Status {
            status: response.status,
            body: response.body_text(),
        })
    }
}

/// `QJsonDocument::fromJson(body).object()` after the success check: a non-2xx
/// is an error; an empty, malformed or non-object body reads as an empty
/// object (and so as a default-constructed DTO), never as a parse error.
fn success_json(response: &HttpResponse) -> Result<Value> {
    expect_success(response)?;
    Ok(match serde_json::from_slice::<Value>(&response.body) {
        Ok(value @ Value::Object(_)) => value,
        _ => Value::Object(Default::default()),
    })
}

/// `QJsonDocument::fromJson` followed by the `isObject() ? object() : array()`
/// dance: a malformed or scalar document reads as an empty array.
fn json_root(body: &[u8]) -> Value {
    match serde_json::from_slice::<Value>(body) {
        Ok(value @ (Value::Object(_) | Value::Array(_))) => value,
        _ => Value::Array(Vec::new()),
    }
}

/// `QJsonDocument::fromJson(text)`: `None` when the document is null.
fn json_root_text(text: &str) -> Option<Value> {
    match serde_json::from_str::<Value>(text) {
        Ok(value @ (Value::Object(_) | Value::Array(_))) => Some(value),
        _ => None,
    }
}

fn str_field<'a>(object: &'a Value, key: &str) -> &'a str {
    object.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Evaluates one `/sdp` response: `Ok(Some(answer))` when it carries the SDP,
/// `Err` for a signaling error envelope, `Ok(None)` to keep polling.
fn sdp_answer_from_response(response: &HttpResponse) -> Result<Option<String>> {
    if response.status == 204 || response.body.is_empty() {
        return Ok(None);
    }
    let root = json_root(&response.body);
    if let Some((code, message)) = extract_signaling_error(&root) {
        return Err(Error::Other(format!("SDP server error {code}: {message}")));
    }
    Ok(extract_sdp(&root))
}

/// `extractSdp`: the answer may be inline (`{"sdp"}`), nested under
/// `exchangeResponse`, or a stringified JSON document holding either.
fn extract_sdp(value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => {
            let sdp = str_field(value, "sdp");
            if !sdp.is_empty() {
                return Some(sdp.to_owned());
            }
            object.get("exchangeResponse").and_then(extract_sdp)
        }
        Value::String(text) => json_root_text(text).and_then(|root| extract_sdp(&root)),
        _ => None,
    }
}

/// `decodeCandidate`: the service returns candidates as objects or as
/// stringified JSON objects. Strips a leading "a=" and drops the
/// end-of-candidates sentinel and anything that is not a `candidate:` line.
fn decode_candidate(object: &Value) -> Option<IceCandidatePayload> {
    let mut candidate = str_field(object, "candidate").trim();
    if let Some(rest) = candidate.strip_prefix("a=") {
        candidate = rest;
    }
    if candidate.contains("end-of-candidates") {
        return None;
    }
    let candidate = candidate.trim();
    if !candidate.starts_with("candidate:") {
        return None;
    }
    let index = match object.get("sdpMLineIndex") {
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|value| value as i64))
            .map(|value| value as i32)?,
        Some(Value::String(text)) => text.parse::<i32>().ok()?,
        _ => return None,
    };

    Some(IceCandidatePayload {
        candidate: candidate.to_owned(),
        sdp_mline_index: index,
        sdp_mid: object
            .get("sdpMid")
            .and_then(Value::as_str)
            .unwrap_or("0")
            .to_owned(),
        username_fragment: str_field(object, "usernameFragment").to_owned(),
    })
}

/// `candidateObjectFromValue`: entries may be objects or strings containing a JSON object.
fn candidate_object_from_value(value: &Value) -> Option<Value> {
    match value {
        Value::Object(_) => Some(value.clone()),
        Value::String(text) => match serde_json::from_str::<Value>(text) {
            Ok(object @ Value::Object(_)) => Some(object),
            _ => None,
        },
        _ => None,
    }
}

fn decode_candidate_array(array: &[Value]) -> Vec<IceCandidatePayload> {
    array
        .iter()
        // end-of-candidates and malformed entries are skipped
        .filter_map(candidate_object_from_value)
        .filter_map(|object| decode_candidate(&object))
        .collect()
}

/// `extractIce`: remote candidates arrive as a bare array, nested under
/// `exchangeResponse` / `iceCandidates` / `candidates` (possibly stringified),
/// or as a single candidate object. `None` means the envelope is unrecognised.
fn extract_ice(value: &Value) -> Option<Vec<IceCandidatePayload>> {
    match value {
        Value::Array(array) => Some(decode_candidate_array(array)),
        Value::Object(object) => {
            for key in ["exchangeResponse", "iceCandidates", "candidates"] {
                if let Some(nested) = object.get(key) {
                    return extract_ice(nested);
                }
            }
            decode_candidate(value).map(|single| vec![single])
        }
        Value::String(text) => json_root_text(text).and_then(|root| extract_ice(&root)),
        _ => None,
    }
}

/// `extractSignalingError`: `(code, message)` when the payload carries an
/// `errorDetails` envelope.
fn extract_signaling_error(value: &Value) -> Option<(String, String)> {
    if !value.is_object() {
        return None;
    }
    let details = value.get("errorDetails").unwrap_or(&Value::Null);
    let code = str_field(details, "code");
    let message = str_field(details, "message");
    if code.is_empty() && message.is_empty() {
        return None;
    }
    Some((code.to_owned(), message.to_owned()))
}

fn truncated_body(body: &[u8], max_chars: usize) -> String {
    let text = String::from_utf8_lossy(body);
    if text.chars().count() <= max_chars {
        text.into_owned()
    } else {
        let mut truncated: String = text.chars().take(max_chars).collect();
        truncated.push_str("...");
        truncated
    }
}

/// A high-priority candidate lets us skip the brief grace period for the
/// initial front-door candidate. Low priority alone does not imply unusable.
fn has_high_priority_remote_candidate(candidates: &[IceCandidatePayload]) -> bool {
    candidates.iter().any(|candidate| {
        candidate
            .candidate
            .split(' ')
            .filter(|part| !part.is_empty())
            .nth(3)
            .and_then(|priority| priority.parse::<u64>().ok())
            .is_some_and(|priority| priority > 1000)
    })
}

/// `collectPaths`: every `sessionPath`/`path` string anywhere in the response.
fn collect_session_paths(value: &Value, paths: &mut Vec<String>) {
    match value {
        Value::Array(array) => {
            for item in array {
                collect_session_paths(item, paths);
            }
        }
        Value::Object(object) => {
            for (key, item) in object {
                if (key == "sessionPath" || key == "path")
                    && let Value::String(path) = item
                {
                    paths.push(path.clone());
                }
                collect_session_paths(item, paths);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    //! Port of qt/tests/tst_xcloudapi.cpp (the `XCloudApiClient` cases;
    //! `catalogReleasesBatchCallback` lives in `catalog.rs`).
    use std::sync::Arc;

    use serde_json::Value;
    use xcloud_proto::models::{
        IceCandidatePayload, dedupe_ice_candidates, is_preferred_outbound_ice_candidate,
        make_sdp_offer_body, normalize_outbound_ice_candidate, sort_ice_candidates,
    };
    use xcloud_proto::preferences::{
        StreamKind, StreamPreferences, StreamResolutionMode, make_device_info,
    };

    use super::*;
    use crate::transport::{FakeTransport, Reply};

    fn client(transport: &Arc<FakeTransport>) -> XCloudApiClient {
        let client = XCloudApiClient::new(transport.clone());
        client.update_credentials("https://host", "GS");
        client
    }

    fn request_body(transport: &FakeTransport, url_part: &str) -> Vec<u8> {
        transport
            .requests()
            .into_iter()
            .find(|request| request.url.contains(url_part))
            .map(|request| request.body)
            .unwrap_or_default()
    }

    fn header<'a>(headers: &'a Headers, name: &str) -> &'a str {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .unwrap_or("")
    }

    fn fast(max_attempts: u32) -> PollPolicy {
        PollPolicy::new(max_attempts, Duration::from_millis(1))
    }

    #[test]
    fn auth_headers_carry_device_info() {
        let transport = Arc::new(FakeTransport::new());
        let client = XCloudApiClient::new(transport);
        let preferences = StreamPreferences {
            resolution: StreamResolutionMode::P1080,
            ..StreamPreferences::default()
        };
        client.set_preferences(&preferences);
        client.update_credentials("https://westus.gssv-play-prod.xboxlive.com/", "GS");

        let headers = client.auth_headers();
        assert_eq!(header(&headers, "Authorization"), "Bearer GS");
        assert_eq!(header(&headers, "X-Gssv-Client"), "XboxComBrowser");
        assert_eq!(
            header(&headers, "X-MS-Device-Info"),
            make_device_info("en-US", StreamResolutionMode::P1080, "")
        );
        assert_eq!(
            client.base_host(),
            "https://westus.gssv-play-prod.xboxlive.com"
        );
    }

    #[tokio::test]
    async fn start_stream_body_matches_reference() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);

        transport.enqueue_json(
            "/v5/sessions/cloud/play",
            r#"{"sessionPath":"v5/sessions/cloud/ABC","sessionId":"ABC","state":"Provisioning"}"#,
            200,
        );

        let preferences = StreamPreferences {
            resolution: StreamResolutionMode::P720,
            fallback_region_names: vec!["eastus".to_owned()],
            ..StreamPreferences::default()
        };
        let result = client
            .start_stream(StreamKind::Cloud, "TITLE", &preferences)
            .await
            .unwrap();
        assert_eq!(result.session_path, "v5/sessions/cloud/ABC");

        let body: Value = serde_json::from_slice(&request_body(&transport, "/play")).unwrap();
        assert_eq!(body["titleId"], "TITLE");
        assert_eq!(body["serverId"], "");
        assert_eq!(body["clientSessionId"], "");
        let settings = &body["settings"];
        assert_eq!(settings["osName"], "android");
        assert_eq!(settings["nanoVersion"], "V3;WebrtcTransport.dll");
        assert_eq!(body["fallbackRegionNames"][0], "eastus");
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
        let chat = &configuration["chatConfiguration"];
        assert_eq!(chat["sampleFrequencyHz"], 24000);
        assert_eq!(chat["format"]["codec"], "opus");
    }

    #[tokio::test]
    async fn sdp_immediate_answer() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        transport.enqueue_json("/sessions/cloud/ABC/sdp", r#"{"sdp":"ANSWER"}"#, 200);

        let answer = client
            .send_sdp_offer("/v5/sessions/cloud/ABC", "v=0", PollPolicy::SDP)
            .await
            .unwrap();
        assert_eq!(answer, "ANSWER");
    }

    #[tokio::test]
    async fn sdp_polling_answer() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        // POST answers with 204, the follow-up GET carries the nested answer.
        transport.enqueue("/sessions/cloud/ABC/sdp", Reply::new(204, Vec::new()));
        transport.enqueue_json(
            "/sessions/cloud/ABC/sdp",
            r#"{"exchangeResponse":{"sdp":"POLLED"}}"#,
            200,
        );

        let answer = client
            .send_sdp_offer("/v5/sessions/cloud/ABC", "v=0", fast(5))
            .await
            .unwrap();
        assert_eq!(answer, "POLLED");
    }

    #[test]
    fn ice_candidate_normalization() {
        assert_eq!(
            normalize_outbound_ice_candidate("a=candidate:1 1 UDP 1 1.2.3.4 9 typ host"),
            Some("candidate:1 1 UDP 1 1.2.3.4 9 typ host".to_owned())
        );
        assert_eq!(normalize_outbound_ice_candidate("end-of-candidates"), None);
        assert_eq!(normalize_outbound_ice_candidate("not-a-candidate"), None);

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

    #[tokio::test]
    async fn ice_exchange_posts_normalized_candidates() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);

        transport.enqueue("/sessions/cloud/ABC/ice", Reply::new(200, Vec::new()));
        transport.enqueue_json(
            "/sessions/cloud/ABC/ice",
            r#"[{"candidate":"candidate:9 1 UDP 2122260223 10.0.0.1 5000 typ host","sdpMLineIndex":0,"sdpMid":"0"}]"#,
            200,
        );

        let candidate = IceCandidatePayload {
            candidate: "a=candidate:1 1 UDP 1 192.168.1.5 54321 typ host".to_owned(),
            sdp_mid: "0".to_owned(),
            sdp_mline_index: 0,
            ..IceCandidatePayload::default()
        };

        let result = client
            .send_ice_candidates("/v5/sessions/cloud/ABC", &[candidate], false, fast(3))
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].candidate,
            "candidate:9 1 UDP 2122260223 10.0.0.1 5000 typ host"
        );

        // The official wire shape: messageType iceCandidate, a singular "candidate"
        // array of stringified objects, terminated by end-of-candidates.
        let posted: Value = serde_json::from_slice(&request_body(&transport, "/ice")).unwrap();
        assert_eq!(posted["messageType"], "iceCandidate");
        let candidates = posted["candidate"].as_array().unwrap();
        assert_eq!(candidates.len(), 2);
        let first: Value = serde_json::from_str(candidates[0].as_str().unwrap()).unwrap();
        assert_eq!(
            first["candidate"],
            "candidate:1 1 UDP 1 192.168.1.5 54321 typ host"
        );
        let end: Value = serde_json::from_str(candidates[1].as_str().unwrap()).unwrap();
        assert_eq!(end["candidate"], "a=end-of-candidates");
    }

    #[tokio::test]
    async fn ice_parses_stringified_exchange_response() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);

        // Real server shape: exchangeResponse is a string holding an array whose
        // entries are themselves stringified candidate objects.
        let exchange = serde_json::json!([
            serde_json::json!({
                "candidate": "candidate:2 1 UDP 2122260223 203.0.113.7 9000 typ srflx",
                "sdpMLineIndex": 0,
                "sdpMid": "0",
            })
            .to_string(),
            serde_json::json!({
                "candidate": "a=end-of-candidates",
                "sdpMLineIndex": 0,
                "sdpMid": "0",
            })
            .to_string(),
        ])
        .to_string();

        transport.enqueue("/sessions/cloud/ABC/ice", Reply::new(200, Vec::new()));
        transport.enqueue_json(
            "/sessions/cloud/ABC/ice",
            &serde_json::json!({ "exchangeResponse": exchange }).to_string(),
            200,
        );

        let candidate =
            IceCandidatePayload::new("candidate:9 1 UDP 2122260223 192.168.1.5 54321 typ host");
        let result = client
            .send_ice_candidates("/v5/sessions/cloud/ABC", &[candidate], false, fast(3))
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].candidate.contains("203.0.113.7"));
    }

    #[tokio::test]
    async fn ice_polls_past_placeholder_candidate() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);

        // POST, then two GET polls: a dead front-door placeholder first, then the
        // real candidate.
        transport.enqueue("/sessions/cloud/ABC/ice", Reply::new(200, Vec::new()));
        transport.enqueue_json(
            "/sessions/cloud/ABC/ice",
            r#"[{"candidate":"candidate:1 1 UDP 100 13.104.0.1 9000 typ host","sdpMLineIndex":0,"sdpMid":"0"}]"#,
            200,
        );
        transport.enqueue_json(
            "/sessions/cloud/ABC/ice",
            r#"[{"candidate":"candidate:2 1 UDP 2122260223 203.0.113.7 9000 typ srflx","sdpMLineIndex":0,"sdpMid":"0"}]"#,
            200,
        );

        let candidate =
            IceCandidatePayload::new("candidate:9 1 UDP 2122260223 192.168.1.5 54321 typ host");
        let result = client
            .send_ice_candidates("/v5/sessions/cloud/ABC", &[candidate], false, fast(3))
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!(result[0].candidate.contains("203.0.113.7"));
    }

    #[tokio::test]
    async fn ice_tries_low_priority_candidates_promptly() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        transport.enqueue("/sessions/cloud/ABC/ice", Reply::new(200, Vec::new()));
        for _ in 0..2 {
            transport.enqueue_json(
                "/sessions/cloud/ABC/ice",
                r#"[{"candidate":"candidate:1 1 UDP 100 203.0.113.7 9000 typ host","sdpMLineIndex":0,"sdpMid":"0"}]"#,
                200,
            );
        }
        let result = client
            .send_ice_candidates("/v5/sessions/cloud/ABC", &[], false, fast(30))
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(transport.requests().len(), 3);
    }

    #[tokio::test]
    async fn wait_time_handles_no_content() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        transport.enqueue("/waittime/", Reply::new(204, Vec::new()));

        let result = client
            .get_wait_time("/v5/sessions/cloud/ABC", "TITLE")
            .await
            .unwrap();
        assert_eq!(result.estimated_total_wait_time_in_seconds, 0);
    }

    /// `QJsonDocument::fromJson(body).object()` on a 2xx with a malformed,
    /// truncated or scalar body yields an empty object, so every endpoint
    /// returns a default DTO rather than a JSON parse error.
    #[tokio::test]
    async fn malformed_success_body_reads_as_default_dto() {
        let bodies: [&[u8]; 4] = [
            b"<html><body>Service Unavailable</body></html>",
            b"{",
            b"42",
            b"[{\"state\":\"ReadyToConnect\"}]",
        ];
        for body in bodies {
            let transport = Arc::new(FakeTransport::new());
            let client = client(&transport);
            transport.enqueue("/v6/servers/home", Reply::new(200, body.to_vec()));
            transport.enqueue("/v2/titles/mru", Reply::new(200, body.to_vec()));
            transport.enqueue("/v2/titles", Reply::new(200, body.to_vec()));
            transport.enqueue("/play", Reply::new(200, body.to_vec()));
            transport.enqueue("/state", Reply::new(200, body.to_vec()));
            transport.enqueue("/waittime/", Reply::new(200, body.to_vec()));

            let label = String::from_utf8_lossy(body).into_owned();
            assert_eq!(
                client.get_consoles().await.unwrap(),
                ConsoleListResponse::default(),
                "{label}"
            );
            assert_eq!(
                client.get_cloud_titles_mru(10).await.unwrap(),
                XCloudTitlesResponse::default(),
                "{label}"
            );
            assert_eq!(
                client.get_cloud_titles().await.unwrap(),
                XCloudTitlesResponse::default(),
                "{label}"
            );
            assert_eq!(
                client
                    .start_stream(StreamKind::Cloud, "TITLE", &StreamPreferences::default())
                    .await
                    .unwrap(),
                StreamSessionStartResponse::default(),
                "{label}"
            );
            let state = client
                .get_session_state("/v5/sessions/cloud/ABC")
                .await
                .unwrap();
            assert_eq!(state, StreamStateResponse::default(), "{label}");
            assert!(!state.has_error(), "{label}");
            assert_eq!(
                client
                    .get_wait_time("/v5/sessions/cloud/ABC", "TITLE")
                    .await
                    .unwrap(),
                WaitTimeResponse::default(),
                "{label}"
            );
        }
    }

    /// The success check still comes first: a non-2xx with a malformed body
    /// is an `HTTP <status>: <body>` error, not an empty DTO.
    #[tokio::test]
    async fn malformed_failure_body_is_still_a_status_error() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        transport.enqueue("/state", Reply::new(503, b"<html>down</html>".to_vec()));

        let err = client
            .get_session_state("/v5/sessions/cloud/ABC")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Status { status: 503, .. }), "{err}");
        assert!(err.to_string().contains("<html>down</html>"), "{err}");
    }

    #[tokio::test]
    async fn stop_session_issues_delete() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        transport.enqueue("/v5/sessions/cloud/ABC", Reply::new(204, Vec::new()));

        client.stop_session("/v5/sessions/cloud/ABC").await.unwrap();
        assert!(
            transport
                .requests()
                .iter()
                .any(|request| request.method == "DELETE")
        );
    }

    #[tokio::test]
    async fn cleanup_stale_sessions_deletes_old_sessions() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);

        transport.enqueue_json(
            "/v5/sessions/cloud/active",
            r#"{"results":[{"sessionPath":"v5/sessions/cloud/OLD"}]}"#,
            200,
        );
        transport.enqueue("/v5/sessions/cloud/OLD", Reply::new(204, Vec::new()));

        client
            .cleanup_stale_sessions(StreamKind::Cloud)
            .await
            .unwrap();

        assert!(transport.requests().iter().any(|request| {
            request.method == "DELETE" && request.url.contains("v5/sessions/cloud/OLD")
        }));
    }

    // -- Rust-side coverage of behaviour the C++ exercised only end to end. --

    #[tokio::test]
    async fn sdp_polling_times_out_after_max_attempts() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        transport.enqueue("/sdp", Reply::new(204, Vec::new()));
        for _ in 0..3 {
            transport.enqueue("/sdp", Reply::new(204, Vec::new()));
        }
        let err = client
            .send_sdp_offer("/v5/sessions/cloud/ABC", "v=0", fast(3))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Timeout(_)), "{err}");
        assert_eq!(transport.requests().len(), 4);
    }

    #[tokio::test]
    async fn sdp_surfaces_signaling_error_envelope() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        transport.enqueue_json(
            "/sdp",
            r#"{"errorDetails":{"code":"SdpFailed","message":"bad offer"}}"#,
            200,
        );
        let err = client
            .send_sdp_offer("/v5/sessions/cloud/ABC", "v=0", fast(3))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "SDP server error SdpFailed: bad offer");
    }

    #[tokio::test]
    async fn ice_gives_up_quietly_after_max_attempts() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        transport.enqueue("/ice", Reply::new(200, Vec::new()));
        for _ in 0..2 {
            transport.enqueue("/ice", Reply::new(204, Vec::new()));
        }
        let result = client
            .send_ice_candidates("/v5/sessions/cloud/ABC", &[], false, fast(2))
            .await
            .unwrap();
        assert!(result.is_empty());
        assert_eq!(transport.requests().len(), 3);
    }

    #[tokio::test]
    async fn ice_rejects_malformed_envelope() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        transport.enqueue("/ice", Reply::new(200, Vec::new()));
        transport.enqueue_json("/ice", r#"{"unexpected":true}"#, 200);
        let err = client
            .send_ice_candidates("/v5/sessions/cloud/ABC", &[], false, fast(2))
            .await
            .unwrap_err();
        assert!(
            err.to_string().starts_with("Malformed ICE envelope: "),
            "{err}"
        );
    }

    #[tokio::test]
    async fn non_success_status_is_reported() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        transport.enqueue_json("/state", r#"{"error":"nope"}"#, 500);
        let err = client
            .get_session_state("/v5/sessions/cloud/ABC")
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Status { status: 500, body } if body == r#"{"error":"nope"}"#),
            "{err}"
        );
    }

    #[tokio::test]
    async fn library_requests_use_slim_headers() {
        let transport = Arc::new(FakeTransport::new());
        let client = client(&transport);
        transport.enqueue_json("/v2/titles/mru?mr=7", r#"{"results":[]}"#, 200);
        transport.enqueue_json("/v2/titles", r#"{"results":[]}"#, 200);
        client.get_cloud_titles_mru(7).await.unwrap();
        client.get_cloud_titles().await.unwrap();
        for request in transport.requests() {
            assert_eq!(request.header("Authorization"), Some("Bearer GS"));
            assert_eq!(request.header("Accept"), Some("application/json"));
            assert_eq!(request.header("X-MS-Device-Info"), None);
        }
    }

    #[test]
    fn extract_ice_handles_nested_and_single_shapes() {
        let single = serde_json::json!({
            "candidate": "candidate:1 1 UDP 5 1.2.3.4 9 typ host",
            "sdpMLineIndex": "0",
            "sdpMid": "0",
        });
        assert_eq!(extract_ice(&single).unwrap().len(), 1);
        let nested = serde_json::json!({ "iceCandidates": [single.clone(), "garbage"] });
        assert_eq!(extract_ice(&nested).unwrap().len(), 1);
        let candidates = serde_json::json!({ "candidates": [single] });
        assert_eq!(extract_ice(&candidates).unwrap().len(), 1);
        assert!(extract_ice(&serde_json::json!({ "nope": 1 })).is_none());
        assert!(extract_ice(&Value::String("not json".to_owned())).is_none());
        assert!(!has_high_priority_remote_candidate(&[
            IceCandidatePayload::new("candidate:1 1 UDP 1000 1.2.3.4 9 typ host")
        ]));
    }

    #[test]
    fn truncated_body_adds_ellipsis() {
        assert_eq!(truncated_body(b"abc", 5), "abc");
        assert_eq!(truncated_body(b"abcdef", 3), "abc...");
    }
}
