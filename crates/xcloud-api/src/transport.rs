//! Port of qt/src/core/HttpTransport.{h,cpp} (`HttpTransport` trait, `ReqwestTransport`)
//! and qt/tests/FakeHttpTransport.h (`FakeTransport`, the deterministic double other
//! crates' tests drive the auth/API layers with).
//!
//! Differences from the C++:
//! - `QtHttpTransport::send` never fails and reports transport failures through
//!   `HttpResponse::error`. Here `send` returns `Result<HttpResponse>`: an HTTP status of
//!   any class (including 4xx/5xx, whose bodies are meaningful) is `Ok`, while a failure
//!   that produced no status at all (DNS, TLS, connection reset, timeout, refused
//!   redirect) is `Err(Error::Http)` / `Err(Error::Timeout)`.
//! - `QMap<QString, QString>` headers become an insertion-ordered `Vec<(String, String)>`
//!   with case-insensitive replace/lookup helpers.
//! - `HttpRequest::timeout` keeps Qt's `setTransferTimeout` meaning: it is an
//!   *inactivity* timeout (the reply is aborted when no bytes arrive for that long), not
//!   a total deadline. `Duration::ZERO` disables it, like `timeoutMs <= 0`.
//! - `QNetworkAccessManager` silently adds `User-Agent: Mozilla/5.0`, `Accept-Language`
//!   (from the system locale) and `Connection: Keep-Alive` to every request that does not
//!   set them (`QHttpNetworkConnectionPrivate::prepareRequest`). [`ReqwestTransport`]
//!   installs the same defaults so the wire format matches; per-request headers win.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;

use crate::{Error, Result};

/// Default per-request transfer timeout (`HttpRequest::timeoutMs = 30000`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum redirects followed, matching `QNetworkRequest::maximumRedirectsAllowed`.
const MAX_REDIRECTS: usize = 50;

/// User-Agent `QNetworkAccessManager` sends when the request sets none.
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0";

/// Error text for an aborted transfer (`reply->error() == OperationCanceledError`).
const TIMED_OUT: &str = "Request timed out";

/// `Accept-Language` value Qt derives from `QLocale::system().name()` (for example
/// `"en_US"`, `"de_DE"` or `"C"`), see `QHttpNetworkConnectionPrivate::prepareRequest`:
/// `C` -> `en,*`; `en-XX` -> `en-XX,*`; anything else -> `xx-YY,en,*`.
pub fn accept_language(system_locale: &str) -> String {
    let locale = system_locale.replace('_', "-");
    if locale == "C" {
        "en,*".to_owned()
    } else if locale.starts_with("en-") {
        format!("{locale},*")
    } else {
        format!("{locale},en,*")
    }
}

/// `QLocale::system().name()` approximation: the first of `LC_ALL`, `LC_MESSAGES`, `LANG`
/// that is set, stripped of its encoding/modifier (`de_DE.UTF-8@euro` -> `de_DE`). Qt on
/// macOS reads the CFLocale preferences instead; without that we fall back to `C`, which
/// yields Qt's `en,*`.
pub fn system_locale_name() -> String {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .map(|value| {
            value
                .split(['.', '@'])
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned()
        })
        .find(|name| !name.is_empty())
        .filter(|name| name != "POSIX")
        .unwrap_or_else(|| "C".to_owned())
}

/// The `Accept-Language` header value for this machine, as Qt would send it.
pub fn system_accept_language() -> String {
    accept_language(&system_locale_name())
}

/// Insertion-ordered header list; the C++ used `QMap<QString, QString>`.
pub type Headers = Vec<(String, String)>;

/// One outbound request. Mirrors `HttpRequest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Headers,
    pub body: Vec<u8>,
    pub timeout: Duration,
}

