//! Port of qt/src/auth/TokenStore.{h,cpp} (`TokenStore` trait, `KeyringTokenStore`,
//! `MemoryTokenStore`). The `StreamTokens`/`LoginRegion` JSON codec from
//! qt/src/auth/AuthTypes.h and TokenStore.cpp lives in [`xcloud_proto::auth_types`].
//!
//! The Qt production store (`SettingsTokenStore`) wrote secrets to plain `QSettings`.
//! Per docs/archive/PORTING_PLAN.md section 2 the Rust port keeps them in the OS credential store
//! through the `keyring` crate (macOS Keychain first). Every operation can therefore
//! fail, so the trait returns `Result` where the C++ was `void`/infallible; keyring
//! failures map to `Error::TokenStore`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use crate::{Error, Result};

// `LoginRegion`, `StreamTokens` and `preferred_region` are owned by xcloud-proto
// (`auth_types`); they are re-exported here so the token store API reads as in the C++.
pub use xcloud_proto::auth_types::{LoginRegion, StreamTokens, preferred_region};

/// Abstract credential store. An absent value reads back as an empty string (the C++
/// `QString()` convention), so `has_session` is "a refresh token exists".
pub trait TokenStore: Send + Sync {
    fn save_msa_token(&self, token: &str) -> Result<()>;
    fn load_msa_token(&self) -> Result<String>;

    fn save_refresh_token(&self, token: &str) -> Result<()>;
    fn load_refresh_token(&self) -> Result<String>;

    /// `expires_at == None` mirrors an invalid `QDateTime`: the token is never fresh.
    fn save_lpt_token(&self, token: &str, expires_at: Option<DateTime<Utc>>) -> Result<()>;
    fn load_lpt_token(&self) -> Result<String>;
    fn lpt_token_is_fresh(&self) -> Result<bool>;

    fn save_stream_tokens(&self, tokens: &StreamTokens) -> Result<()>;
    fn load_stream_tokens(&self) -> Result<StreamTokens>;

    fn clear_all(&self) -> Result<()>;

    fn has_session(&self) -> Result<bool> {
        Ok(!self.load_refresh_token()?.is_empty())
    }
}

#[derive(Debug, Default)]
struct MemoryState {
    msa_token: String,
    refresh_token: String,
    lpt_token: String,
    lpt_expiry: Option<DateTime<Utc>>,
    stream_tokens: StreamTokens,
}

/// In-memory store used by unit tests and previews. Port of `MemoryTokenStore`.
#[derive(Debug, Default)]
pub struct MemoryTokenStore {
    state: Mutex<MemoryState>,
}

impl MemoryTokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl TokenStore for MemoryTokenStore {
    fn save_msa_token(&self, token: &str) -> Result<()> {
        self.lock().msa_token = token.to_owned();
        Ok(())
    }

    fn load_msa_token(&self) -> Result<String> {
        Ok(self.lock().msa_token.clone())
    }

    fn save_refresh_token(&self, token: &str) -> Result<()> {
        self.lock().refresh_token = token.to_owned();
        Ok(())
    }

    fn load_refresh_token(&self) -> Result<String> {
        Ok(self.lock().refresh_token.clone())
    }

    fn save_lpt_token(&self, token: &str, expires_at: Option<DateTime<Utc>>) -> Result<()> {
        let mut state = self.lock();
        state.lpt_token = token.to_owned();
        state.lpt_expiry = expires_at;
        Ok(())
    }

    fn load_lpt_token(&self) -> Result<String> {
        Ok(self.lock().lpt_token.clone())
    }

    /// Unlike the settings/keyring store, the C++ memory store also requires a
    /// non-empty token.
    fn lpt_token_is_fresh(&self) -> Result<bool> {
        let state = self.lock();
        Ok(!state.lpt_token.is_empty()
            && state.lpt_expiry.is_some_and(|expiry| expiry > Utc::now()))
    }

    fn save_stream_tokens(&self, tokens: &StreamTokens) -> Result<()> {
        self.lock().stream_tokens = tokens.clone();
        Ok(())
    }

