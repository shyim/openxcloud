//! Port of qt/src/core/Settings.{h,cpp}: persistence of [`AppSettings`] plus the settings
//! panel of docs/archive/PORTING_PLAN.md section 7.
//!
//! The Qt class was a typed wrapper over `QSettings` with stable string keys
//! (`hud/enabled`, `stream/resolutionProfile`, `input/deadzone`, ...). Here the same keys
//! become one JSON document, grouped the way `QSettings` grouped them:
//!
//! ```json
//! {
//!   "hud":    { "enabled": false },
//!   "stream": { "resolutionProfile": "1440p", "frameRate": 60, "locale": "en-US",
//!               "preferredRegion": "", "preferIPv6": false },
//!   "input":  { "deadzone": 0.15, "invertY": false, "swapAB": false,
//!               "triggerSensitivity": 0.5, "vibrationIntensity": 1.0 },
//!   "render": { "presentMode": "immediate", "upscaleEnabled": true, "sharpness": 0.25 }
//! }
//! ```
//!
//! The `render` group is new in the Rust port (`AppSettings::present_mode`,
//! `upscale_enabled`, `sharpness`). Every key is optional: a missing key reads as the
//! `Settings::*()` default exactly like `QSettings::value(key, default)` did, so an older
//! or hand-edited file keeps working. The file lives at
//! `ProjectDirs("com", "raylabsstudio", "xcloud-rs").config_dir()/settings.json`
//! (`~/Library/Application Support/com.raylabsstudio.xcloud-rs/settings.json` on macOS) and
//! is written atomically (temp file + rename) whenever a value changed, replacing
//! `QSettings::sync`.
//!
//! The resolution profile accepts the Qt spelling (`p1440`, `p1080HQ`) on load and is
//! normalised to the `AppSettings` spelling (`1440p`, `1080pHQ`) the combo box lists.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::state::AppSettings;

const LOG: &str = "Settings";

/// `ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)`.
pub const QUALIFIER: &str = "de";
pub const ORGANIZATION: &str = "shyim";
pub const APPLICATION: &str = "openxcloud";
/// File name under the config directory.
pub const SETTINGS_FILE_NAME: &str = "settings.json";

/// `Settings::resolutionProfile` values the panel offers (`StreamResolutionMode`).
pub const RESOLUTION_PROFILES: [&str; 5] = ["auto", "720p", "1080p", "1440p", "1080pHQ"];
/// `Settings::preferredFrameRate` values the panel offers.
pub const FRAME_RATES: [i32; 2] = [30, 60];
/// `AppSettings::present_mode` values: lowest latency first.
pub const PRESENT_MODES: [&str; 2] = ["immediate", "fifo"];
/// `AppSettings::upscaler` values (`xcloud_gpu::Upscaler::as_str`): FSR 1.0 (default) and
/// MetalFX (macOS with `--features metalfx`; the renderer falls back to FSR otherwise).
pub const UPSCALERS: [&str; 2] = ["fsr", "metalfx"];

/// Accessible labels of the panel's widgets (what the tests query).
pub const LABEL_RESOLUTION: &str = "Resolution profile";
pub const LABEL_FRAME_RATE: &str = "Frame rate";
pub const LABEL_REGION: &str = "Preferred region";
pub const LABEL_IPV6: &str = "Prefer IPv6";
pub const LABEL_DEADZONE: &str = "Stick deadzone";
pub const LABEL_INVERT_Y: &str = "Invert Y";
pub const LABEL_SWAP_AB: &str = "Swap A/B";
pub const LABEL_TRIGGER: &str = "Trigger sensitivity";
pub const LABEL_VIBRATION: &str = "Vibration";
pub const LABEL_PRESENT: &str = "Present mode";
pub const LABEL_UPSCALE: &str = "Upscaler";
pub const LABEL_UPSCALER_KIND: &str = "Upscaling method";
pub const LABEL_SHARPNESS: &str = "Sharpness";

// ----------------------------------------------------------------------------- paths

/// The platform directories of this app.
pub fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
}

