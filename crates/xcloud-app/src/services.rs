//! Port of qt/src/app/AppController.{h,cpp}: the composition root. Builds the HTTP
//! transport, the token store, `MicrosoftAuthService`, `XCloudApiClient`,
//! `GamePassCatalogClient` and the WebRTC transport, holds the tokio runtime handle the
//! controllers spawn on, and ports `updateApiCredentials`, `wireSession` and `bootstrap`.
//!
//! Differences from the C++:
//! - `QtHttpTransport` is `ReqwestTransport`; `SettingsTokenStore` is the keychain-backed
//!   `KeyringTokenStore` (docs/archive/PORTING_PLAN.md section 2), or `MemoryTokenStore` when the
//!   environment variable `OPENXCLOUD_MEMORY_TOKENS=1` is set (and always under `cfg(test)`).
//! - WebRTC availability is a runtime choice: webrtc-rs is linked, so the
//!   production graph gets a `WebRtcTransport`; tests pass a `NullTransport` through
//!   [`Services::with_parts`]. `streaming_available` is `Transport::is_available`.
//! - The `tokensChanged` / `authenticated` signal connections of `wireSession` are the
//!   [`SessionEvent`] values the session controller emits; the app shell drains them once
//!   per frame and hands each to [`Services::handle_session_event`].
//! - `bootstrap` only asks the session controller to restore the session. The C++
//!   `updateApiCredentials()` with the stored bundle happens when the controller reports
//!   [`SessionEvent::TokensRestored`]: the keychain read runs once on the controller's
//!   tokio task instead of on the UI thread that calls `bootstrap`. The initial library
//!   load follows from the `Authenticated` event exactly as in the C++
//!   (`authenticated` → `refresh(true)`).

use std::sync::Arc;

use anyhow::Context as _;
use tokio::runtime::Handle;
use xcloud_api::auth::{MicrosoftAuthService, StreamTokens};
use xcloud_api::catalog::GamePassCatalogClient;
use xcloud_api::client::XCloudApiClient;
use xcloud_api::token_store::{KeyringTokenStore, MemoryTokenStore, TokenStore};
use xcloud_api::transport::{HttpTransport, ReqwestTransport};
use xcloud_proto::preferences::{
    StreamPreferences, StreamResolutionMode, stream_resolution_from_string,
};
use xcloud_webrtc::Transport;
use xcloud_webrtc::native::WebRtcTransport;

use crate::session::SessionEvent;
use crate::state::{AppSettings, LibraryCommand, LibraryHandle, SessionCommand, SessionHandle};

const LOG: &str = "App";

/// `OPENXCLOUD_MEMORY_TOKENS=1` keeps credentials in memory for this process only (no
/// keychain prompts; useful for previews and smoke runs).
pub const MEMORY_TOKENS_ENV: &str = "OPENXCLOUD_MEMORY_TOKENS";

/// Application version from the Cargo package metadata.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything `AppController` constructed and owned, shared with the controllers.
/// The HTTP transport and the token store are reachable through `auth`
/// (`transport()`, `token_store()`).
#[derive(Clone)]
pub struct Services {
    pub auth: Arc<MicrosoftAuthService>,
    pub api: Arc<XCloudApiClient>,
    pub catalog: Arc<GamePassCatalogClient>,
    /// `m_bridge` as the stream controller consumes it.
    pub transport: Arc<dyn Transport>,
    /// The same object as `transport` when it is the webrtc bridge, for the
    /// webrtc-only entry points (`set_configurable_media_sink`, `request_keyframe`).
    pub webrtc: Option<Arc<WebRtcTransport>>,
    /// `AppController::streamingAvailable`.
    pub streaming_available: bool,
    /// The runtime every controller task is spawned on.
    pub runtime: Handle,
    /// `AppController::appVersion`.
    pub app_version: String,
}

impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Services")
            .field("streaming_available", &self.streaming_available)
            .field("app_version", &self.app_version)
            .field("webrtc", &self.webrtc.is_some())
            .finish_non_exhaustive()
    }
}

impl Services {
    /// `AppController::AppController`: builds the production object graph. `runtime` is
    /// the handle of the app's tokio runtime.
    pub fn new(runtime: Handle) -> anyhow::Result<Self> {
        let http: Arc<dyn HttpTransport> =
            Arc::new(ReqwestTransport::new().context("cannot build the HTTP client")?);
        let store = token_store_from_env();
        let webrtc = Arc::new(WebRtcTransport::new());
        let mut services = Self::with_parts(http, store, webrtc.clone(), runtime);
        services.webrtc = Some(webrtc);
        Ok(services)
    }