    fn load_stream_tokens(&self) -> Result<StreamTokens> {
        Ok(self.lock().stream_tokens.clone())
    }

    fn clear_all(&self) -> Result<()> {
        *self.lock() = MemoryState::default();
        Ok(())
    }
}

/// Keychain service name under which [`KeyringTokenStore`] files its entries.
pub const KEYRING_SERVICE: &str = "openxcloud";

const KEY_MSA_TOKEN: &str = "msa_token";
const KEY_REFRESH_TOKEN: &str = "refresh_token";
const KEY_LPT_TOKEN: &str = "lpt_token";
const KEY_LPT_EXPIRY: &str = "lpt_expiry";
const KEY_STREAM_TOKENS: &str = "stream_tokens";
const KEY_BUNDLE: &str = "credentials_v1";
type Credentials = HashMap<String, String>;

trait CredentialBackend: Send + Sync {
    fn read(&self, key: &str) -> Result<String>;
    fn write(&self, key: &str, value: &str) -> Result<()>;
    fn remove(&self, key: &str) -> Result<()>;
}

struct OsCredentials {
    service: String,
}

impl OsCredentials {
    fn entry(&self, key: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, key)
            .map_err(|e| Error::TokenStore(format!("{key}: {e}")))
    }
}

impl CredentialBackend for OsCredentials {
    fn read(&self, key: &str) -> Result<String> {
        match self.entry(key)?.get_password() {
            Ok(value) => Ok(value),
            Err(keyring::Error::NoEntry) => Ok(String::new()),
            Err(e) => Err(Error::TokenStore(format!("read {key}: {e}"))),
        }
    }

    fn write(&self, key: &str, value: &str) -> Result<()> {
        self.entry(key)?
            .set_password(value)
            .map_err(|e| Error::TokenStore(format!("write {key}: {e}")))
    }

    fn remove(&self, key: &str) -> Result<()> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(Error::TokenStore(format!("delete {key}: {e}"))),
        }
    }
}

/// One Keychain item for all credentials, loaded once per store and shared by clones.
/// An absent bundle is a fresh, signed-out session.
#[derive(Clone)]
pub struct KeyringTokenStore {
    service: String,
    backend: Arc<dyn CredentialBackend>,
    cache: Arc<Mutex<Option<Credentials>>>,
}

impl std::fmt::Debug for KeyringTokenStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyringTokenStore")
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

impl Default for KeyringTokenStore {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyringTokenStore {
    pub fn new() -> Self {
        Self::with_service(KEYRING_SERVICE)
    }

    pub fn with_service(service: &str) -> Self {
        Self {
            service: service.to_owned(),
            backend: Arc::new(OsCredentials {
                service: service.to_owned(),
            }),
            cache: Arc::new(Mutex::new(None)),
        }
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    fn load(&self) -> Result<Credentials> {
        let raw = self.backend.read(KEY_BUNDLE)?;
        if raw.is_empty() {
            return Ok(Credentials::new());
        }
        serde_json::from_str(&raw)
            .map_err(|_| Error::TokenStore("invalid credential bundle".into()))
    }

    fn persist(&self, values: &Credentials) -> Result<()> {
        self.backend
            .write(KEY_BUNDLE, &serde_json::to_string(values)?)
    }

    fn read(&self, key: &str) -> Result<String> {
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if cache.is_none() {
            *cache = Some(self.load()?);
        }
        Ok(cache
            .as_ref()
            .unwrap()
            .get(key)
            .cloned()
            .unwrap_or_default())
    }

    fn update(&self, changes: &[(&str, &str)]) -> Result<()> {
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if cache.is_none() {
            *cache = Some(self.load()?);
        }
        let mut values = cache.as_ref().unwrap().clone();
        for (key, value) in changes {
            if value.is_empty() {
                values.remove(*key);
            } else {
                values.insert((*key).to_owned(), (*value).to_owned());
            }
        }
        if cache.as_ref() != Some(&values) {
            self.persist(&values)?;
            *cache = Some(values);
        }
        Ok(())
    }

    fn write(&self, key: &str, value: &str) -> Result<()> {
        self.update(&[(key, value)])
    }
}

impl TokenStore for KeyringTokenStore {
    fn save_msa_token(&self, token: &str) -> Result<()> {
        self.write(KEY_MSA_TOKEN, token)
    }