impl Default for HttpRequest {
    fn default() -> Self {
        Self {
            method: "GET".to_owned(),
            url: String::new(),
            headers: Headers::new(),
            body: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl HttpRequest {
    /// Case-insensitive header lookup (`request.headers.value(name)` in the C++ tests).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Inserts or replaces a header, like `QMap::insert`. Replacement keeps the original
    /// position so the header order stays deterministic.
    pub fn set_header(&mut self, name: &str, value: &str) {
        if let Some(slot) = self
            .headers
            .iter_mut()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
        {
            slot.1 = value.to_owned();
        } else {
            self.headers.push((name.to_owned(), value.to_owned()));
        }
    }

    /// Request body as text (lossy UTF-8).
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// The final response of a request. Mirrors `HttpResponse` minus the `error`
/// field, which became the `Err` arm of [`HttpTransport::send`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    /// URL the request was sent to (the C++ reports `reply->request().url()`).
    pub url: String,
}

impl HttpResponse {
    /// `HttpResponse::isSuccess`: a 2xx status.
    pub fn is_success(&self) -> bool {
        is_success(self.status)
    }

    /// `HttpResponse::bodyText`.
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Parses the body as JSON.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_slice(&self.body)?)
    }
}

/// True for 2xx status codes.
pub fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

/// Seam that lets tests drive the auth/API layers without a live network.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// Performs `request` and resolves with the final response, including any HTTP
    /// error status. Only failures without a status become `Err`.
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse>;

    async fn get(&self, url: &str, headers: Headers) -> Result<HttpResponse> {
        self.send(HttpRequest {
            method: "GET".to_owned(),
            url: url.to_owned(),
            headers,
            ..HttpRequest::default()
        })
        .await
    }

    async fn post_json(&self, url: &str, json: Vec<u8>, headers: Headers) -> Result<HttpResponse> {
        let mut request = HttpRequest {
            method: "POST".to_owned(),
            url: url.to_owned(),
            headers,
            body: json,
            ..HttpRequest::default()
        };
        request.set_header("Content-Type", "application/json");
        request.set_header("Accept", "application/json");
        self.send(request).await
    }

    async fn post_form(&self, url: &str, form: Vec<u8>, headers: Headers) -> Result<HttpResponse> {
        let mut request = HttpRequest {
            method: "POST".to_owned(),
            url: url.to_owned(),
            headers,
            body: form,
            ..HttpRequest::default()
        };
        request.set_header("Content-Type", "application/x-www-form-urlencoded");
        self.send(request).await
    }

    async fn post_raw(&self, url: &str, body: Vec<u8>, headers: Headers) -> Result<HttpResponse> {
        self.send(HttpRequest {
            method: "POST".to_owned(),
            url: url.to_owned(),
            headers,
            body,
            ..HttpRequest::default()
        })
        .await
    }

    /// `HttpTransport::deleteResource`.
    async fn delete(&self, url: &str, headers: Headers) -> Result<HttpResponse> {
        self.send(HttpRequest {
            method: "DELETE".to_owned(),
            url: url.to_owned(),
            headers,
            ..HttpRequest::default()
        })
        .await
    }
}

/// Production transport built on `reqwest` (rustls). Port of `QtHttpTransport`.
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// A `ClientBuilder` carrying the `QNetworkAccessManager` behaviour the Qt client got
    /// for free, for callers that want to layer their own settings (proxy, ...) on top:
    /// - `NoLessSafeRedirectPolicy`: follows up to 50 redirects but refuses an
    ///   `https` -> `http` downgrade;
    /// - the default headers `User-Agent: Mozilla/5.0`, `Accept-Language: <locale>` and
    ///   `Connection: Keep-Alive`, each replaced by a per-request header of the same name.
    pub fn builder() -> reqwest::ClientBuilder {
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                return attempt.error("too many redirects");
            }
            let downgrade = attempt.previous().last().map(|from| from.scheme()) == Some("https")
                && attempt.url().scheme() != "https";
            if downgrade {
                attempt.error("insecure redirect refused (https -> http)")
            } else {
                attempt.follow()
            }
        });
        let mut defaults = reqwest::header::HeaderMap::new();
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&system_accept_language()) {
            defaults.insert(reqwest::header::ACCEPT_LANGUAGE, value);
        }
        defaults.insert(
            reqwest::header::CONNECTION,
            reqwest::header::HeaderValue::from_static("Keep-Alive"),
        );
        reqwest::Client::builder()
            .redirect(policy)
            .user_agent(DEFAULT_USER_AGENT)
            .default_headers(defaults)
    }

    /// Builds the production client from [`ReqwestTransport::builder`].
    pub fn new() -> Result<Self> {
        let client = Self::builder()
            .build()
            .map_err(|e| Error::Http(e.to_string()))?;
        Ok(Self { client })
    }

    /// Wraps an existing client (e.g. one configured with a proxy). Start from
    /// [`ReqwestTransport::builder`] to keep the Qt redirect and header defaults.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