    /// Builds the graph from explicit parts (tests, previews). `transport` is the WebRTC
    /// bridge; `streaming_available` follows its `is_available()`.
    pub fn with_parts(
        http: Arc<dyn HttpTransport>,
        store: Arc<dyn TokenStore>,
        transport: Arc<dyn Transport>,
        runtime: Handle,
    ) -> Self {
        let auth = Arc::new(MicrosoftAuthService::new(Arc::clone(&http), store));
        let api = Arc::new(XCloudApiClient::new(Arc::clone(&http)));
        let catalog = Arc::new(GamePassCatalogClient::new(http));
        let streaming_available = transport.is_available();
        Self {
            auth,
            api,
            catalog,
            transport,
            webrtc: None,
            streaming_available,
            runtime,
            app_version: APP_VERSION.to_owned(),
        }
    }

    /// `AppController::updateApiCredentials`: hands the API client the regional host and
    /// GSSV token picked by [`api_credentials`].
    pub fn update_api_credentials(&self, tokens: &StreamTokens) {
        let (host, token) = api_credentials(tokens);
        self.api.update_credentials(&host, &token);
        tracing::info!(
            target: LOG,
            "API credentials updated host={host} tokenPresent={}",
            if token.is_empty() { "false" } else { "true" }
        );
    }

    /// The preferences half of `StreamController::refreshPreferences`: derives the
    /// `StreamPreferences` from the settings, stores them on the API client and re-applies
    /// the current credentials so the device-info header is rebuilt.
    pub fn apply_preferences(&self, settings: &AppSettings) {
        let preferences = stream_preferences(settings);
        self.api.set_preferences(&preferences);
        self.api
            .update_credentials(&self.api.base_host(), &self.api.gs_token());
    }

    /// `AppController::bootstrap`: starts the session restore. Safe to call on the UI
    /// thread: it never touches the token store. The stored bundle reaches the API client
    /// through `SessionEvent::TokensRestored` and the initial library load happens when
    /// the session controller reports `SessionEvent::Authenticated` (see
    /// [`Self::handle_session_event`]).
    pub fn bootstrap(&self, session: &SessionHandle) {
        session.send(SessionCommand::RestoreSession);
    }

    /// `AppController::wireSession`, driven by the app shell: `tokensChanged` (and the
    /// `updateApiCredentials()` of `bootstrap`) update the API credentials and
    /// `authenticated` forces a library refresh.
    pub fn handle_session_event(&self, event: &SessionEvent, library: &LibraryHandle) {
        match event {
            SessionEvent::TokensRestored(tokens) => {
                self.update_api_credentials(tokens);
            }
            SessionEvent::Authenticated(tokens) => {
                self.update_api_credentials(tokens);
                library.send(LibraryCommand::Refresh { force: true });
            }
            SessionEvent::SignedOut => {
                self.update_api_credentials(&StreamTokens::default());
            }
        }
    }
}