    fn load_msa_token(&self) -> Result<String> {
        self.read(KEY_MSA_TOKEN)
    }

    fn save_refresh_token(&self, token: &str) -> Result<()> {
        self.write(KEY_REFRESH_TOKEN, token)
    }

    fn load_refresh_token(&self) -> Result<String> {
        self.read(KEY_REFRESH_TOKEN)
    }

    fn save_lpt_token(&self, token: &str, expires_at: Option<DateTime<Utc>>) -> Result<()> {
        // Qt stored `expiresAt.toString(Qt::ISODate)`, or empty for an invalid date.
        let expiry = expires_at.map(|at| at.to_rfc3339()).unwrap_or_default();
        self.update(&[(KEY_LPT_TOKEN, token), (KEY_LPT_EXPIRY, &expiry)])
    }

    fn load_lpt_token(&self) -> Result<String> {
        self.read(KEY_LPT_TOKEN)
    }

    fn lpt_token_is_fresh(&self) -> Result<bool> {
        let raw = self.read(KEY_LPT_EXPIRY)?;
        if raw.is_empty() {
            return Ok(false);
        }
        Ok(DateTime::parse_from_rfc3339(&raw)
            .map(|expiry| expiry.with_timezone(&Utc) > Utc::now())
            .unwrap_or(false))
    }

    fn save_stream_tokens(&self, tokens: &StreamTokens) -> Result<()> {
        let json = serde_json::to_string(&tokens.to_json())?;
        self.write(KEY_STREAM_TOKENS, &json)
    }

    fn load_stream_tokens(&self) -> Result<StreamTokens> {
        let raw = self.read(KEY_STREAM_TOKENS)?;
        if raw.is_empty() {
            return Ok(StreamTokens::default());
        }
        match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(value) if value.is_object() => Ok(StreamTokens::from_json(&value)),
            Ok(_) | Err(_) => {
                tracing::warn!(target: "Auth", "stored stream tokens are not a JSON object; ignoring");
                Ok(StreamTokens::default())
            }
        }
    }

    fn clear_all(&self) -> Result<()> {
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        self.backend.remove(KEY_BUNDLE)?;
        *cache = Some(Credentials::new());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[derive(Default)]
    struct FakeCredentials {
        values: Mutex<Credentials>,
        reads: Mutex<Vec<String>>,
        fail_write: std::sync::atomic::AtomicBool,
    }

    impl CredentialBackend for FakeCredentials {
        fn read(&self, key: &str) -> Result<String> {
            self.reads.lock().unwrap().push(key.to_owned());
            Ok(self
                .values
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .unwrap_or_default())
        }
        fn write(&self, key: &str, value: &str) -> Result<()> {
            if self.fail_write.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(Error::TokenStore("write denied".into()));
            }
            self.values
                .lock()
                .unwrap()
                .insert(key.to_owned(), value.to_owned());
            Ok(())
        }
        fn remove(&self, key: &str) -> Result<()> {
            self.values.lock().unwrap().remove(key);
            Ok(())
        }
    }

    fn fake_store(backend: &Arc<FakeCredentials>) -> KeyringTokenStore {
        KeyringTokenStore {
            service: "test".into(),
            backend: backend.clone(),
            cache: Arc::new(Mutex::new(None)),
        }
    }