fn map_reqwest_error(error: reqwest::Error) -> Error {
    if error.is_timeout() {
        // A cancelled reply is the expected outcome of a timeout.
        Error::Timeout(TIMED_OUT.to_owned())
    } else {
        Error::Http(error.to_string())
    }
}

/// Runs one step of the transfer under Qt's transfer-timeout rule: the step must make
/// progress within `inactivity`, or the transfer is aborted. `Duration::ZERO` disables
/// the timer (`QNetworkRequest::setTransferTimeout(0)`).
async fn progress_within<T>(
    inactivity: Duration,
    step: impl Future<Output = reqwest::Result<T>>,
) -> Result<T> {
    if inactivity.is_zero() {
        return step.await.map_err(map_reqwest_error);
    }
    match tokio::time::timeout(inactivity, step).await {
        Ok(result) => result.map_err(map_reqwest_error),
        Err(_elapsed) => Err(Error::Timeout(TIMED_OUT.to_owned())),
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|e| Error::Http(format!("invalid method {:?}: {e}", request.method)))?;
        let mut builder = self.client.request(method.clone(), request.url.as_str());
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        // The Qt code sends GET/DELETE without a body when it is empty, and any other
        // verb with the (possibly empty) body; reqwest sends no body for an empty one.
        let is_bodyless = request.body.is_empty()
            && (method == reqwest::Method::GET || method == reqwest::Method::DELETE);
        if !is_bodyless {
            builder = builder.body(request.body);
        }

        tracing::trace!(target: "Http", method = %method, url = %request.url, "send");
        // Qt's transfer timeout is restarted on every progress notification. Headers
        // (connect + upload + first response bytes) are one step, then each body chunk
        // is another, so a slow but moving download succeeds while a stall is cut off
        // `timeout` after the last byte.
        let inactivity = request.timeout;
        let mut response = progress_within(inactivity, builder.send()).await?;

        let status = response.status().as_u16();
        let mut body = Vec::new();
        while let Some(chunk) = progress_within(inactivity, response.chunk()).await? {
            body.extend_from_slice(&chunk);
        }
        Ok(HttpResponse {
            status,
            body,
            url: request.url,
        })
    }
}

/// One canned reply for [`FakeTransport`]. Mirrors `FakeHttpTransport::Reply`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    /// Non-empty makes `send` fail with `Error::Http(error)`, like a transport error.
    pub error: String,
    pub delay_ms: u64,
}

impl Reply {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            body: body.into(),
            error: String::new(),
            delay_ms: 0,
        }
    }
}

#[derive(Debug, Clone)]
struct Canned {
    /// Restricts the match to one HTTP method; `None` matches any (the C++ behaviour).
    method: Option<String>,
    url_contains: String,
    reply: Reply,
}

#[derive(Debug, Default)]
struct FakeState {
    replies: VecDeque<Canned>,
    requests: Vec<HttpRequest>,
}

/// Deterministic transport double. Matches queued replies by a URL substring (first
/// queued match wins and is consumed) and records every request. Port of
/// `FakeHttpTransport`; `pub` so other crates' tests can use it.
#[derive(Debug, Default)]
pub struct FakeTransport {
    state: Mutex<FakeState>,
}

impl FakeTransport {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Queues a reply for any request whose URL contains `url_contains`.
    pub fn enqueue(&self, url_contains: &str, reply: Reply) {
        self.lock().replies.push_back(Canned {
            method: None,
            url_contains: url_contains.to_owned(),
            reply,
        });
    }

    /// Queues a reply for requests with `method` whose URL contains `url_contains`.
    pub fn enqueue_for(&self, method: &str, url_contains: &str, reply: Reply) {
        self.lock().replies.push_back(Canned {
            method: Some(method.to_owned()),
            url_contains: url_contains.to_owned(),
            reply,
        });
    }

