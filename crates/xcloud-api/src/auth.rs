//! Port of qt/src/auth/MicrosoftAuthService.{h,cpp}. The qt/src/auth/AuthTypes.h value
//! types (`DeviceCodeInfo`, `XboxToken`, `LoginRegion`, `StreamTokens`, `preferred_region`)
//! live in [`xcloud_proto::auth_types`] and are re-exported here.
//!
//! Device-code sign-in, Xbox Live user token, XSTS tokens for the xboxlive and GSSV
//! relying parties, the GSSV offering logins (xhome, xgpuweb, xgpuwebf2p) and the
//! Passport console-transfer token (LPT). Every URL, header, form field and JSON key is the
//! one the Qt client sends.
//!
//! Differences from the C++:
//! - `QFuture<Result<T>>` became `async fn -> Result<T>`. Transport failures (no HTTP
//!   status at all) keep their `Error::Http`/`Error::Timeout` variant, with the C++ context
//!   string prepended; every other failure is `Error::Auth` carrying the C++ message.
//!   The device-code poll uses the variant, not a string prefix, to tell transient
//!   network errors from OAuth errors. Like the C++ it keeps polling only through
//!   `authorization_pending` and network failures; every other OAuth error, `slow_down`
//!   included, aborts the sign-in.
//! - `get_xsts_token` returns the XSTS token together with the `uhs` display claim, and
//!   `exchange_for_stream_tokens` fills `StreamTokens::web_token_uhs` from the web token
//!   like the Swift original does; the C++ left that field empty. The persisted bundle
//!   also includes the web token (the C++ saved the bundle before merging it in).
//! - `TokenStore` calls return `Result` (keychain-backed), so they are propagated.
//!
//! Secrets (access, refresh, user, XSTS, gs and LPT tokens) are never logged.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::token_store::TokenStore;
use crate::transport::{Headers, HttpResponse, HttpTransport};
use crate::{Error, Result};
pub use xcloud_proto::auth_types::{
    DeviceCodeInfo, LoginRegion, StreamTokens, XboxToken, preferred_region,
};

const LOG_TARGET: &str = "Auth";

/// Entra public client used by xal-node / Stratix (`MicrosoftAuthService::clientId`).
pub const CLIENT_ID: &str = "1f907974-e22b-4810-a9de-d9647380c97e";
/// `MicrosoftAuthService::scope`.
pub const SCOPE: &str = "xboxlive.signin openid profile offline_access";
/// `MicrosoftAuthService::lptScope`.
pub const LPT_SCOPE: &str =
    "service::http://Passport.NET/purpose::PURPOSE_XBOX_CLOUD_CONSOLE_TRANSFER_TOKEN";

/// Relying party of the XSTS token that the GSSV offering login takes
/// (`getXstsToken`'s default argument).
pub const GSSV_RELYING_PARTY: &str = "http://gssv.xboxlive.com/";
/// Relying party of the XSTS "web" token used for Xbox Live web APIs (`getWebToken`).
pub const XBOX_LIVE_RELYING_PARTY: &str = "http://xboxlive.com";

/// `pollForMsaToken`'s default `intervalSeconds`.
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;
/// `pollForMsaToken`'s default `maxWaitSeconds`.
pub const DEFAULT_POLL_MAX_WAIT_SECS: u64 = 900;

const DEVICE_CODE_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const LIVE_TOKEN_URL: &str = "https://login.live.com/oauth20_token.srf";
const XBOX_USER_AUTH_URL: &str = "https://user.auth.xboxlive.com/user/authenticate";
const XSTS_AUTH_URL: &str = "https://xsts.auth.xboxlive.com/xsts/authorize";

/// Outcome of one device-code token request, before it is flattened into a `Result`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DeviceCodePoll {
    Token(String),
    /// `authorization_pending`: the user has not finished signing in yet.
    Pending,
    /// Any other OAuth `error` code (`slow_down`, `expired_token`,
    /// `authorization_declined`, ...). The poll aborts on these, as the C++ does.
    OAuthError(String),
}

/// Result of one GSSV offering login (`gssvLogin`): gsToken, base host, regions.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GssvLogin {
    gs_token: String,
    base_host: String,
    regions: Vec<LoginRegion>,
}

/// Owns Microsoft device-code sign-in, Xbox Live token exchange and stream-token
/// acquisition. Mirrors the Swift MicrosoftAuthService.
#[derive(Clone)]
pub struct MicrosoftAuthService {
    transport: Arc<dyn HttpTransport>,
    store: Arc<dyn TokenStore>,
}

impl std::fmt::Debug for MicrosoftAuthService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrosoftAuthService")
            .finish_non_exhaustive()
    }
}

impl MicrosoftAuthService {
    pub fn new(transport: Arc<dyn HttpTransport>, store: Arc<dyn TokenStore>) -> Self {
        Self { transport, store }
    }

    pub fn token_store(&self) -> &Arc<dyn TokenStore> {
        &self.store
    }

    pub fn transport(&self) -> &Arc<dyn HttpTransport> {
        &self.transport
    }

    /// Step 1: request a device code for browser sign-in.
    pub async fn request_device_code(&self) -> Result<DeviceCodeInfo> {
        let response = self
            .post_form(
                DEVICE_CODE_URL,
                &[("client_id", CLIENT_ID), ("scope", SCOPE)],
            )
            .await
            .map_err(|e| with_context("Network error", e))?;
        if response.status != 200 {
            return Err(Error::Auth(format!(
                "Device code request failed HTTP {}: {}",
                response.status,
                summarize_body(&response.body)
            )));
        }
        let object = parse_json_object(&response.body);
        let info = DeviceCodeInfo::from_json(&object);
        if info.device_code.is_empty() {
            return Err(Error::Auth(format!(
                "Invalid response: {}",
                summarize_body(&response.body)
            )));
        }
        tracing::debug!(
            target: LOG_TARGET,
            expires_in = info.expires_in,
            interval = info.interval,
            "device code issued"
        );
        Ok(info)
    }

    /// One token-endpoint request for `device_code`. On success the MSA access token
    /// (and a refresh token when present) are stored. Exposed for direct unit testing;
    /// `oauth:<error>` failures (`authorization_pending`, `slow_down`, `expired_token`, ...)
    /// surface as `Error::Auth`.
    pub async fn exchange_device_code(&self, device_code: &str) -> Result<String> {
        match self.try_exchange_device_code(device_code).await? {
            DeviceCodePoll::Token(token) => Ok(token),
            DeviceCodePoll::Pending => Err(Error::Auth("oauth:authorization_pending".to_owned())),
            DeviceCodePoll::OAuthError(error) => Err(Error::Auth(format!("oauth:{error}"))),
        }
    }

    async fn try_exchange_device_code(&self, device_code: &str) -> Result<DeviceCodePoll> {
        let response = self
            .post_form(
                TOKEN_URL,
                &[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", CLIENT_ID),
                    ("device_code", device_code),
                ],
            )
            .await?;

        let object = parse_json_object(&response.body);
        let error = json_string(&object, "error");
        if response.status != 200 && !error.is_empty() {
            return Ok(match error.as_str() {
                "authorization_pending" => DeviceCodePoll::Pending,
                _ => DeviceCodePoll::OAuthError(error),
            });
        }
        if response.status != 200 {
            return Err(Error::Auth(format!(
                "HTTP {}: {}",
                response.status,
                summarize_body(&response.body)
            )));
        }

        let access_token = json_string(&object, "access_token");
        if access_token.is_empty() {
            return Err(Error::Auth(format!(
                "Invalid response: {}",
                summarize_body(&response.body)
            )));
        }
        self.store.save_msa_token(&access_token)?;
        let refresh = json_string(&object, "refresh_token");
        if !refresh.is_empty() {
            self.store.save_refresh_token(&refresh)?;
        }
        tracing::info!(target: LOG_TARGET, "device code authorized");
        Ok(DeviceCodePoll::Token(access_token))
    }