/// `config_dir()/settings.json`, `None` when the platform has no home directory.
pub fn settings_path() -> Option<PathBuf> {
    project_dirs().map(|dirs| dirs.config_dir().join(SETTINGS_FILE_NAME))
}

/// `QStandardPaths::AppDataLocation`: where the log file goes.
pub fn data_dir() -> Option<PathBuf> {
    project_dirs().map(|dirs| dirs.data_dir().to_path_buf())
}

// ----------------------------------------------------------------------------- document

/// The on-disk document: one object per `QSettings` group, keys as in `Settings.cpp`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct SettingsFile {
    hud: HudGroup,
    stream: StreamGroup,
    input: InputGroup,
    render: RenderGroup,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct HudGroup {
    /// `hud/enabled`.
    enabled: Option<bool>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StreamGroup {
    /// `stream/resolutionProfile`.
    resolution_profile: Option<String>,
    /// `stream/frameRate`.
    frame_rate: Option<i32>,
    /// `stream/locale`.
    locale: Option<String>,
    /// `stream/preferredRegion`.
    preferred_region: Option<String>,
    /// `stream/preferIPv6`.
    #[serde(rename = "preferIPv6")]
    prefer_ipv6: Option<bool>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct InputGroup {
    /// `input/deadzone`.
    deadzone: Option<f64>,
    /// `input/invertY`.
    invert_y: Option<bool>,
    /// `input/swapAB`.
    #[serde(rename = "swapAB")]
    swap_ab: Option<bool>,
    /// `input/triggerSensitivity`.
    trigger_sensitivity: Option<f64>,
    /// `input/vibrationIntensity`.
    vibration_intensity: Option<f64>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RenderGroup {
    /// `render/presentMode` (new).
    present_mode: Option<String>,
    /// `render/upscaleEnabled` (new).
    upscale_enabled: Option<bool>,
    upscale_height: Option<u32>,
    /// `render/sharpness` (new).
    sharpness: Option<f32>,
    /// `render/upscaler` (new): one of [`UPSCALERS`].
    upscaler: Option<String>,
}

impl SettingsFile {
    fn from_settings(s: &AppSettings) -> Self {
        Self {
            hud: HudGroup {
                enabled: Some(s.hud_enabled),
            },
            stream: StreamGroup {
                resolution_profile: Some(s.resolution_profile.clone()),
                frame_rate: Some(s.preferred_frame_rate),
                locale: Some(s.locale.clone()),
                preferred_region: Some(s.preferred_region.clone()),
                prefer_ipv6: Some(s.prefer_ipv6),
            },
            input: InputGroup {
                deadzone: Some(s.deadzone),
                invert_y: Some(s.invert_y),
                swap_ab: Some(s.swap_ab),
                trigger_sensitivity: Some(s.trigger_sensitivity),
                vibration_intensity: Some(s.vibration_intensity),
            },
            render: RenderGroup {
                present_mode: Some(s.present_mode.clone()),
                upscale_enabled: Some(s.upscale_enabled),
                upscale_height: Some(normalize_upscale_height(s.upscale_height)),
                sharpness: Some(s.sharpness),
                upscaler: Some(s.upscaler.clone()),
            },
        }
    }

    /// Every missing key takes the `AppSettings::default()` value
    /// (`QSettings::value(key, default)`).
    fn into_settings(self) -> AppSettings {
        let d = AppSettings::default();
        AppSettings {
            hud_enabled: self.hud.enabled.unwrap_or(d.hud_enabled),
            resolution_profile: normalize_profile(
                self.stream
                    .resolution_profile
                    .as_deref()
                    .unwrap_or(&d.resolution_profile),
            ),
            preferred_frame_rate: self.stream.frame_rate.unwrap_or(d.preferred_frame_rate),
            locale: self.stream.locale.unwrap_or(d.locale),
            preferred_region: self.stream.preferred_region.unwrap_or(d.preferred_region),
            prefer_ipv6: self.stream.prefer_ipv6.unwrap_or(d.prefer_ipv6),
            deadzone: self.input.deadzone.unwrap_or(d.deadzone),
            invert_y: self.input.invert_y.unwrap_or(d.invert_y),
            swap_ab: self.input.swap_ab.unwrap_or(d.swap_ab),
            trigger_sensitivity: self
                .input
                .trigger_sensitivity
                .unwrap_or(d.trigger_sensitivity),
            vibration_intensity: self
                .input
                .vibration_intensity
                .unwrap_or(d.vibration_intensity),
            present_mode: normalize_present_mode(
                self.render
                    .present_mode
                    .as_deref()
                    .unwrap_or(&d.present_mode),
            ),
            upscale_enabled: self.render.upscale_enabled.unwrap_or(d.upscale_enabled),
            upscale_height: normalize_upscale_height(
                self.render.upscale_height.unwrap_or(d.upscale_height),
            ),
            sharpness: self.render.sharpness.unwrap_or(d.sharpness),
            upscaler: normalize_upscaler(self.render.upscaler.as_deref().unwrap_or(&d.upscaler)),
        }
    }
}

/// `Settings::resolutionProfile` → the spelling [`RESOLUTION_PROFILES`] uses. Accepts the
/// Qt spelling (`p1440`, `p1080HQ`), case-insensitively. Anything unknown becomes
/// `1080p`, which is what `services::resolution_from_profile` streams for it.
/// The system language as a raw locale identifier (`de_DE`), when available.
#[cfg(target_os = "macos")]
pub fn system_locale_identifier() -> Option<String> {
    let locale = objc2_core_foundation::CFLocale::current()?;
    let identifier = locale.identifier()?;
    Some(identifier.to_string())
}

#[cfg(not(target_os = "macos"))]
pub fn system_locale_identifier() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"].iter().find_map(|k| {
        std::env::var(k)
            .ok()
            .filter(|v| !v.is_empty() && v != "C" && v != "POSIX")
    })
}