    pub fn enqueue_json(&self, url_contains: &str, json: &str, status: u16) {
        self.enqueue(url_contains, Reply::new(status, json.as_bytes()));
    }

    pub fn enqueue_json_delayed(&self, url_contains: &str, json: &str, delay_ms: u64) {
        self.enqueue(
            url_contains,
            Reply {
                delay_ms,
                ..Reply::new(200, json.as_bytes())
            },
        );
    }

    pub fn enqueue_error(&self, url_contains: &str, error: &str) {
        self.enqueue(
            url_contains,
            Reply {
                status: 0,
                body: Vec::new(),
                error: error.to_owned(),
                delay_ms: 0,
            },
        );
    }

    /// Every request seen so far, in order.
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.lock().requests.clone()
    }

    pub fn request_count(&self) -> usize {
        self.lock().requests.len()
    }

    /// Number of canned replies not yet consumed.
    pub fn pending_count(&self) -> usize {
        self.lock().replies.len()
    }

    fn take_reply(&self, request: &HttpRequest) -> Option<Reply> {
        let mut state = self.lock();
        state.requests.push(request.clone());
        let index = state.replies.iter().position(|canned| {
            canned
                .method
                .as_deref()
                .is_none_or(|m| m.eq_ignore_ascii_case(&request.method))
                && request.url.contains(&canned.url_contains)
        })?;
        state.replies.remove(index).map(|canned| canned.reply)
    }
}