/// True when `OPENXCLOUD_MEMORY_TOKENS` is `1`, `true`, `yes` or `on` (case-insensitive).
pub fn memory_tokens_requested() -> bool {
    std::env::var(MEMORY_TOKENS_ENV).is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// The production token store: the keychain, unless `OPENXCLOUD_MEMORY_TOKENS=1` (or a test
/// build) asks for the in-memory store.
pub fn token_store_from_env() -> Arc<dyn TokenStore> {
    if cfg!(test) || memory_tokens_requested() {
        tracing::info!(target: LOG, "using the in-memory token store");
        Arc::new(MemoryTokenStore::new())
    } else {
        Arc::new(KeyringTokenStore::new())
    }
}

/// The host/token pair `AppController::updateApiCredentials` picks: xCloud, then the
/// free-to-play offering, then xHome. Either string may be empty when nothing is usable.
pub fn api_credentials(tokens: &StreamTokens) -> (String, String) {
    let candidates = [
        (&tokens.xcloud_host, &tokens.xcloud_token),
        (&tokens.xcloud_f2p_host, &tokens.xcloud_f2p_token),
        (&tokens.xhome_host, &tokens.xhome_token),
    ];
    for (host, token) in candidates {
        if !host.is_empty() && !token.is_empty() {
            return (host.clone(), token.clone());
        }
    }
    // The C++ falls through to the xHome pair even when it is incomplete.
    (tokens.xhome_host.clone(), tokens.xhome_token.clone())
}

/// `Settings::resolutionProfile` → `StreamResolutionMode`. Accepts the persisted Qt
/// spelling (`p1080`, `p1080HQ`) and the UI spelling of `AppSettings` (`1080p`,
/// `1080pHQ`); anything else is `P1080`.
pub fn resolution_from_profile(profile: &str) -> StreamResolutionMode {
    let trimmed = profile.trim();
    let normalised = match trimmed.to_ascii_lowercase().as_str() {
        "720p" => "p720",
        "1080p" => "p1080",
        "1440p" => "p1440",
        "1080phq" => "p1080HQ",
        _ => trimmed,
    };
    stream_resolution_from_string(normalised, StreamResolutionMode::P1080)
}

/// `StreamController::refreshPreferences`: the `StreamPreferences` the API client and
/// the signaling orchestrator read from the settings.
pub fn stream_preferences(settings: &AppSettings) -> StreamPreferences {
    StreamPreferences {
        resolution: resolution_from_profile(&settings.resolution_profile),
        locale: settings.locale.clone(),
        prefer_ipv6: settings.prefer_ipv6,
        preferred_region_id: settings.preferred_region.clone(),
        ..StreamPreferences::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Handle as UiHandle, LibrarySnapshot, SessionSnapshot};
    use xcloud_api::transport::FakeTransport;
    use xcloud_webrtc::null::NullTransport;

    fn tokens() -> StreamTokens {
        StreamTokens {
            xhome_token: "home-token".into(),
            xhome_host: "home.example".into(),
            xcloud_token: "cloud-token".into(),
            xcloud_host: "cloud.example".into(),
            xcloud_f2p_token: "f2p-token".into(),
            xcloud_f2p_host: "f2p.example".into(),
            ..StreamTokens::default()
        }
    }

    fn services(store: Arc<MemoryTokenStore>) -> Services {
        Services::with_parts(
            Arc::new(FakeTransport::new()),
            store,
            Arc::new(NullTransport::new()),
            Handle::current(),
        )
    }

    #[test]
    fn prefers_xcloud_then_f2p_then_xhome() {
        let full = tokens();
        assert_eq!(
            api_credentials(&full),
            ("cloud.example".to_owned(), "cloud-token".to_owned())
        );

        let mut no_cloud = tokens();
        no_cloud.xcloud_token.clear();
        assert_eq!(
            api_credentials(&no_cloud),
            ("f2p.example".to_owned(), "f2p-token".to_owned())
        );

        let mut home_only = tokens();
        home_only.xcloud_host.clear();
        home_only.xcloud_f2p_host.clear();
        assert_eq!(
            api_credentials(&home_only),
            ("home.example".to_owned(), "home-token".to_owned())
        );
    }

    #[test]
    fn empty_tokens_yield_empty_credentials() {
        assert_eq!(
            api_credentials(&StreamTokens::default()),
            (String::new(), String::new())
        );
    }

    #[test]
    fn resolution_profile_accepts_both_spellings() {
        assert_eq!(
            resolution_from_profile("p1440"),
            StreamResolutionMode::P1440
        );
        assert_eq!(
            resolution_from_profile("1440p"),
            StreamResolutionMode::P1440
        );
        assert_eq!(
            resolution_from_profile("1080pHQ"),
            StreamResolutionMode::P1080HQ
        );
        assert_eq!(
            resolution_from_profile("P1080HQ"),
            StreamResolutionMode::P1080HQ
        );
        assert_eq!(
            resolution_from_profile(" auto "),
            StreamResolutionMode::Auto
        );
        assert_eq!(resolution_from_profile("720p"), StreamResolutionMode::P720);
        assert_eq!(
            resolution_from_profile("bogus"),
            StreamResolutionMode::P1080
        );
    }

    #[test]
    fn stream_preferences_follow_settings() {
        let settings = AppSettings {
            resolution_profile: "1440p".into(),
            locale: "de-DE".into(),
            prefer_ipv6: true,
            preferred_region: "weu".into(),
            ..AppSettings::default()
        };
        let preferences = stream_preferences(&settings);
        assert_eq!(preferences.resolution, StreamResolutionMode::P1440);
        assert_eq!(preferences.locale, "de-DE");
        assert!(preferences.prefer_ipv6);
        assert_eq!(preferences.preferred_region_id, "weu");
        assert!(preferences.fallback_region_names.is_empty());
        assert!(preferences.os_name_override.is_empty());
    }

    #[tokio::test]
    async fn update_api_credentials_and_preferences_reach_the_client() {
        let services = services(Arc::new(MemoryTokenStore::new()));
        assert!(!services.streaming_available);
        assert!(services.webrtc.is_none());
        assert_eq!(services.app_version, APP_VERSION);

        services.update_api_credentials(&tokens());
        assert_eq!(services.api.base_host(), "cloud.example");
        assert_eq!(services.api.gs_token(), "cloud-token");

        services.apply_preferences(&AppSettings {
            resolution_profile: "720p".into(),
            ..AppSettings::default()
        });
        assert_eq!(
            services.api.preferences().resolution,
            StreamResolutionMode::P720
        );
        // Re-applying preferences keeps the credentials.
        assert_eq!(services.api.base_host(), "cloud.example");
        assert_eq!(services.api.gs_token(), "cloud-token");
    }

    #[tokio::test]
    async fn bootstrap_requests_restore_without_reading_the_store() {
        let store = Arc::new(MemoryTokenStore::new());
        store.save_stream_tokens(&tokens()).expect("memory store");
        let services = services(store);
        let (session, _snapshot, commands) = UiHandle::new(SessionSnapshot::default());

        services.bootstrap(&session);

        // The stored bundle is read by the session controller's task, not here on the
        // caller's (UI) thread; the credentials follow from `TokensRestored`.
        assert_eq!(services.api.base_host(), "");
        assert_eq!(services.api.gs_token(), "");
        assert_eq!(commands.try_recv(), Ok(SessionCommand::RestoreSession));
        assert!(commands.try_recv().is_err());
    }

    #[tokio::test]
    async fn tokens_restored_event_applies_the_stored_bundle() {
        let services = services(Arc::new(MemoryTokenStore::new()));
        let (library, _snapshot, commands) = UiHandle::new(LibrarySnapshot::default());

        services.handle_session_event(&SessionEvent::TokensRestored(Box::new(tokens())), &library);
        assert_eq!(services.api.base_host(), "cloud.example");
        assert_eq!(services.api.gs_token(), "cloud-token");
        assert!(commands.try_recv().is_err(), "bootstrap does not refresh");

        // An empty bundle (fresh install) applies empty credentials, like the C++.
        services.handle_session_event(&SessionEvent::TokensRestored(Box::default()), &library);
        assert_eq!(services.api.base_host(), "");
        assert_eq!(services.api.gs_token(), "");
    }

    #[test]
    fn default_settings_match_the_qt_defaults() {
        // core/Settings.cpp: stream/resolutionProfile "p1440", input/triggerSensitivity 0.5.
        let settings = AppSettings::default();
        assert_eq!(
            stream_preferences(&settings).resolution,
            StreamResolutionMode::P1440
        );
        assert_eq!(settings.trigger_sensitivity, 0.5);
        assert_eq!(settings.preferred_frame_rate, 60);
        assert_eq!(settings.locale, "en-US");
        assert_eq!(settings.preferred_region, "");
        assert!(!settings.prefer_ipv6);
        assert!(!settings.hud_enabled);
        assert_eq!(settings.deadzone, 0.15);
        assert!(!settings.invert_y);
        assert!(!settings.swap_ab);
        assert_eq!(settings.vibration_intensity, 1.0);
    }

    #[tokio::test]
    async fn authenticated_event_updates_credentials_and_refreshes_library() {
        let services = services(Arc::new(MemoryTokenStore::new()));
        let (library, _snapshot, commands) = UiHandle::new(LibrarySnapshot::default());

        services.handle_session_event(&SessionEvent::Authenticated(Box::new(tokens())), &library);
        assert_eq!(services.api.base_host(), "cloud.example");
        assert_eq!(services.api.gs_token(), "cloud-token");
        assert_eq!(
            commands.try_recv(),
            Ok(LibraryCommand::Refresh { force: true })
        );

        services.handle_session_event(&SessionEvent::SignedOut, &library);
        assert_eq!(services.api.base_host(), "");
        assert_eq!(services.api.gs_token(), "");
        assert!(commands.try_recv().is_err(), "sign-out does not refresh");
    }

    #[test]
    fn test_builds_use_the_memory_store() {
        // `cfg!(test)` selects the memory store regardless of the environment, so this
        // never touches the keychain.
        let store = token_store_from_env();
        store.save_refresh_token("R").expect("memory store");
        assert_eq!(store.load_refresh_token().expect("memory store"), "R");
    }
}