/// The supported locale closest to the system language (`en-US` when unknown).
pub fn system_locale() -> String {
    system_locale_identifier()
        .map(|id| xcloud_proto::locales::closest(&id).to_owned())
        .unwrap_or_else(|| xcloud_proto::locales::DEFAULT_LOCALE.to_owned())
}

/// The `stream/locale` value stored in the settings file at `path`, if any.
pub fn stored_locale(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let file: SettingsFile = serde_json::from_str(&text).ok()?;
    file.stream.locale.filter(|l| !l.trim().is_empty())
}

/// A stored locale normalised to a supported tag; unknown values fall back to the
/// closest supported language.
pub fn normalize_locale(locale: &str) -> String {
    xcloud_proto::locales::closest(locale).to_owned()
}

pub fn normalize_profile(profile: &str) -> String {
    match profile.trim().to_ascii_lowercase().as_str() {
        "auto" => "auto",
        "720p" | "p720" => "720p",
        "1080p" | "p1080" => "1080p",
        "1440p" | "p1440" => "1440p",
        "1080phq" | "p1080hq" => "1080pHQ",
        _ => "1080p",
    }
    .to_owned()
}

/// `AppSettings::present_mode` → one of [`PRESENT_MODES`]; unknown values become the
/// default (`immediate`).
pub fn normalize_present_mode(name: &str) -> String {
    match name.trim().to_ascii_lowercase().as_str() {
        "fifo" | "vsync" => "fifo",
        _ => "immediate",
    }
    .to_owned()
}

/// Supported output targets; preserve the historical 4K default for unknown values.
pub fn normalize_upscale_height(height: u32) -> u32 {
    match height {
        1080 | 1440 | 2160 => height,
        _ => 2160,
    }
}

/// `AppSettings::upscaler` → one of [`UPSCALERS`]. Accepts what `xcloud_gpu::Upscaler::parse`
/// accepts for the two (`fsr`, `fsr1`, `metalfx`, `metal-fx`, `fx`), case-insensitively;
/// anything else (including `bypass`, which is `upscale_enabled = false` here) becomes `fsr`.
pub fn normalize_upscaler(name: &str) -> String {
    match name.trim().to_ascii_lowercase().as_str() {
        "metalfx" | "metal-fx" | "fx" => "metalfx",
        _ => "fsr",
    }
    .to_owned()
}