#[async_trait]
impl HttpTransport for FakeTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse> {
        let Some(reply) = self.take_reply(&request) else {
            return Err(Error::Http(format!(
                "no canned response for {}",
                request.url
            )));
        };
        if reply.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(reply.delay_ms)).await;
        }
        if !reply.error.is_empty() {
            return Err(Error::Http(reply.error));
        }
        Ok(HttpResponse {
            status: reply.status,
            body: reply.body,
            url: request.url,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_success_covers_2xx_only() {
        assert!(!is_success(199));
        assert!(is_success(200));
        assert!(is_success(204));
        assert!(is_success(299));
        assert!(!is_success(300));
        assert!(!is_success(404));
        assert!(!is_success(0));
        assert!(
            HttpResponse {
                status: 201,
                ..HttpResponse::default()
            }
            .is_success()
        );
    }

    #[test]
    fn request_defaults_mirror_cpp() {
        let request = HttpRequest::default();
        assert_eq!(request.method, "GET");
        assert_eq!(request.timeout, Duration::from_secs(30));
        assert!(request.headers.is_empty());
        assert!(request.body.is_empty());
    }

    #[test]
    fn set_header_replaces_case_insensitively_and_keeps_order() {
        let mut request = HttpRequest::default();
        request.set_header("Authorization", "a");
        request.set_header("content-type", "text/plain");
        request.set_header("Content-Type", "application/json");
        assert_eq!(
            request.headers,
            vec![
                ("Authorization".to_owned(), "a".to_owned()),
                ("content-type".to_owned(), "application/json".to_owned()),
            ]
        );
        assert_eq!(request.header("CONTENT-TYPE"), Some("application/json"));
        assert_eq!(request.header("Accept"), None);
    }

    #[tokio::test]
    async fn fake_matches_url_substring_and_consumes_reply() {
        let transport = FakeTransport::new();
        transport.enqueue_json("/state", r#"{"state":"Provisioning"}"#, 200);
        transport.enqueue_json("/state", r#"{"state":"ReadyToConnect"}"#, 200);

        let first = transport
            .get("https://host/v5/sessions/cloud/ABC/state", vec![])
            .await
            .unwrap();
        assert_eq!(first.status, 200);
        assert_eq!(first.body_text(), r#"{"state":"Provisioning"}"#);
        assert_eq!(first.url, "https://host/v5/sessions/cloud/ABC/state");

        let second = transport
            .get("https://host/v5/sessions/cloud/ABC/state", vec![])
            .await
            .unwrap();
        assert_eq!(second.body_text(), r#"{"state":"ReadyToConnect"}"#);
        assert_eq!(transport.pending_count(), 0);

        let err = transport
            .get("https://host/v5/sessions/cloud/ABC/state", vec![])
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Http(msg) if msg == "no canned response for https://host/v5/sessions/cloud/ABC/state"),
            "{err}"
        );
        assert_eq!(transport.request_count(), 3);
    }

    #[tokio::test]
    async fn fake_first_matching_reply_wins_regardless_of_queue_position() {
        let transport = FakeTransport::new();
        transport.enqueue_json("/v2/titles", r#"{"a":1}"#, 200);
        transport.enqueue_json("catalog.gamepass.com", r#"{"b":2}"#, 200);

        let catalog = transport
            .get("https://catalog.gamepass.com/products", vec![])
            .await
            .unwrap();
        assert_eq!(catalog.body_text(), r#"{"b":2}"#);
        let titles = transport
            .get("https://host/v2/titles", vec![])
            .await
            .unwrap();
        assert_eq!(titles.body_text(), r#"{"a":1}"#);
    }

    #[tokio::test]
    async fn fake_enqueue_error_fails_send() {
        let transport = FakeTransport::new();
        transport.enqueue_error("xgpuwebf2p.gssv-play-prod.xboxlive.com", "not entitled");
        let err = transport
            .get(
                "https://xgpuwebf2p.gssv-play-prod.xboxlive.com/v2/login",
                vec![],
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Http(msg) if msg == "not entitled"),
            "{err}"
        );
        assert_eq!(transport.request_count(), 1);
    }

    #[tokio::test]
    async fn fake_non_2xx_status_is_ok_response() {
        let transport = FakeTransport::new();
        transport.enqueue("/sessions/cloud/ABC/sdp", Reply::new(204, Vec::new()));
        transport.enqueue_json("/waittime/", r#"{"error":"nope"}"#, 500);
        let sdp = transport
            .get("https://host/v5/sessions/cloud/ABC/sdp", vec![])
            .await
            .unwrap();
        assert_eq!(sdp.status, 204);
        assert!(sdp.body.is_empty());
        assert!(sdp.is_success());
        let wait = transport
            .get("https://host/v5/waittime/123", vec![])
            .await
            .unwrap();
        assert_eq!(wait.status, 500);
        assert!(!wait.is_success());
        assert_eq!(wait.body_text(), r#"{"error":"nope"}"#);
    }

    #[tokio::test]
    async fn fake_delayed_reply_waits() {
        let transport = FakeTransport::new();
        transport.enqueue_json_delayed("catalog.gamepass.com", r#"{"Products":{}}"#, 40);
        let started = tokio::time::Instant::now();
        let response = transport
            .get("https://catalog.gamepass.com/v3/products", vec![])
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body_text(), r#"{"Products":{}}"#);
        assert!(started.elapsed() >= Duration::from_millis(40));
    }

    #[tokio::test]
    async fn fake_records_requests_in_order_with_method_headers_and_body() {
        let transport = FakeTransport::new();
        transport.enqueue_json("devicecode", r#"{"user_code":"ABCD"}"#, 200);
        transport.enqueue("/v5/sessions/cloud/ABC", Reply::new(204, Vec::new()));

        transport
            .post_form(
                "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode",
                b"client_id=x&scope=y".to_vec(),
                vec![],
            )
            .await
            .unwrap();
        transport
            .delete(
                "https://host/v5/sessions/cloud/ABC",
                vec![("Authorization".to_owned(), "Bearer T".to_owned())],
            )
            .await
            .unwrap();

        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].body, b"client_id=x&scope=y");
        assert!(
            requests[0]
                .header("Content-Type")
                .unwrap()
                .contains("x-www-form-urlencoded")
        );
        assert_eq!(requests[1].method, "DELETE");
        assert_eq!(requests[1].header("Authorization"), Some("Bearer T"));
        assert!(requests[1].body.is_empty());
    }

    #[tokio::test]
    async fn fake_enqueue_for_filters_by_method() {
        let transport = FakeTransport::new();
        transport.enqueue_for("DELETE", "/v5/sessions/cloud/", Reply::new(204, Vec::new()));
        transport.enqueue_for(
            "get",
            "/v5/sessions/cloud/",
            Reply::new(200, b"{}".to_vec()),
        );

        let get = transport
            .get("https://host/v5/sessions/cloud/active", vec![])
            .await
            .unwrap();
        assert_eq!(get.status, 200);
        let del = transport
            .delete("https://host/v5/sessions/cloud/OLD", vec![])
            .await
            .unwrap();
        assert_eq!(del.status, 204);
        assert!(
            transport
                .post_raw("https://host/v5/sessions/cloud/x", Vec::new(), vec![])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn post_json_adds_content_type_and_accept_without_clobbering_others() {
        let transport = FakeTransport::new();
        transport.enqueue_json("xsts.auth.xboxlive.com", r#"{"Token":"XSTS"}"#, 200);
        let response = transport
            .post_json(
                "https://xsts.auth.xboxlive.com/xsts/authorize",
                br#"{"a":1}"#.to_vec(),
                vec![
                    ("x-xbl-contract-version".to_owned(), "1".to_owned()),
                    ("Accept".to_owned(), "text/plain".to_owned()),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            response.json::<serde_json::Value>().unwrap()["Token"],
            "XSTS"
        );

        let request = &transport.requests()[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.body, br#"{"a":1}"#);
        assert_eq!(
            request.headers,
            vec![
                ("x-xbl-contract-version".to_owned(), "1".to_owned()),
                ("Accept".to_owned(), "application/json".to_owned()),
                ("Content-Type".to_owned(), "application/json".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn post_raw_sets_no_content_type() {
        let transport = FakeTransport::new();
        transport.enqueue_json("/ice", r#"{}"#, 200);
        transport
            .post_raw(
                "https://host/sessions/cloud/ABC/ice",
                b"raw".to_vec(),
                vec![],
            )
            .await
            .unwrap();
        let request = &transport.requests()[0];
        assert_eq!(request.method, "POST");
        assert_eq!(request.body, b"raw");
        assert!(request.headers.is_empty());
    }

    #[tokio::test]
    async fn fake_is_usable_through_dyn_and_arc() {
        let transport: std::sync::Arc<dyn HttpTransport> =
            std::sync::Arc::new(FakeTransport::new());
        let err = transport.get("https://host/x", vec![]).await.unwrap_err();
        assert!(matches!(err, Error::Http(_)));
    }

    #[test]
    fn reqwest_transport_builds() {
        let transport = ReqwestTransport::new().unwrap();
        let _client = transport.client();
    }

    #[test]
    fn accept_language_follows_qt_prepare_request_rules() {
        assert_eq!(accept_language("C"), "en,*");
        assert_eq!(accept_language("en_US"), "en-US,*");
        assert_eq!(accept_language("en_GB"), "en-GB,*");
        assert_eq!(accept_language("de_DE"), "de-DE,en,*");
        assert_eq!(accept_language("ja_JP"), "ja-JP,en,*");
        // Qt only maps a bare "C"; "en" alone is neither "C" nor "en-".
        assert_eq!(accept_language("en"), "en,en,*");
        // Whatever the host locale is, the value is well formed and ends in the wildcard.
        let system = system_accept_language();
        assert!(system.ends_with(",*"), "{system}");
        assert!(!system.contains('_'), "{system}");
    }

    /// One step of a scripted server response.
    enum Step {
        Write(&'static [u8]),
        Sleep(Duration),
    }

    /// Starts a one-shot HTTP/1.1 server on a loopback port. It reads the request head,
    /// plays `script`, then closes. Returns the base URL and the handle that yields the
    /// raw request bytes.
    fn scripted_server(script: Vec<Step>) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            for step in script {
                match step {
                    // Write errors are expected once the client has given up.
                    Step::Write(bytes) => {
                        if stream.write_all(bytes).is_err() {
                            break;
                        }
                        let _ = stream.flush();
                    }
                    Step::Sleep(duration) => std::thread::sleep(duration),
                }
            }
            request
        });
        (url, handle)
    }

    fn header_values(raw_request: &[u8], name: &str) -> Vec<String> {
        String::from_utf8_lossy(raw_request)
            .lines()
            .filter_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.trim()
                    .eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_owned())
            })
            .collect()
    }

    fn local_transport() -> ReqwestTransport {
        ReqwestTransport::with_client(ReqwestTransport::builder().no_proxy().build().unwrap())
    }

    const OK_HEAD: &[u8] =
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\n";

    #[tokio::test]
    async fn reqwest_transport_sends_qt_default_headers() {
        let (url, server) = scripted_server(vec![Step::Write(OK_HEAD), Step::Write(b"ok")]);
        let response = local_transport()
            .get(&format!("{url}/v2/titles"), vec![])
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body_text(), "ok");

        let request = server.join().unwrap();
        assert_eq!(
            header_values(&request, "User-Agent"),
            vec![DEFAULT_USER_AGENT.to_owned()],
            "{}",
            String::from_utf8_lossy(&request)
        );
        assert_eq!(
            header_values(&request, "Accept-Language"),
            vec![system_accept_language()]
        );
        assert_eq!(
            header_values(&request, "Connection"),
            vec!["Keep-Alive".to_owned()]
        );
    }

    #[tokio::test]
    async fn reqwest_transport_request_headers_replace_defaults() {
        let (url, server) = scripted_server(vec![Step::Write(OK_HEAD), Step::Write(b"ok")]);
        let chrome_ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                         (KHTML, like Gecko) Chrome/119.0.0.0 Safari/537.36";
        local_transport()
            .get(
                &format!("{url}/catalog"),
                vec![
                    ("User-Agent".to_owned(), chrome_ua.to_owned()),
                    ("Accept-Language".to_owned(), "fr-FR".to_owned()),
                ],
            )
            .await
            .unwrap();

        let request = server.join().unwrap();
        assert_eq!(
            header_values(&request, "User-Agent"),
            vec![chrome_ua.to_owned()]
        );
        assert_eq!(
            header_values(&request, "Accept-Language"),
            vec!["fr-FR".to_owned()]
        );
    }

    #[tokio::test]
    async fn reqwest_transport_timeout_is_inactivity_not_total_deadline() {
        // 8 bytes at 100 ms intervals: ~800 ms total, never more than 100 ms idle.
        let mut script = vec![Step::Write(
            b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n",
        )];
        for byte in [b"a", b"b", b"c", b"d", b"e", b"f", b"g", b"h"] {
            script.push(Step::Sleep(Duration::from_millis(100)));
            script.push(Step::Write(byte));
        }
        let (url, server) = scripted_server(script);
        let started = std::time::Instant::now();
        let response = local_transport()
            .send(HttpRequest {
                url: format!("{url}/slow"),
                timeout: Duration::from_millis(400),
                ..HttpRequest::default()
            })
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body_text(), "abcdefgh");
        assert!(
            started.elapsed() > Duration::from_millis(400),
            "the whole transfer must have outlived the timeout value: {:?}",
            started.elapsed()
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn reqwest_transport_stalled_body_times_out_after_inactivity() {
        let (url, server) = scripted_server(vec![
            Step::Write(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n"),
            Step::Write(b"ab"),
            Step::Sleep(Duration::from_secs(3)),
            Step::Write(b"cd"),
        ]);
        let started = std::time::Instant::now();
        let err = local_transport()
            .send(HttpRequest {
                url: format!("{url}/stall"),
                timeout: Duration::from_millis(300),
                ..HttpRequest::default()
            })
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Timeout(msg) if msg == "Request timed out"),
            "{err}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "must abort ~300 ms after the last byte, not wait for the server: {:?}",
            started.elapsed()
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn reqwest_transport_stalled_headers_time_out() {
        let (url, server) = scripted_server(vec![
            Step::Sleep(Duration::from_secs(3)),
            Step::Write(OK_HEAD),
            Step::Write(b"ok"),
        ]);
        let err = local_transport()
            .send(HttpRequest {
                url: format!("{url}/never"),
                timeout: Duration::from_millis(300),
                ..HttpRequest::default()
            })
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Timeout(_)), "{err}");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn reqwest_transport_zero_timeout_disables_the_timer() {
        let (url, server) = scripted_server(vec![
            Step::Write(OK_HEAD),
            Step::Sleep(Duration::from_millis(300)),
            Step::Write(b"ok"),
        ]);
        let response = local_transport()
            .send(HttpRequest {
                url: format!("{url}/untimed"),
                timeout: Duration::ZERO,
                ..HttpRequest::default()
            })
            .await
            .unwrap();
        assert_eq!(response.body_text(), "ok");
        server.join().unwrap();
    }
}