    /// Step 2: poll the token endpoint until the user authorizes or the code expires.
    /// Waits `interval_seconds` before the first and between later attempts, keeps going
    /// through `authorization_pending` and transient network failures, and gives up with
    /// "device code expired" once `max_wait_seconds` have passed. Any other OAuth error
    /// (`slow_down`, `expired_token`, `authorization_declined`, ...) aborts the poll with
    /// `oauth:<error>`, exactly as `pollForMsaToken` does.
    pub async fn poll_for_msa_token(
        &self,
        device_code: &str,
        interval_seconds: u64,
        max_wait_seconds: u64,
    ) -> Result<String> {
        self.poll_for_msa_token_with(
            device_code,
            Duration::from_secs(interval_seconds.max(1)),
            Duration::from_secs(max_wait_seconds.max(1)),
        )
        .await
    }

    /// [`Self::poll_for_msa_token`] with arbitrary durations (sub-second intervals for
    /// tests). Both are clamped to at least one millisecond.
    pub async fn poll_for_msa_token_with(
        &self,
        device_code: &str,
        interval: Duration,
        max_wait: Duration,
    ) -> Result<String> {
        let interval = interval.max(Duration::from_millis(1));
        let deadline = Instant::now() + max_wait.max(Duration::from_millis(1));

        loop {
            tokio::time::sleep(interval).await;
            if Instant::now() >= deadline {
                return Err(Error::Auth("device code expired".to_owned()));
            }
            match self.try_exchange_device_code(device_code).await {
                Ok(DeviceCodePoll::Token(token)) => return Ok(token),
                Ok(DeviceCodePoll::Pending) => {}
                Ok(DeviceCodePoll::OAuthError(error)) => {
                    return Err(Error::Auth(format!("oauth:{error}")));
                }
                // Keep polling through transient failures (app suspended, Wi-Fi handoff).
                Err(error) if is_transient(&error) => {
                    tracing::warn!(
                        target: LOG_TARGET,
                        error = %error,
                        "device code poll network error, retrying"
                    );
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Exchanges a stored MSA refresh token for a new access token (and stores both).
    pub async fn refresh_msa_token(&self, refresh_token: &str) -> Result<String> {
        let response = self
            .post_form(
                TOKEN_URL,
                &[
                    ("client_id", CLIENT_ID),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token),
                    ("scope", SCOPE),
                ],
            )
            .await
            .map_err(|e| with_context("Network error", e))?;
        if response.status != 200 {
            return Err(Error::Auth(format!(
                "Refresh HTTP {}: {}",
                response.status,
                summarize_body(&response.body)
            )));
        }
        let object = parse_json_object(&response.body);
        let access_token = json_string(&object, "access_token");
        if access_token.is_empty() {
            return Err(Error::Auth("Invalid refresh response".to_owned()));
        }
        self.store.save_msa_token(&access_token)?;
        let new_refresh = json_string(&object, "refresh_token");
        if !new_refresh.is_empty() {
            self.store.save_refresh_token(&new_refresh)?;
        }
        tracing::debug!(target: LOG_TARGET, "MSA token refreshed");
        Ok(access_token)
    }

    /// Exchanges `refresh_token` for the Passport console-transfer token (the xCloud
    /// `/connect` "Long Play Token"), storing it with its expiry and rotating the refresh
    /// token when Microsoft issued a new one.
    pub async fn fetch_lpt(&self, refresh_token: &str) -> Result<String> {
        let response = self
            .post_form(
                LIVE_TOKEN_URL,
                &[
                    ("client_id", CLIENT_ID),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh_token),
                    ("scope", LPT_SCOPE),
                ],
            )
            .await
            .map_err(|e| with_context("Network error", e))?;
        if response.status != 200 {
            return Err(Error::Auth(format!(
                "LPT HTTP {}: {}",
                response.status,
                summarize_body(&response.body)
            )));
        }
        let object = parse_json_object(&response.body);
        let token = json_string(&object, "access_token");
        if token.is_empty() {
            return Err(Error::Auth("Invalid LPT response".to_owned()));
        }
        let expires_in = json_i64(&object, "expires_in", 0);
        let expires_at =
            (expires_in > 0).then(|| Utc::now() + chrono::Duration::seconds(expires_in));
        self.store.save_lpt_token(&token, expires_at)?;
        let new_refresh = json_string(&object, "refresh_token");
        if !new_refresh.is_empty() {
            self.store.save_refresh_token(&new_refresh)?;
        }
        tracing::debug!(target: LOG_TARGET, expires_in, "LPT acquired");
        Ok(token)
    }

    /// Exchange the stored refresh token for the Passport console-transfer token.
    pub async fn fetch_lpt_for_cloud_connect(&self) -> Result<String> {
        let refresh_token = self.store.load_refresh_token()?;
        if refresh_token.is_empty() {
            return Err(Error::Auth("No stream token available.".to_owned()));
        }
        self.fetch_lpt(&refresh_token).await
    }

    /// Step 3a: MSA access token -> Xbox Live user token (`user.auth.xboxlive.com`).
    pub async fn get_xbox_user_token(&self, msa_access_token: &str) -> Result<XboxToken> {
        let body = json!({
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT",
            "Properties": {
                "AuthMethod": "RPS",
                "SiteName": "user.auth.xboxlive.com",
                "RpsTicket": format!("d={msa_access_token}"),
            },
        });
        let object = self
            .post_xbox_json(XBOX_USER_AUTH_URL, &body, "Xbox user authenticate")
            .await
            .map_err(|e| with_context("Token exchange failed", e))?;
        let token = xbox_token_from(&object);
        if token.token.is_empty() {
            return Err(Error::Auth("Invalid response: missing Token".to_owned()));
        }
        tracing::debug!(target: LOG_TARGET, "Xbox user token acquired");
        Ok(token)
    }

    /// Step 3b: Xbox user token -> XSTS token for `relying_party`
    /// ([`GSSV_RELYING_PARTY`] for stream tokens, [`XBOX_LIVE_RELYING_PARTY`] for the web
    /// token). Returns the token with its `uhs` display claim.
    pub async fn get_xsts_token(&self, user_token: &str, relying_party: &str) -> Result<XboxToken> {
        let body = json!({
            "RelyingParty": relying_party,
            "TokenType": "JWT",
            "Properties": {
                "UserTokens": [user_token],
                "SandboxId": "RETAIL",
            },
        });
        let object = self
            .post_xbox_json(XSTS_AUTH_URL, &body, "XSTS authorize")
            .await
            .map_err(|e| with_context("Token exchange failed", e))?;
        let token = xbox_token_from(&object);
        if token.token.is_empty() {
            return Err(Error::Auth("Invalid XSTS response".to_owned()));
        }
        tracing::debug!(target: LOG_TARGET, relying_party, "XSTS token acquired");
        Ok(token)
    }

    /// XSTS token for the Xbox Live web APIs (`getWebToken`).
    pub async fn get_web_token(&self, user_token: &str) -> Result<XboxToken> {
        self.get_xsts_token(user_token, XBOX_LIVE_RELYING_PARTY)
            .await
    }

    /// Step 4: GSSV XSTS token -> gsTokens. xhome is required; xgpuweb and xgpuwebf2p are
    /// best effort and run concurrently. The bundle is persisted for the next launch.
    pub async fn get_stream_token_bundle(&self, xsts_token: &str) -> Result<StreamTokens> {
        let home = self
            .gssv_login(xsts_token, "xhome")
            .await
            .map_err(|e| with_context("Token exchange failed", e))?;
        let mut tokens = StreamTokens {
            xhome_token: home.gs_token,
            xhome_host: home.base_host,
            ..StreamTokens::default()
        };

        let (cloud, f2p) = tokio::join!(
            self.gssv_login(xsts_token, "xgpuweb"),
            self.gssv_login(xsts_token, "xgpuwebf2p"),
        );
        match cloud {
            Ok(cloud) => {
                tokens.xcloud_token = cloud.gs_token;
                tokens.xcloud_host = cloud.base_host;
                tokens.xcloud_regions = cloud.regions;
            }
            Err(error) => {
                tracing::info!(target: LOG_TARGET, error = %error, "xgpuweb offering unavailable");
            }
        }
        match f2p {
            Ok(f2p) => {
                tokens.xcloud_f2p_token = f2p.gs_token;
                tokens.xcloud_f2p_host = f2p.base_host;
                if tokens.xcloud_regions.is_empty() {
                    tokens.xcloud_regions = f2p.regions;
                }
            }
            Err(error) => {
                tracing::info!(target: LOG_TARGET, error = %error, "xgpuwebf2p offering unavailable");
            }
        }

        self.store.save_stream_tokens(&tokens)?;
        Ok(tokens)
    }

    /// Step 3: exchange an MSA access token for the full set of stream tokens. The web
    /// token is best effort; its failure never blocks stream token acquisition.
    pub async fn exchange_for_stream_tokens(&self, msa_access_token: &str) -> Result<StreamTokens> {
        let user = self.get_xbox_user_token(msa_access_token).await?;
        let web = match self.get_web_token(&user.token).await {
            Ok(web) => Some(web),
            Err(error) => {
                tracing::warn!(target: LOG_TARGET, error = %error, "web token unavailable");
                None
            }
        };
        let xsts = self.get_xsts_token(&user.token, GSSV_RELYING_PARTY).await?;
        let mut tokens = self.get_stream_token_bundle(&xsts.token).await?;
        if let Some(web) = web {
            tokens.web_token = web.token;
            tokens.web_token_uhs = web.user_hash;
            self.store.save_stream_tokens(&tokens)?;
        }
        Ok(tokens)
    }

    /// Re-derive stream tokens from the stored refresh token.
    pub async fn refresh_stream_tokens(&self) -> Result<StreamTokens> {
        let refresh_token = self.store.load_refresh_token()?;
        if refresh_token.is_empty() {
            return Err(Error::Auth("No stream token available.".to_owned()));
        }
        let msa_token = self.refresh_msa_token(&refresh_token).await?;
        self.exchange_for_stream_tokens(&msa_token).await
    }

    /// The persisted bundle from the last exchange (`restoreStreamTokens`).
    pub fn restore_stream_tokens(&self) -> Result<StreamTokens> {
        self.store.load_stream_tokens()
    }

    /// True when a refresh token is stored (`hasStoredSession`).
    pub fn has_stored_session(&self) -> Result<bool> {
        Ok(!self.store.load_refresh_token()?.is_empty())
    }

    /// Forgets every stored credential (`clearSession`, Swift `signOut`).
    pub fn clear_session(&self) -> Result<()> {
        self.store.clear_all()
    }

    /// POSTs a form body (`postForm` helper): `Accept: application/json` plus the
    /// `Content-Type` the transport adds.
    async fn post_form(&self, url: &str, fields: &[(&str, &str)]) -> Result<HttpResponse> {
        let headers: Headers = vec![("Accept".to_owned(), "application/json".to_owned())];
        self.transport
            .post_form(url, form_encode(fields), headers)
            .await
    }

    /// POSTs JSON with the Xbox Live contract headers and decodes the object response
    /// (`postXboxJson`).
    async fn post_xbox_json(&self, url: &str, body: &Value, what: &str) -> Result<Value> {
        let payload = serde_json::to_vec(body)?;
        let headers: Headers = vec![
            ("Accept".to_owned(), "application/json".to_owned()),
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("x-xbl-contract-version".to_owned(), "1".to_owned()),
        ];
        let response = self
            .transport
            .post_raw(url, payload, headers)
            .await
            .map_err(|e| with_context(what, e))?;
        if !response.is_success() {
            return Err(Error::Auth(format!(
                "{what}: HTTP {}: {}",
                response.status,
                summarize_body(&response.body)
            )));
        }
        Ok(parse_json_object(&response.body))
    }

    /// Performs the GSSV offering login used for xhome / xgpuweb / xgpuwebf2p
    /// (`gssvLogin`). The XSTS token travels in the body, not an `Authorization` header.
    async fn gssv_login(&self, xsts_token: &str, offering: &str) -> Result<GssvLogin> {
        let host = format!("{offering}.gssv-play-prod.xboxlive.com");
        let url = format!("https://{host}/v2/login/user");
        let payload = serde_json::to_vec(&json!({
            "token": xsts_token,
            "offeringId": offering,
        }))?;
        let headers: Headers = vec![
            ("Accept".to_owned(), "application/json".to_owned()),
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("x-gssv-client".to_owned(), "XboxComBrowser".to_owned()),
            ("x-xbl-contract-version".to_owned(), "1".to_owned()),
        ];
        let response = self
            .transport
            .post_raw(&url, payload, headers)
            .await
            .map_err(|e| with_context("Stream token", e))?;
        if response.status != 200 {
            return Err(Error::Auth(format!(
                "Stream token HTTP {}: {}",
                response.status,
                summarize_body(&response.body)
            )));
        }

        let object = parse_json_object(&response.body);
        let gs_token = json_string(&object, "gsToken");
        let regions = parse_regions(&object);

        let mut base_host = format!("https://{host}");
        if !regions.is_empty() {
            let region = preferred_region(&regions, "");
            if !region.base_uri.is_empty() {
                base_host = region.base_uri;
            }
        }
        let base_host = base_host.trim_end_matches('/').to_owned();

        if gs_token.is_empty() {
            return Err(Error::Auth("Stream token: missing gsToken".to_owned()));
        }
        tracing::debug!(
            target: LOG_TARGET,
            offering,
            host = %base_host,
            regions = regions.len(),
            "GSSV login succeeded"
        );
        Ok(GssvLogin {
            gs_token,
            base_host,
            regions,
        })
    }
}

/// `offeringSettings.regions`, skipping entries without a `baseUri` and deriving a
/// missing `name` from the first host label of the URI.
fn parse_regions(object: &Value) -> Vec<LoginRegion> {
    let Some(raw_regions) = object
        .get("offeringSettings")
        .and_then(|settings| settings.get("regions"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    raw_regions
        .iter()
        .filter_map(|value| {
            let uri = json_string(value, "baseUri");
            if uri.is_empty() {
                return None;
            }
            let mut name = json_string(value, "name");
            if name.is_empty() {
                name = url::Url::parse(&uri)
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_owned))
                    .and_then(|host| host.split('.').next().map(str::to_owned))
                    .unwrap_or_default();
            }
            Some(LoginRegion {
                name,
                base_uri: uri,
                is_default: value
                    .get("isDefault")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// `Token` plus `DisplayClaims.xui[0].uhs` from an Xbox Live token response.
fn xbox_token_from(object: &Value) -> XboxToken {
    let user_hash = object
        .get("DisplayClaims")
        .and_then(|claims| claims.get("xui"))
        .and_then(Value::as_array)
        .and_then(|xui| xui.first())
        .map(|first| json_string(first, "uhs"))
        .unwrap_or_default();
    XboxToken {
        token: json_string(object, "Token"),
        user_hash,
    }
}

/// Transport-level failures that the device-code poll should retry.
fn is_transient(error: &Error) -> bool {
    matches!(error, Error::Http(_) | Error::Timeout(_))
}

/// Prepends `context` to a transport error's message while keeping its variant, so
/// callers can still tell network failures apart.
fn with_context(context: &str, error: Error) -> Error {
    match error {
        Error::Http(message) => Error::Http(format!("{context}: {message}")),
        Error::Timeout(message) => Error::Timeout(format!("{context}: {message}")),
        other => other,
    }
}

/// `QUrlQuery::toString(QUrl::FullyEncoded)`: pairs joined by `&`/`=`, each key and value
/// percent-encoded the way Qt does it. Unreserved characters and the RFC 3986 delimiters
/// that are legal inside a query (`!$'()*+,;:/?@[]`) stay as they are; space, `&`, `=`,
/// `#`, `%` and everything else (including non-ASCII, as UTF-8) is percent-encoded.
pub fn form_encode(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, (key, value)) in pairs.iter().enumerate() {
        if index > 0 {
            out.push(b'&');
        }
        qt_percent_encode(key, &mut out);
        out.push(b'=');
        qt_percent_encode(value, &mut out);
    }
    out
}

fn qt_percent_encode(text: &str, out: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in text.bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b':'
                    | b'/'
                    | b'?'
                    | b'@'
                    | b'['
                    | b']'
            );
        if keep {
            out.push(byte);
        } else {
            out.push(b'%');
            out.push(HEX[usize::from(byte >> 4)]);
            out.push(HEX[usize::from(byte & 0x0f)]);
        }
    }
}

/// `parseJsonObject`: the body as a JSON object, or an empty object when it is not one.
fn parse_json_object(body: &[u8]) -> Value {
    match serde_json::from_slice::<Value>(body) {
        Ok(value) if value.is_object() => value,
        _ => Value::Object(serde_json::Map::new()),
    }
}

/// `QJsonValue::toString()`: a missing or non-string value reads as empty.
fn json_string(object: &Value, key: &str) -> String {
    object
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// `QJsonValue::toInt(default)`.
fn json_i64(object: &Value, key: &str, default: i64) -> i64 {
    object.get(key).and_then(Value::as_i64).unwrap_or(default)
}

/// `summarizeBody`: the first 512 characters of the body, or `<empty>`.
fn summarize_body(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    if text.is_empty() {
        "<empty>".to_owned()
    } else {
        text.chars().take(512).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token_store::MemoryTokenStore;
    use crate::transport::FakeTransport;

    struct Harness {
        transport: Arc<FakeTransport>,
        store: Arc<MemoryTokenStore>,
        service: MicrosoftAuthService,
    }

    fn harness() -> Harness {
        let transport = Arc::new(FakeTransport::new());
        let store = Arc::new(MemoryTokenStore::new());
        let service = MicrosoftAuthService::new(transport.clone(), store.clone());
        Harness {
            transport,
            store,
            service,
        }
    }

    fn body_text(transport: &FakeTransport, index: usize) -> String {
        transport.requests()[index].body_text()
    }

    #[tokio::test]
    async fn device_code_request_and_parse() {
        let h = harness();
        h.transport.enqueue_json(
            "devicecode",
            r#"{"device_code":"DEV","user_code":"ABCD-EFGH","verification_uri":"https://microsoft.com/devicelogin","verification_uri_complete":"https://microsoft.com/devicelogin?otc=ABCD-EFGH","expires_in":900,"interval":5}"#,
            200,
        );

        let info = h.service.request_device_code().await.unwrap();
        assert_eq!(info.user_code, "ABCD-EFGH");
        assert_eq!(info.device_code, "DEV");
        assert_eq!(
            info.sign_in_url(),
            "https://microsoft.com/devicelogin?otc=ABCD-EFGH"
        );
        assert_eq!(info.expires_in, 900);
        assert_eq!(info.interval, 5);

        assert_eq!(h.transport.request_count(), 1);
        let request = &h.transport.requests()[0];
        assert!(request.url.contains("oauth2/v2.0/devicecode"));
        assert_eq!(request.method, "POST");
        let body = request.body_text();
        assert!(body.contains("client_id=1f907974-e22b-4810-a9de-d9647380c97e"));
        assert!(body.contains("scope=xboxlive.signin"));
        assert!(
            request
                .header("Content-Type")
                .unwrap()
                .contains("x-www-form-urlencoded")
        );
        assert_eq!(request.header("Accept"), Some("application/json"));
    }

    #[tokio::test]
    async fn device_code_handles_pending() {
        let h = harness();
        h.transport.enqueue_json(
            "oauth2/v2.0/token",
            r#"{"error":"authorization_pending","error_description":"wait"}"#,
            400,
        );

        let err = h.service.exchange_device_code("DEV").await.unwrap_err();
        assert!(matches!(&err, Error::Auth(_)), "{err}");
        assert!(err.to_string().contains("authorization_pending"), "{err}");
        assert_eq!(h.store.load_msa_token().unwrap(), "");
    }

    #[tokio::test]
    async fn device_code_handles_expired() {
        let h = harness();
        h.transport
            .enqueue_json("oauth2/v2.0/token", r#"{"error":"expired_token"}"#, 400);

        let err = h.service.exchange_device_code("DEV").await.unwrap_err();
        assert!(matches!(&err, Error::Auth(_)), "{err}");
        assert!(err.to_string().contains("expired_token"), "{err}");
    }

    #[tokio::test]
    async fn xbox_user_token_exchange() {
        let h = harness();
        h.transport.enqueue_json(
            "user.auth.xboxlive.com",
            r#"{"Token":"USERTOKEN","DisplayClaims":{"xui":[{"uhs":"UHASH"}]}}"#,
            200,
        );

        let token = h.service.get_xbox_user_token("MSA").await.unwrap();
        assert_eq!(token.token, "USERTOKEN");
        assert_eq!(token.user_hash, "UHASH");

        let request = &h.transport.requests()[0];
        assert_eq!(
            request.url,
            "https://user.auth.xboxlive.com/user/authenticate"
        );
        let body = request.body_text();
        assert!(body.contains(r#""RpsTicket":"d=MSA""#), "{body}");
        assert!(
            body.contains(r#""RelyingParty":"http://auth.xboxlive.com""#),
            "{body}"
        );
        assert!(body.contains(r#""AuthMethod":"RPS""#), "{body}");
        assert!(
            body.contains(r#""SiteName":"user.auth.xboxlive.com""#),
            "{body}"
        );
        assert!(body.contains(r#""TokenType":"JWT""#), "{body}");
        assert_eq!(request.header("x-xbl-contract-version"), Some("1"));
        assert_eq!(request.header("Content-Type"), Some("application/json"));
        assert_eq!(request.header("Accept"), Some("application/json"));
    }

    #[tokio::test]
    async fn xsts_exchange() {
        let h = harness();
        h.transport.enqueue_json(
            "xsts.auth.xboxlive.com",
            r#"{"Token":"XSTSTOKEN","DisplayClaims":{"xui":[{"uhs":"UH"}]}}"#,
            200,
        );

        let token = h
            .service
            .get_xsts_token("USERTOKEN", GSSV_RELYING_PARTY)
            .await
            .unwrap();
        assert_eq!(token.token, "XSTSTOKEN");
        assert_eq!(token.user_hash, "UH");

        let request = &h.transport.requests()[0];
        assert_eq!(request.url, "https://xsts.auth.xboxlive.com/xsts/authorize");
        let body = request.body_text();
        assert!(body.contains(r#""UserTokens":["USERTOKEN"]"#), "{body}");
        assert!(body.contains(r#""SandboxId":"RETAIL""#), "{body}");
        assert!(
            body.contains(r#""RelyingParty":"http://gssv.xboxlive.com/""#),
            "{body}"
        );
    }

    #[tokio::test]
    async fn lpt_exchange_saves_token() {
        let h = harness();
        h.transport.enqueue_json(
            "oauth20_token.srf",
            r#"{"access_token":"LPTTOKEN","refresh_token":"NEWREFRESH","expires_in":3600}"#,
            200,
        );

        let token = h.service.fetch_lpt("REFRESH").await.unwrap();
        assert_eq!(token, "LPTTOKEN");
        assert_eq!(h.store.load_lpt_token().unwrap(), "LPTTOKEN");
        assert_eq!(h.store.load_refresh_token().unwrap(), "NEWREFRESH");
        assert!(h.store.lpt_token_is_fresh().unwrap());

        let request = &h.transport.requests()[0];
        assert_eq!(request.url, "https://login.live.com/oauth20_token.srf");
        let body = request.body_text();
        assert!(body.contains("scope=service"), "{body}");
        assert!(
            body.contains("PURPOSE_XBOX_CLOUD_CONSOLE_TRANSFER_TOKEN"),
            "{body}"
        );
        assert!(body.contains("grant_type=refresh_token"), "{body}");
        assert!(body.contains("refresh_token=REFRESH"), "{body}");
    }

    #[tokio::test]
    async fn gssv_login_parses_regions() {
        let h = harness();
        h.transport.enqueue_json(
            "xhome.gssv-play-prod.xboxlive.com",
            r#"{"gsToken":"XHOMEGS","offeringSettings":{"regions":[{"name":"eastus","baseUri":"https://eastus.gssv-play-prod.xboxlive.com/","isDefault":false},{"name":"westus","baseUri":"https://westus.gssv-play-prod.xboxlive.com","isDefault":true}]}}"#,
            200,
        );
        h.transport.enqueue_json(
            "xgpuweb.gssv-play-prod.xboxlive.com",
            r#"{"gsToken":"CLOUDGS","offeringSettings":{"regions":[{"name":"westus","baseUri":"https://westus.gssv-play-prod.xboxlive.com","isDefault":true}]}}"#,
            200,
        );
        h.transport
            .enqueue_error("xgpuwebf2p.gssv-play-prod.xboxlive.com", "not entitled");

        let tokens = h.service.get_stream_token_bundle("XSTS").await.unwrap();
        assert_eq!(tokens.xhome_token, "XHOMEGS");
        assert_eq!(
            tokens.xhome_host,
            "https://westus.gssv-play-prod.xboxlive.com"
        );
        assert_eq!(tokens.xcloud_token, "CLOUDGS");
        assert_eq!(
            tokens.xcloud_host,
            "https://westus.gssv-play-prod.xboxlive.com"
        );
        assert_eq!(tokens.xcloud_f2p_token, "");
        assert_eq!(tokens.xcloud_f2p_host, "");
        assert_eq!(tokens.xcloud_regions.len(), 1);
        assert_eq!(tokens.xcloud_regions[0].name, "westus");
        // Persisted for next launch.
        assert_eq!(h.store.load_stream_tokens().unwrap().xhome_token, "XHOMEGS");

        let requests = h.transport.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[0].url,
            "https://xhome.gssv-play-prod.xboxlive.com/v2/login/user"
        );
        assert_eq!(
            requests[0].body_text(),
            r#"{"offeringId":"xhome","token":"XSTS"}"#
        );
        assert_eq!(requests[0].header("x-gssv-client"), Some("XboxComBrowser"));
        assert_eq!(requests[0].header("x-xbl-contract-version"), Some("1"));
        assert_eq!(requests[0].header("Content-Type"), Some("application/json"));
        assert_eq!(requests[0].header("Accept"), Some("application/json"));
    }

    #[tokio::test]
    async fn full_exchange_sets_both_offerings() {
        let h = harness();
        h.transport.enqueue_json(
            "user.auth.xboxlive.com",
            r#"{"Token":"USER","DisplayClaims":{"xui":[{"uhs":"U"}]}}"#,
            200,
        );
        // First XSTS call is the web token, second is the GSSV stream token.
        h.transport
            .enqueue_json("xsts.auth.xboxlive.com", r#"{"Token":"WEBXSTS"}"#, 200);
        h.transport
            .enqueue_json("xsts.auth.xboxlive.com", r#"{"Token":"STREAMXSTS"}"#, 200);
        h.transport
            .enqueue_json("xhome.gssv", r#"{"gsToken":"XHOME"}"#, 200);
        h.transport.enqueue_json(
            "xgpuweb.gssv",
            r#"{"gsToken":"XCLOUD","offeringSettings":{"regions":[{"name":"weu","baseUri":"https://weu.gssv-play-prod.xboxlive.com","isDefault":true}]}}"#,
            200,
        );
        h.transport
            .enqueue_json("xgpuwebf2p.gssv", r#"{"gsToken":"F2P"}"#, 200);

        let tokens = h.service.exchange_for_stream_tokens("MSA").await.unwrap();
        assert_eq!(tokens.xhome_token, "XHOME");
        assert_eq!(
            tokens.xhome_host,
            "https://xhome.gssv-play-prod.xboxlive.com"
        );
        assert_eq!(tokens.xcloud_token, "XCLOUD");
        assert_eq!(
            tokens.xcloud_host,
            "https://weu.gssv-play-prod.xboxlive.com"
        );
        assert_eq!(tokens.xcloud_f2p_token, "F2P");
        assert_eq!(
            tokens.xcloud_f2p_host,
            "https://xgpuwebf2p.gssv-play-prod.xboxlive.com"
        );
        assert_eq!(tokens.web_token, "WEBXSTS");
        assert_eq!(tokens.xcloud_regions.len(), 1);
        assert!(tokens.is_valid());

        // The XSTS requests went out in web-then-GSSV order with the user token.
        let requests = h.transport.requests();
        assert!(
            requests[1]
                .body_text()
                .contains(r#""RelyingParty":"http://xboxlive.com""#)
        );
        assert!(
            requests[2]
                .body_text()
                .contains(r#""RelyingParty":"http://gssv.xboxlive.com/""#)
        );
        assert!(requests[2].body_text().contains(r#""UserTokens":["USER"]"#));
        // The GSSV logins carry the stream XSTS token, and the whole bundle is persisted.
        assert!(requests[3].body_text().contains(r#""token":"STREAMXSTS""#));
        assert_eq!(h.store.load_stream_tokens().unwrap(), tokens);
    }

    /// The C++ test polls with a 1 s interval and a 20 s budget; the port keeps the same
    /// shape at 1/50 scale through `poll_for_msa_token_with` so the suite stays fast.
    #[tokio::test]
    async fn poll_survives_pending_then_succeeds() {
        let h = harness();
        h.transport.enqueue_json(
            "oauth2/v2.0/token",
            r#"{"error":"authorization_pending"}"#,
            400,
        );
        h.transport
            .enqueue_error("oauth2/v2.0/token", "temporary network failure");
        h.transport.enqueue_json(
            "oauth2/v2.0/token",
            r#"{"access_token":"MSATOKEN","refresh_token":"REFRESH"}"#,
            200,
        );

        let started = Instant::now();
        let token = h
            .service
            .poll_for_msa_token_with("DEV", Duration::from_millis(20), Duration::from_millis(400))
            .await
            .unwrap();
        assert_eq!(token, "MSATOKEN");
        assert_eq!(h.store.load_refresh_token().unwrap(), "REFRESH");
        assert_eq!(h.store.load_msa_token().unwrap(), "MSATOKEN");
        assert_eq!(h.transport.request_count(), 3);
        // One interval before each of the three attempts.
        assert!(started.elapsed() >= Duration::from_millis(60));

        let body = body_text(&h.transport, 0);
        assert!(body.contains("grant_type=urn:ietf:params:oauth:grant-type:device_code"));
        assert!(body.contains("device_code=DEV"));
        assert!(body.contains("client_id=1f907974-e22b-4810-a9de-d9647380c97e"));
    }

    // Cases beyond tst_authservice.cpp, covering the behaviour the port adds or relies on.

    /// `pollForMsaToken` continues only for `oauth:authorization_pending` and `network:`
    /// failures; `oauth:slow_down` is any-other-OAuth-error and finishes the poll.
    #[tokio::test]
    async fn poll_aborts_on_slow_down_like_any_other_oauth_error() {
        let h = harness();
        h.transport.enqueue_json(
            "oauth2/v2.0/token",
            r#"{"error":"authorization_pending"}"#,
            400,
        );
        h.transport
            .enqueue_json("oauth2/v2.0/token", r#"{"error":"slow_down"}"#, 400);
        // Never reached: the poll must not continue past slow_down.
        h.transport
            .enqueue_json("oauth2/v2.0/token", r#"{"access_token":"MSATOKEN"}"#, 200);

        let err = h
            .service
            .poll_for_msa_token_with("DEV", Duration::from_millis(5), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(&err, Error::Auth(_)), "{err}");
        assert!(err.to_string().ends_with("oauth:slow_down"), "{err}");
        assert_eq!(h.transport.request_count(), 2);
        assert_eq!(h.store.load_msa_token().unwrap(), "");

        // The single-shot exchange reports it the same way.
        let h = harness();
        h.transport
            .enqueue_json("oauth2/v2.0/token", r#"{"error":"slow_down"}"#, 400);
        let err = h.service.exchange_device_code("DEV").await.unwrap_err();
        assert!(err.to_string().ends_with("oauth:slow_down"), "{err}");
        assert!(!is_transient(&err));
    }

    #[tokio::test]
    async fn poll_stops_on_expired_token_and_on_deadline() {
        let h = harness();
        h.transport.enqueue_json(
            "oauth2/v2.0/token",
            r#"{"error":"authorization_pending"}"#,
            400,
        );
        h.transport
            .enqueue_json("oauth2/v2.0/token", r#"{"error":"expired_token"}"#, 400);
        let err = h
            .service
            .poll_for_msa_token_with("DEV", Duration::from_millis(5), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("oauth:expired_token"), "{err}");
        assert_eq!(h.transport.request_count(), 2);

        // Nothing queued: every attempt is a transient "no canned response" failure
        // until the deadline passes.
        let h = harness();
        let started = Instant::now();
        let err = h
            .service
            .poll_for_msa_token_with("DEV", Duration::from_millis(40), Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("device code expired"), "{err}");
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert_eq!(h.transport.request_count(), 2);

        // Zero durations are clamped rather than spinning.
        let h = harness();
        h.transport
            .enqueue_json("oauth2/v2.0/token", r#"{"access_token":"T"}"#, 200);
        assert_eq!(
            h.service
                .poll_for_msa_token_with("DEV", Duration::ZERO, Duration::from_secs(1))
                .await
                .unwrap(),
            "T"
        );
    }

    #[tokio::test]
    async fn exchange_device_code_passes_transport_errors_through() {
        let h = harness();
        h.transport.enqueue_error("oauth2/v2.0/token", "offline");
        let err = h.service.exchange_device_code("DEV").await.unwrap_err();
        assert!(
            matches!(&err, Error::Http(msg) if msg == "offline"),
            "{err}"
        );
        assert!(is_transient(&err));
        assert!(!is_transient(&Error::Auth("oauth:slow_down".to_owned())));
    }

    #[tokio::test]
    async fn exchange_device_code_rejects_non_200_without_oauth_error_and_missing_token() {
        let h = harness();
        h.transport
            .enqueue_json("oauth2/v2.0/token", "<html>gateway</html>", 502);
        let err = h.service.exchange_device_code("DEV").await.unwrap_err();
        assert!(err.to_string().contains("HTTP 502"), "{err}");

        h.transport
            .enqueue_json("oauth2/v2.0/token", r#"{"token_type":"Bearer"}"#, 200);
        let err = h.service.exchange_device_code("DEV").await.unwrap_err();
        assert!(err.to_string().contains("Invalid response"), "{err}");
        assert_eq!(h.store.load_msa_token().unwrap(), "");
    }

    #[tokio::test]
    async fn device_code_request_failures() {
        let h = harness();
        h.transport
            .enqueue_json("devicecode", r#"{"error":"invalid_client"}"#, 400);
        let err = h.service.request_device_code().await.unwrap_err();
        assert!(
            err.to_string()
                .contains(r#"Device code request failed HTTP 400: {"error":"invalid_client"}"#),
            "{err}"
        );

        h.transport
            .enqueue_json("devicecode", r#"{"user_code":"ABCD"}"#, 200);
        let err = h.service.request_device_code().await.unwrap_err();
        assert!(err.to_string().contains("Invalid response"), "{err}");

        h.transport.enqueue_error("devicecode", "dns");
        let err = h.service.request_device_code().await.unwrap_err();
        assert!(
            matches!(&err, Error::Http(msg) if msg == "Network error: dns"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn device_code_defaults_apply_when_fields_are_missing_or_mistyped() {
        let h = harness();
        h.transport.enqueue_json(
            "devicecode",
            r#"{"device_code":"DEV","user_code":"ABCD","verification_uri":"https://microsoft.com/devicelogin","expires_in":"soon","interval":-3}"#,
            200,
        );
        let info = h.service.request_device_code().await.unwrap();
        assert_eq!(info.expires_in, 900);
        // `QJsonValue::toInt(5)` keeps a whole-number -3; `pollForMsaToken` clamps it to
        // one second later (`qMax(1, intervalSeconds)`), as the CLI does before polling.
        assert_eq!(info.interval, -3);
        assert_eq!(info.verification_uri_complete, "");
        assert_eq!(
            info.sign_in_url(),
            "https://microsoft.com/devicelogin?otc=ABCD"
        );
        assert_eq!(DeviceCodeInfo::default().sign_in_url(), "");
    }

    #[tokio::test]
    async fn refresh_msa_token_rotates_refresh_token() {
        let h = harness();
        h.transport.enqueue_json(
            "oauth2/v2.0/token",
            r#"{"access_token":"MSA2","refresh_token":"REFRESH2"}"#,
            200,
        );
        let token = h.service.refresh_msa_token("REFRESH1").await.unwrap();
        assert_eq!(token, "MSA2");
        assert_eq!(h.store.load_msa_token().unwrap(), "MSA2");
        assert_eq!(h.store.load_refresh_token().unwrap(), "REFRESH2");

        let body = body_text(&h.transport, 0);
        assert!(body.contains("client_id=1f907974-e22b-4810-a9de-d9647380c97e"));
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("refresh_token=REFRESH1"));
        assert!(body.contains("scope=xboxlive.signin%20openid%20profile%20offline_access"));

        h.transport
            .enqueue_json("oauth2/v2.0/token", r#"{"error":"invalid_grant"}"#, 400);
        let err = h.service.refresh_msa_token("REFRESH2").await.unwrap_err();
        assert!(err.to_string().contains("Refresh HTTP 400"), "{err}");

        h.transport.enqueue_json("oauth2/v2.0/token", r#"{}"#, 200);
        let err = h.service.refresh_msa_token("REFRESH2").await.unwrap_err();
        assert!(
            err.to_string().contains("Invalid refresh response"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn lpt_without_expiry_is_never_fresh_and_errors_surface() {
        let h = harness();
        h.transport
            .enqueue_json("oauth20_token.srf", r#"{"access_token":"LPTTOKEN"}"#, 200);
        assert_eq!(h.service.fetch_lpt("REFRESH").await.unwrap(), "LPTTOKEN");
        assert_eq!(h.store.load_lpt_token().unwrap(), "LPTTOKEN");
        assert!(!h.store.lpt_token_is_fresh().unwrap());
        assert_eq!(h.store.load_refresh_token().unwrap(), "");

        h.transport
            .enqueue_json("oauth20_token.srf", r#"{"error":"invalid_grant"}"#, 400);
        let err = h.service.fetch_lpt("REFRESH").await.unwrap_err();
        assert!(err.to_string().contains("LPT HTTP 400"), "{err}");

        h.transport.enqueue_json("oauth20_token.srf", r#"{}"#, 200);
        let err = h.service.fetch_lpt("REFRESH").await.unwrap_err();
        assert!(err.to_string().contains("Invalid LPT response"), "{err}");
    }

    #[tokio::test]
    async fn fetch_lpt_for_cloud_connect_uses_stored_refresh_token() {
        let h = harness();
        let err = h.service.fetch_lpt_for_cloud_connect().await.unwrap_err();
        assert!(
            err.to_string().contains("No stream token available."),
            "{err}"
        );
        assert_eq!(h.transport.request_count(), 0);

        h.store.save_refresh_token("STORED").unwrap();
        h.transport.enqueue_json(
            "oauth20_token.srf",
            r#"{"access_token":"LPT","expires_in":60}"#,
            200,
        );
        assert_eq!(
            h.service.fetch_lpt_for_cloud_connect().await.unwrap(),
            "LPT"
        );
        assert!(body_text(&h.transport, 0).contains("refresh_token=STORED"));
    }

    #[tokio::test]
    async fn xbox_json_failures_keep_context() {
        let h = harness();
        h.transport.enqueue_error("user.auth.xboxlive.com", "reset");
        let err = h.service.get_xbox_user_token("MSA").await.unwrap_err();
        assert!(
            matches!(&err, Error::Http(msg) if msg == "Token exchange failed: Xbox user authenticate: reset"),
            "{err}"
        );

        h.transport
            .enqueue_json("user.auth.xboxlive.com", r#"{"XErr":2148916233}"#, 401);
        let err = h.service.get_xbox_user_token("MSA").await.unwrap_err();
        assert!(
            err.to_string()
                .contains(r#"Xbox user authenticate: HTTP 401: {"XErr":2148916233}"#),
            "{err}"
        );

        h.transport
            .enqueue_json("user.auth.xboxlive.com", r#"{"DisplayClaims":{}}"#, 200);
        let err = h.service.get_xbox_user_token("MSA").await.unwrap_err();
        assert!(
            err.to_string().contains("Invalid response: missing Token"),
            "{err}"
        );

        h.transport
            .enqueue_json("xsts.auth.xboxlive.com", r#"{"Token":""}"#, 200);
        let err = h.service.get_web_token("USER").await.unwrap_err();
        assert!(err.to_string().contains("Invalid XSTS response"), "{err}");
        assert!(body_text(&h.transport, 3).contains(r#""RelyingParty":"http://xboxlive.com""#));
    }

    #[tokio::test]
    async fn gssv_login_requires_xhome_and_gs_token() {
        let h = harness();
        h.transport.enqueue_error("xhome.gssv", "offline");
        let err = h.service.get_stream_token_bundle("XSTS").await.unwrap_err();
        assert!(
            matches!(&err, Error::Http(msg) if msg == "Token exchange failed: Stream token: offline"),
            "{err}"
        );
        assert_eq!(h.transport.request_count(), 1);
        assert!(!h.store.load_stream_tokens().unwrap().is_valid());

        h.transport
            .enqueue_json("xhome.gssv", r#"{"message":"forbidden"}"#, 403);
        let err = h.service.get_stream_token_bundle("XSTS").await.unwrap_err();
        assert!(
            err.to_string()
                .contains(r#"Stream token HTTP 403: {"message":"forbidden"}"#),
            "{err}"
        );

        h.transport
            .enqueue_json("xhome.gssv", r#"{"offeringSettings":{}}"#, 200);
        let err = h.service.get_stream_token_bundle("XSTS").await.unwrap_err();
        assert!(
            err.to_string().contains("Stream token: missing gsToken"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn gssv_regions_fall_back_to_f2p_and_derive_names() {
        let h = harness();
        h.transport
            .enqueue_json("xhome.gssv", r#"{"gsToken":"XHOME"}"#, 200);
        h.transport
            .enqueue_json("xgpuweb.gssv", r#"{"gsToken":"XCLOUD"}"#, 200);
        h.transport.enqueue_json(
            "xgpuwebf2p.gssv",
            r#"{"gsToken":"F2P","offeringSettings":{"regions":[{"baseUri":"https://neu.gssv-play-prod.xboxlive.com/"},{"name":"skipped"},{"name":"weu","baseUri":"https://weu.gssv-play-prod.xboxlive.com","isDefault":true}]}}"#,
            200,
        );

        let tokens = h.service.get_stream_token_bundle("XSTS").await.unwrap();
        assert_eq!(
            tokens.xcloud_host,
            "https://xgpuweb.gssv-play-prod.xboxlive.com"
        );
        assert_eq!(
            tokens.xcloud_f2p_host,
            "https://weu.gssv-play-prod.xboxlive.com"
        );
        assert_eq!(
            tokens.xcloud_regions,
            vec![
                LoginRegion {
                    name: "neu".into(),
                    base_uri: "https://neu.gssv-play-prod.xboxlive.com/".into(),
                    is_default: false,
                },
                LoginRegion {
                    name: "weu".into(),
                    base_uri: "https://weu.gssv-play-prod.xboxlive.com".into(),
                    is_default: true,
                },
            ]
        );
    }

    #[tokio::test]
    async fn exchange_for_stream_tokens_tolerates_missing_web_token_and_fills_uhs() {
        let h = harness();
        h.transport.enqueue_json(
            "user.auth.xboxlive.com",
            r#"{"Token":"USER","DisplayClaims":{"xui":[{"uhs":"U"}]}}"#,
            200,
        );
        h.transport
            .enqueue_error("xsts.auth.xboxlive.com", "web down");
        h.transport
            .enqueue_json("xsts.auth.xboxlive.com", r#"{"Token":"STREAMXSTS"}"#, 200);
        h.transport
            .enqueue_json("xhome.gssv", r#"{"gsToken":"XHOME"}"#, 200);
        h.transport.enqueue_error("xgpuweb.gssv", "nope");
        h.transport.enqueue_error("xgpuwebf2p.gssv", "nope");

        let tokens = h.service.exchange_for_stream_tokens("MSA").await.unwrap();
        assert_eq!(tokens.xhome_token, "XHOME");
        assert_eq!(tokens.web_token, "");
        assert_eq!(tokens.web_token_uhs, "");
        assert!(tokens.xcloud_regions.is_empty());

        let h = harness();
        h.transport.enqueue_json(
            "user.auth.xboxlive.com",
            r#"{"Token":"USER","DisplayClaims":{"xui":[{"uhs":"U"}]}}"#,
            200,
        );
        h.transport.enqueue_json(
            "xsts.auth.xboxlive.com",
            r#"{"Token":"WEBXSTS","DisplayClaims":{"xui":[{"uhs":"WEBUHS"}]}}"#,
            200,
        );
        h.transport
            .enqueue_json("xsts.auth.xboxlive.com", r#"{"Token":"STREAMXSTS"}"#, 200);
        h.transport
            .enqueue_json("xhome.gssv", r#"{"gsToken":"XHOME"}"#, 200);
        h.transport.enqueue_error("xgpuweb.gssv", "nope");
        h.transport.enqueue_error("xgpuwebf2p.gssv", "nope");
        let tokens = h.service.exchange_for_stream_tokens("MSA").await.unwrap();
        assert_eq!(tokens.web_token, "WEBXSTS");
        assert_eq!(tokens.web_token_uhs, "WEBUHS");
        assert_eq!(
            h.store.load_stream_tokens().unwrap().web_token_uhs,
            "WEBUHS"
        );

        // A failed user token aborts before any XSTS call.
        let h = harness();
        h.transport
            .enqueue_json("user.auth.xboxlive.com", r#"{}"#, 200);
        assert!(h.service.exchange_for_stream_tokens("MSA").await.is_err());
        assert_eq!(h.transport.request_count(), 1);
    }

    #[tokio::test]
    async fn refresh_stream_tokens_and_session_helpers() {
        let h = harness();
        assert!(!h.service.has_stored_session().unwrap());
        let err = h.service.refresh_stream_tokens().await.unwrap_err();
        assert!(
            err.to_string().contains("No stream token available."),
            "{err}"
        );
        assert_eq!(h.transport.request_count(), 0);

        h.store.save_refresh_token("R1").unwrap();
        assert!(h.service.has_stored_session().unwrap());
        h.transport.enqueue_json(
            "oauth2/v2.0/token",
            r#"{"access_token":"MSA","refresh_token":"R2"}"#,
            200,
        );
        h.transport.enqueue_json(
            "user.auth.xboxlive.com",
            r#"{"Token":"USER","DisplayClaims":{"xui":[{"uhs":"U"}]}}"#,
            200,
        );
        h.transport
            .enqueue_json("xsts.auth.xboxlive.com", r#"{"Token":"WEB"}"#, 200);
        h.transport
            .enqueue_json("xsts.auth.xboxlive.com", r#"{"Token":"XSTS"}"#, 200);
        h.transport
            .enqueue_json("xhome.gssv", r#"{"gsToken":"XHOME"}"#, 200);
        h.transport
            .enqueue_json("xgpuweb.gssv", r#"{"gsToken":"XCLOUD"}"#, 200);
        h.transport
            .enqueue_json("xgpuwebf2p.gssv", r#"{"gsToken":"F2P"}"#, 200);

        let tokens = h.service.refresh_stream_tokens().await.unwrap();
        assert_eq!(tokens.xhome_token, "XHOME");
        assert_eq!(tokens.xcloud_token, "XCLOUD");
        assert_eq!(tokens.xcloud_f2p_token, "F2P");
        assert_eq!(tokens.web_token, "WEB");
        assert_eq!(h.store.load_refresh_token().unwrap(), "R2");
        assert_eq!(h.service.restore_stream_tokens().unwrap(), tokens);
        assert!(body_text(&h.transport, 0).contains("refresh_token=R1"));

        h.service.clear_session().unwrap();
        assert!(!h.service.has_stored_session().unwrap());
        assert_eq!(
            h.service.restore_stream_tokens().unwrap(),
            StreamTokens::default()
        );
        assert_eq!(h.store.load_msa_token().unwrap(), "");
    }

    #[test]
    fn form_encode_matches_qt_fully_encoded() {
        assert_eq!(
            form_encode(&[("client_id", CLIENT_ID), ("scope", SCOPE)]),
            b"client_id=1f907974-e22b-4810-a9de-d9647380c97e&scope=xboxlive.signin%20openid%20profile%20offline_access"
        );
        assert_eq!(
            String::from_utf8(form_encode(&[("scope", LPT_SCOPE)])).unwrap(),
            "scope=service::http://Passport.NET/purpose::PURPOSE_XBOX_CLOUD_CONSOLE_TRANSFER_TOKEN"
        );
        assert_eq!(
            String::from_utf8(form_encode(&[("k", "a&b=c#d%e\"f ü")])).unwrap(),
            "k=a%26b%3Dc%23d%25e%22f%20%C3%BC"
        );
        assert_eq!(form_encode(&[]), b"");
    }

    #[test]
    fn json_helpers_mirror_qjsonvalue_defaults() {
        let object = serde_json::json!({"s": "x", "n": 7, "neg": -1, "f": 1.5, "arr": []});
        assert_eq!(json_string(&object, "s"), "x");
        assert_eq!(json_string(&object, "n"), "");
        assert_eq!(json_string(&object, "missing"), "");
        assert_eq!(json_i64(&object, "neg", 0), -1);
        assert_eq!(json_i64(&object, "s", 3), 3);
        assert_eq!(
            parse_json_object(b"[1,2]"),
            Value::Object(Default::default())
        );
        assert_eq!(
            parse_json_object(b"not json"),
            Value::Object(Default::default())
        );
        assert_eq!(parse_json_object(br#"{"a":1}"#)["a"], 1);
        assert_eq!(summarize_body(b""), "<empty>");
        assert_eq!(summarize_body(b"abc"), "abc");
        assert_eq!(
            summarize_body("é".repeat(600).as_bytes()).chars().count(),
            512
        );

        let token =
            xbox_token_from(&serde_json::json!({"Token": "T", "DisplayClaims": {"xui": []}}));
        assert_eq!(
            token,
            XboxToken {
                token: "T".into(),
                user_hash: String::new()
            }
        );
    }
}