/// The wgpu present mode for an `AppSettings::present_mode` value (`None` when unknown).
pub fn present_mode_from_name(name: &str) -> Option<wgpu::PresentMode> {
    match name.trim().to_ascii_lowercase().as_str() {
        "fifo" | "vsync" => Some(wgpu::PresentMode::Fifo),
        "immediate" => Some(wgpu::PresentMode::Immediate),
        _ => None,
    }
}

/// Serialises `settings` as the grouped JSON document.
pub fn to_json(settings: &AppSettings) -> String {
    // A struct of plain scalars and strings cannot fail to serialise.
    serde_json::to_string_pretty(&SettingsFile::from_settings(settings))
        .unwrap_or_else(|_| "{}".to_owned())
}

/// Parses the grouped JSON document; missing keys take their defaults.
pub fn from_json(text: &str) -> serde_json::Result<AppSettings> {
    let file: SettingsFile = serde_json::from_str(text)?;
    Ok(file.into_settings())
}

/// Reads `path`. A missing file is the defaults; an unreadable or malformed file is
/// logged and also yields the defaults, like a `QSettings` file that failed to parse.
pub fn load_from(path: &Path) -> AppSettings {
    match fs::read_to_string(path) {
        Ok(text) => match from_json(&text) {
            Ok(settings) => settings,
            Err(error) => {
                tracing::warn!(target: LOG, "settings file {} is malformed: {error}; using defaults", path.display());
                AppSettings::default()
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => AppSettings::default(),
        Err(error) => {
            tracing::warn!(target: LOG, "cannot read settings file {}: {error}; using defaults", path.display());
            AppSettings::default()
        }
    }
}

/// Writes `settings` to `path` atomically: the document goes to `<path>.tmp` first and is
/// renamed over the target, so a crash mid-write never leaves a truncated file.
pub fn save_to(path: &Path, settings: &AppSettings) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, to_json(settings))?;
    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

// ----------------------------------------------------------------------------- store

/// The persisted settings: what is on disk plus the path. The app shell compares the live
/// [`AppSettings`] against the last saved copy once per frame and saves on change.
#[derive(Clone)]
pub struct SettingsStore {
    path: Option<PathBuf>,
    saved: AppSettings,
    /// Test seam: called right before the disk write of a changed value, so a test can
    /// check what the caller holds (or does not hold) while the store blocks on I/O.
    #[cfg(test)]
    pub(crate) save_probe: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl fmt::Debug for SettingsStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsStore")
            .field("path", &self.path)
            .field("saved", &self.saved)
            .finish()
    }
}

impl SettingsStore {
    /// Loads from [`settings_path`]. Without a config directory the store is in-memory
    /// only (everything else keeps working).
    ///
    /// Until the user picks a language, the locale follows the system language.
    pub fn open() -> Self {
        let mut store = match settings_path() {
            Some(path) => Self::at(path),
            None => {
                tracing::warn!(target: LOG, "no config directory on this platform; settings are not persisted");
                Self::in_memory(AppSettings::default())
            }
        };
        let stored = store.path.as_deref().and_then(stored_locale);
        store.saved.locale = match stored {
            Some(locale) => normalize_locale(&locale),
            None => system_locale(),
        };
        tracing::info!(target: LOG, "locale {}", store.saved.locale);
        store
    }

    /// Loads from an explicit file.
    pub fn at(path: PathBuf) -> Self {
        let saved = load_from(&path);
        tracing::info!(target: LOG, "settings loaded from {}", path.display());
        Self {
            path: Some(path),
            saved,
            #[cfg(test)]
            save_probe: None,
        }
    }