    #[test]
    fn bundle_is_read_once_across_startup_and_clones() {
        let backend = Arc::new(FakeCredentials::default());
        backend
            .write(KEY_BUNDLE, r#"{"refresh_token":"R1"}"#)
            .unwrap();
        let store = fake_store(&backend);
        store.load_stream_tokens().unwrap();
        assert!(store.has_session().unwrap());
        assert_eq!(store.clone().load_refresh_token().unwrap(), "R1");
        store.save_msa_token("MSA").unwrap();
        store.save_refresh_token("R2").unwrap();
        store.save_stream_tokens(&sample_tokens()).unwrap();
        assert_eq!(*backend.reads.lock().unwrap(), [KEY_BUNDLE]);
        let restarted = fake_store(&backend);
        assert_eq!(restarted.load_refresh_token().unwrap(), "R2");
        assert_eq!(restarted.load_stream_tokens().unwrap(), sample_tokens());
    }

    #[test]
    fn fresh_install_only_reads_the_bundle_without_creating_entries() {
        let backend = Arc::new(FakeCredentials::default());
        let store = fake_store(&backend);
        assert!(!store.has_session().unwrap());
        assert_eq!(store.load_stream_tokens().unwrap(), StreamTokens::default());
        assert_eq!(*backend.reads.lock().unwrap(), [KEY_BUNDLE]);
        assert!(backend.values.lock().unwrap().is_empty());
    }

    #[test]
    fn sign_out_removes_the_bundle_and_clears_shared_memory() {
        let backend = Arc::new(FakeCredentials::default());
        let store = fake_store(&backend);
        store.save_refresh_token("R1").unwrap();
        store.save_stream_tokens(&sample_tokens()).unwrap();
        let clone = store.clone();
        store.clear_all().unwrap();
        assert!(!clone.has_session().unwrap());
        assert_eq!(clone.load_stream_tokens().unwrap(), StreamTokens::default());
        assert!(!fake_store(&backend).has_session().unwrap());
        assert!(backend.values.lock().unwrap().is_empty());
        store.clear_all().unwrap();
    }

    #[test]
    fn failed_writes_preserve_the_bundle_and_cached_state() {
        use std::sync::atomic::Ordering;
        let backend = Arc::new(FakeCredentials::default());
        let store = fake_store(&backend);
        store.save_refresh_token("R1").unwrap();
        let previous = backend.values.lock().unwrap()[KEY_BUNDLE].clone();
        backend.fail_write.store(true, Ordering::Relaxed);
        assert!(store.save_refresh_token("R2").is_err());
        assert_eq!(store.load_refresh_token().unwrap(), "R1");
        assert_eq!(backend.values.lock().unwrap()[KEY_BUNDLE], previous);
    }

    #[test]
    fn malformed_bundle_is_not_silently_overwritten() {
        let backend = Arc::new(FakeCredentials::default());
        backend.write(KEY_BUNDLE, "broken").unwrap();
        let store = fake_store(&backend);
        assert!(store.load_refresh_token().is_err());
        assert!(store.save_refresh_token("R2").is_err());
        assert_eq!(backend.values.lock().unwrap()[KEY_BUNDLE], "broken");
    }

    fn sample_tokens() -> StreamTokens {
        StreamTokens {
            xhome_token: "XHOME".into(),
            xhome_host: "xhome.gssv-play-prod.xboxlive.com".into(),
            xcloud_token: "XCLOUD".into(),
            xcloud_host: "xgpuweb.gssv-play-prod.xboxlive.com".into(),
            xcloud_f2p_token: "F2P".into(),
            xcloud_f2p_host: "xgpuwebf2p.gssv-play-prod.xboxlive.com".into(),
            web_token: "WEBXSTS".into(),
            web_token_uhs: "U".into(),
            xcloud_regions: vec![
                LoginRegion {
                    name: "weu".into(),
                    base_uri: "https://weu.gssv-play-prod.xboxlive.com".into(),
                    is_default: true,
                },
                LoginRegion {
                    name: "eus".into(),
                    base_uri: "https://eus.gssv-play-prod.xboxlive.com".into(),
                    is_default: false,
                },
            ],
        }
    }

    #[test]
    fn memory_store_round_trips_msa_and_refresh_tokens() {
        let store = MemoryTokenStore::new();
        assert_eq!(store.load_msa_token().unwrap(), "");
        assert_eq!(store.load_refresh_token().unwrap(), "");
        assert!(!store.has_session().unwrap());

        store.save_msa_token("MSA").unwrap();
        store.save_refresh_token("REFRESH").unwrap();
        assert_eq!(store.load_msa_token().unwrap(), "MSA");
        assert_eq!(store.load_refresh_token().unwrap(), "REFRESH");
        assert!(store.has_session().unwrap());
    }

    #[test]
    fn memory_store_lpt_freshness() {
        let store = MemoryTokenStore::new();
        assert!(!store.lpt_token_is_fresh().unwrap());

        store
            .save_lpt_token("LPT", Some(Utc::now() + Duration::hours(1)))
            .unwrap();
        assert_eq!(store.load_lpt_token().unwrap(), "LPT");
        assert!(store.lpt_token_is_fresh().unwrap());

        store
            .save_lpt_token("LPT", Some(Utc::now() - Duration::seconds(1)))
            .unwrap();
        assert!(!store.lpt_token_is_fresh().unwrap());

        // Invalid QDateTime -> never fresh.
        store.save_lpt_token("LPT", None).unwrap();
        assert!(!store.lpt_token_is_fresh().unwrap());

        // Future expiry but empty token -> not fresh (MemoryTokenStore::lptTokenIsFresh).
        store
            .save_lpt_token("", Some(Utc::now() + Duration::hours(1)))
            .unwrap();
        assert!(!store.lpt_token_is_fresh().unwrap());
    }

    #[test]
    fn memory_store_round_trips_stream_tokens_and_clears() {
        let store = MemoryTokenStore::new();
        assert_eq!(store.load_stream_tokens().unwrap(), StreamTokens::default());
        assert!(!store.load_stream_tokens().unwrap().is_valid());

        let tokens = sample_tokens();
        store.save_stream_tokens(&tokens).unwrap();
        assert_eq!(store.load_stream_tokens().unwrap(), tokens);
        assert!(store.load_stream_tokens().unwrap().is_valid());

        store.save_msa_token("MSA").unwrap();
        store.save_refresh_token("REFRESH").unwrap();
        store
            .save_lpt_token("LPT", Some(Utc::now() + Duration::hours(1)))
            .unwrap();
        store.clear_all().unwrap();

        assert_eq!(store.load_msa_token().unwrap(), "");
        assert_eq!(store.load_refresh_token().unwrap(), "");
        assert_eq!(store.load_lpt_token().unwrap(), "");
        assert!(!store.lpt_token_is_fresh().unwrap());
        assert_eq!(store.load_stream_tokens().unwrap(), StreamTokens::default());
        assert!(!store.has_session().unwrap());
    }

    #[test]
    fn memory_store_is_shareable_as_dyn() {
        let store: std::sync::Arc<dyn TokenStore> = std::sync::Arc::new(MemoryTokenStore::new());
        store.save_refresh_token("R").unwrap();
        assert!(store.has_session().unwrap());
    }

    #[test]
    fn stream_tokens_json_uses_cpp_keys() {
        let json = sample_tokens().to_json();
        assert_eq!(
            json,
            serde_json::json!({
                "xhomeToken": "XHOME",
                "xhomeHost": "xhome.gssv-play-prod.xboxlive.com",
                "xcloudToken": "XCLOUD",
                "xcloudHost": "xgpuweb.gssv-play-prod.xboxlive.com",
                "xcloudF2PToken": "F2P",
                "xcloudF2PHost": "xgpuwebf2p.gssv-play-prod.xboxlive.com",
                "webToken": "WEBXSTS",
                "webTokenUHS": "U",
                "xcloudRegions": [
                    {"name": "weu", "baseUri": "https://weu.gssv-play-prod.xboxlive.com", "isDefault": true},
                    {"name": "eus", "baseUri": "https://eus.gssv-play-prod.xboxlive.com", "isDefault": false}
                ]
            })
        );
        // serde derive and the hand-written codec agree.
        assert_eq!(serde_json::to_value(sample_tokens()).unwrap(), json);
        assert_eq!(
            serde_json::from_value::<StreamTokens>(json.clone()).unwrap(),
            sample_tokens()
        );
        assert_eq!(StreamTokens::from_json(&json), sample_tokens());
    }

    #[test]
    fn stream_tokens_from_json_tolerates_missing_and_mistyped_keys() {
        let value =
            serde_json::json!({"xcloudToken": "X", "xcloudRegions": "nope", "webTokenUHS": 5});
        let tokens = StreamTokens::from_json(&value);
        assert_eq!(tokens.xcloud_token, "X");
        assert_eq!(tokens.web_token_uhs, "");
        assert!(tokens.xcloud_regions.is_empty());
        assert!(tokens.is_valid());

        let region = LoginRegion::from_json(&serde_json::json!({"name": "weu"}));
        assert_eq!(
            region,
            LoginRegion {
                name: "weu".into(),
                ..LoginRegion::default()
            }
        );
    }

    #[test]
    fn preferred_region_honours_pin_then_default_then_first() {
        let regions = sample_tokens().xcloud_regions;
        assert_eq!(preferred_region(&regions, "eus").name, "eus");
        assert_eq!(preferred_region(&regions, "").name, "weu");
        assert_eq!(preferred_region(&regions, "nope").name, "weu");

        let no_default: Vec<LoginRegion> = regions
            .iter()
            .cloned()
            .map(|r| LoginRegion {
                is_default: false,
                ..r
            })
            .collect();
        assert_eq!(preferred_region(&no_default, "").name, "weu");
        assert_eq!(preferred_region(&[], "weu"), LoginRegion::default());
    }

    #[test]
    fn keyring_store_defaults_to_xcloud_rs_service() {
        assert_eq!(KeyringTokenStore::new().service(), "openxcloud");
        assert_eq!(KeyringTokenStore::default().service(), KEYRING_SERVICE);
        assert_eq!(KeyringTokenStore::with_service("other").service(), "other");
    }

    /// Needs a real keychain (and on macOS a possible unlock prompt); CI has none.
    /// Run with `cargo test -p xcloud-api -- --ignored keyring`.
    #[test]
    #[ignore = "requires an OS keychain"]
    fn keyring_store_round_trip() {
        let store =
            KeyringTokenStore::with_service(&format!("xcloud-rs-test-{}", uuid::Uuid::new_v4()));
        store.clear_all().unwrap();

        assert_eq!(store.load_msa_token().unwrap(), "");
        assert_eq!(store.load_refresh_token().unwrap(), "");
        assert!(!store.has_session().unwrap());
        assert!(!store.lpt_token_is_fresh().unwrap());
        assert_eq!(store.load_stream_tokens().unwrap(), StreamTokens::default());

        store.save_msa_token("MSA").unwrap();
        store.save_refresh_token("REFRESH").unwrap();
        store
            .save_lpt_token("LPT", Some(Utc::now() + Duration::hours(1)))
            .unwrap();
        store.save_stream_tokens(&sample_tokens()).unwrap();

        assert_eq!(store.load_msa_token().unwrap(), "MSA");
        assert_eq!(store.load_refresh_token().unwrap(), "REFRESH");
        assert!(store.has_session().unwrap());
        assert_eq!(store.load_lpt_token().unwrap(), "LPT");
        assert!(store.lpt_token_is_fresh().unwrap());
        assert_eq!(store.load_stream_tokens().unwrap(), sample_tokens());

        store
            .save_lpt_token("LPT", Some(Utc::now() - Duration::seconds(1)))
            .unwrap();
        assert!(!store.lpt_token_is_fresh().unwrap());
        store.save_lpt_token("LPT", None).unwrap();
        assert!(!store.lpt_token_is_fresh().unwrap());

        // Saving an empty value removes the entry.
        store.save_refresh_token("").unwrap();
        assert!(!store.has_session().unwrap());

        store.clear_all().unwrap();
        assert_eq!(store.load_msa_token().unwrap(), "");
        assert_eq!(store.load_lpt_token().unwrap(), "");
        assert_eq!(store.load_stream_tokens().unwrap(), StreamTokens::default());
        // Clearing twice is fine.
        store.clear_all().unwrap();
    }
}