    /// A store that never touches the disk (smoke mode, tests).
    pub fn in_memory(settings: AppSettings) -> Self {
        Self {
            path: None,
            saved: settings,
            #[cfg(test)]
            save_probe: None,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The settings as loaded or last saved.
    pub fn settings(&self) -> &AppSettings {
        &self.saved
    }

    /// Saves `current` when it differs from the last saved copy. Returns whether a save
    /// happened. An in-memory store just remembers the value.
    pub fn save_if_changed(&mut self, current: &AppSettings) -> io::Result<bool> {
        if *current == self.saved {
            return Ok(false);
        }
        if let Some(path) = &self.path {
            #[cfg(test)]
            if let Some(probe) = &self.save_probe {
                probe();
            }
            save_to(path, current)?;
            tracing::debug!(target: LOG, "settings saved to {}", path.display());
        }
        self.saved = current.clone();
        Ok(true)
    }
}

// The settings UI lives in `crate::ui::settings_sheet`; the `LABEL_*` names above are
// its accessible labels.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_path(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "xcloud-rs-settings-{}-{n}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn defaults_round_trip_through_json() {
        let settings = AppSettings::default();
        let parsed = from_json(&to_json(&settings)).expect("valid json");
        assert_eq!(parsed, settings);
    }

    #[test]
    fn every_field_round_trips() {
        let settings = AppSettings {
            hud_enabled: true,
            resolution_profile: "1080pHQ".into(),
            preferred_frame_rate: 30,
            locale: "de-DE".into(),
            preferred_region: "weu".into(),
            prefer_ipv6: true,
            deadzone: 0.2,
            invert_y: true,
            swap_ab: true,
            trigger_sensitivity: 0.75,
            vibration_intensity: 0.5,
            present_mode: "fifo".into(),
            upscale_enabled: false,
            upscale_height: 1440,
            sharpness: 1.5,
            upscaler: "metalfx".into(),
        };
        let parsed = from_json(&to_json(&settings)).expect("valid json");
        assert_eq!(parsed, settings);
    }

    #[test]
    fn output_target_defaults_and_rejects_unsupported_sizes() {
        for height in [1080, 1440, 2160] {
            let json = format!(r#"{{"render":{{"upscaleHeight":{height}}}}}"#);
            assert_eq!(from_json(&json).unwrap().upscale_height, height);
        }
        assert_eq!(from_json("{}").unwrap().upscale_height, 2160);
        assert_eq!(
            from_json(r#"{"render":{"upscaleHeight":99999}}"#)
                .unwrap()
                .upscale_height,
            2160
        );
    }

    #[test]
    fn upscaler_names_are_normalised() {
        assert_eq!(normalize_upscaler("MetalFX"), "metalfx");
        assert_eq!(normalize_upscaler(" fx "), "metalfx");
        assert_eq!(normalize_upscaler("fsr"), "fsr");
        assert_eq!(
            normalize_upscaler("bypass"),
            "fsr",
            "off is `upscale_enabled`"
        );
        assert_eq!(normalize_upscaler("bogus"), "fsr");
        let parsed = from_json(r#"{"render":{"upscaler":"metal-fx"}}"#).expect("valid json");
        assert_eq!(parsed.upscaler, "metalfx");
        assert_eq!(from_json("{}").expect("valid json").upscaler, "fsr");
    }

    #[test]
    fn json_uses_the_qsettings_group_keys() {
        let json: serde_json::Value =
            serde_json::from_str(&to_json(&AppSettings::default())).expect("valid json");
        // core/Settings.cpp keys, group → object.
        assert_eq!(json["hud"]["enabled"], false);
        assert_eq!(json["stream"]["resolutionProfile"], "1440p");
        assert_eq!(json["stream"]["frameRate"], 60);
        assert_eq!(json["stream"]["locale"], "en-US");
        assert_eq!(json["stream"]["preferredRegion"], "");
        assert_eq!(json["stream"]["preferIPv6"], false);
        assert_eq!(json["input"]["deadzone"], 0.15);
        assert_eq!(json["input"]["invertY"], false);
        assert_eq!(json["input"]["swapAB"], false);
        assert_eq!(json["input"]["triggerSensitivity"], 0.5);
        assert_eq!(json["input"]["vibrationIntensity"], 1.0);
        assert_eq!(json["render"]["presentMode"], "immediate");
        assert_eq!(json["render"]["upscaleEnabled"], true);
        assert_eq!(json["render"]["upscaler"], "fsr");
    }

    #[test]
    fn missing_keys_fall_back_to_the_qt_defaults() {
        let parsed = from_json(r#"{"stream":{"frameRate":30},"input":{"invertY":true}}"#)
            .expect("valid json");
        let d = AppSettings::default();
        assert_eq!(parsed.preferred_frame_rate, 30);
        assert!(parsed.invert_y);
        assert_eq!(parsed.resolution_profile, "1440p");
        assert_eq!(parsed.trigger_sensitivity, d.trigger_sensitivity);
        assert_eq!(parsed.deadzone, d.deadzone);
        assert_eq!(parsed.present_mode, d.present_mode);
        assert_eq!(parsed.sharpness, d.sharpness);

        let empty = from_json("{}").expect("valid json");
        assert_eq!(empty, d);
    }

    #[test]
    fn qt_profile_spelling_is_normalised_on_load() {
        let parsed = from_json(r#"{"stream":{"resolutionProfile":"p1080HQ"}}"#).expect("json");
        assert_eq!(parsed.resolution_profile, "1080pHQ");
        assert_eq!(normalize_profile("p1440"), "1440p");
        assert_eq!(normalize_profile(" P720 "), "720p");
        assert_eq!(normalize_profile("Auto"), "auto");
        assert_eq!(normalize_profile("bogus"), "1080p");
    }

    #[test]
    fn present_mode_names() {
        assert_eq!(
            present_mode_from_name("fifo"),
            Some(wgpu::PresentMode::Fifo)
        );
        assert_eq!(
            present_mode_from_name("Immediate"),
            Some(wgpu::PresentMode::Immediate)
        );
        assert_eq!(present_mode_from_name("mailbox"), None);
        assert_eq!(normalize_present_mode("FIFO"), "fifo");
        assert_eq!(normalize_present_mode("whatever"), "immediate");
    }

    #[test]
    fn missing_file_and_malformed_file_yield_defaults() {
        let path = temp_path("missing").join("settings.json");
        assert_eq!(load_from(&path), AppSettings::default());

        let path = temp_path("malformed.json");
        fs::write(&path, "{ not json").expect("write");
        assert_eq!(load_from(&path), AppSettings::default());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn save_is_atomic_and_loads_back() {
        let dir = temp_path("save");
        let path = dir.join("nested").join("settings.json");
        let settings = AppSettings {
            invert_y: true,
            preferred_region: "weu".into(),
            ..AppSettings::default()
        };
        save_to(&path, &settings).expect("save");
        assert_eq!(load_from(&path), settings);
        // No temp file left behind.
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        assert!(!PathBuf::from(tmp).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_saves_only_on_change() {
        let dir = temp_path("store");
        let path = dir.join("settings.json");
        let mut store = SettingsStore::at(path.clone());
        assert_eq!(*store.settings(), AppSettings::default());
        assert!(!path.exists(), "opening never writes");

        let unchanged = AppSettings::default();
        assert!(!store.save_if_changed(&unchanged).expect("save"));
        assert!(!path.exists());

        let changed = AppSettings {
            hud_enabled: true,
            ..AppSettings::default()
        };
        assert!(store.save_if_changed(&changed).expect("save"));
        assert_eq!(load_from(&path), changed);
        assert_eq!(*store.settings(), changed);
        assert!(!store.save_if_changed(&changed).expect("save"));

        // A fresh store reads what was saved.
        let reopened = SettingsStore::at(path);
        assert_eq!(*reopened.settings(), changed);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn in_memory_store_remembers_without_disk() {
        let mut store = SettingsStore::in_memory(AppSettings::default());
        assert!(store.path().is_none());
        let changed = AppSettings {
            swap_ab: true,
            ..AppSettings::default()
        };
        assert!(store.save_if_changed(&changed).expect("in memory"));
        assert_eq!(*store.settings(), changed);
    }
}
