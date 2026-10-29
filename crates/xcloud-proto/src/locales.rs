//! The languages offered for a cloud session.
//!
//! The chosen locale is sent as `settings.locale` when a session starts (see
//! [`crate::preferences::make_stream_settings`]), which makes the cloud console run the
//! game in that language, and is also used as the store language and market for the
//! Game Pass catalog.

/// One selectable language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocaleInfo {
    /// BCP-47 tag sent to the service, e.g. `de-DE`.
    pub code: &'static str,
    /// The name in the language itself, e.g. `Deutsch - Deutschland`.
    pub native: &'static str,
    /// The name in English, e.g. `German (Germany)`.
    pub english: &'static str,
}

const fn l(code: &'static str, native: &'static str, english: &'static str) -> LocaleInfo {
    LocaleInfo {
        code,
        native,
        english,
    }
}

/// Every language offered, sorted by native name (the Xbox app's order).
pub const LOCALES: &[LocaleInfo] = &[
    l("cs-CZ", "Čeština - Česká republika", "Czech (Czechia)"),
    l("da-DK", "Dansk - Danmark", "Danish (Denmark)"),
    l("de-AT", "Deutsch - Österreich", "German (Austria)"),
    l("de-DE", "Deutsch - Deutschland", "German (Germany)"),
    l("de-CH", "Deutsch - Schweiz", "German (Switzerland)"),
    l("en-AU", "English - Australia", "English (Australia)"),
    l("en-CA", "English - Canada", "English (Canada)"),
    l("en-IN", "English - India", "English (India)"),
    l("en-IE", "English - Ireland", "English (Ireland)"),
    l("en-NZ", "English - New Zealand", "English (New Zealand)"),
    l("en-ZA", "English - South Africa", "English (South Africa)"),
    l(
        "en-GB",
        "English - United Kingdom",
        "English (United Kingdom)",
    ),
    l(
        "en-US",
        "English - United States",
        "English (United States)",
    ),
    l("es-AR", "Español - Argentina", "Spanish (Argentina)"),
    l("es-ES", "Español - España", "Spanish (Spain)"),
    l("es-MX", "Español - México", "Spanish (Mexico)"),
    l("fr-BE", "Français - Belgique", "French (Belgium)"),
    l("fr-CA", "Français - Canada", "French (Canada)"),
    l("fr-FR", "Français - France", "French (France)"),
    l("fr-CH", "Français - Suisse", "French (Switzerland)"),
    l("it-IT", "Italiano - Italia", "Italian (Italy)"),
    l("hu-HU", "Magyar - Magyarország", "Hungarian (Hungary)"),
    l("nl-BE", "Nederlands - België", "Dutch (Belgium)"),
    l("nl-NL", "Nederlands - Nederland", "Dutch (Netherlands)"),
    l("nb-NO", "Norsk bokmål - Norge", "Norwegian (Norway)"),
    l("pl-PL", "Polski - Polska", "Polish (Poland)"),
    l("pt-BR", "Português - Brasil", "Portuguese (Brazil)"),
    l("pt-PT", "Português - Portugal", "Portuguese (Portugal)"),
    l("sk-SK", "Slovenčina - Slovensko", "Slovak (Slovakia)"),
    l("fi-FI", "Suomi - Suomi", "Finnish (Finland)"),
    l("sv-SE", "Svenska - Sverige", "Swedish (Sweden)"),
    l("tr-TR", "Türkçe - Türkiye", "Turkish (Türkiye)"),
    l("el-GR", "Ελληνικά - Ελλάδα", "Greek (Greece)"),
    l("ru-RU", "Русский - Россия", "Russian (Russia)"),
    l("he-IL", "עברית - ישראל", "Hebrew (Israel)"),
    l(
        "ar-SA",
        "العربية - المملكة العربية السعودية",
        "Arabic (Saudi Arabia)",
    ),
    l("ko-KR", "한국어 - 대한민국", "Korean (Korea)"),
    l("ja-JP", "日本語 - 日本", "Japanese (Japan)"),
    l("zh-CN", "中文 - 中国", "Chinese (Simplified, China)"),
    l(
        "zh-HK",
        "中文 - 香港特別行政區",
        "Chinese (Traditional, Hong Kong SAR)",
    ),
    l("zh-TW", "中文 - 台灣", "Chinese (Traditional, Taiwan)"),
];

/// The default when nothing better is known.
pub const DEFAULT_LOCALE: &str = "en-US";

/// Looks up a locale by tag, case-insensitively and accepting `_` separators.
pub fn find(code: &str) -> Option<&'static LocaleInfo> {
    let wanted = code.trim().replace('_', "-");
    LOCALES
        .iter()
        .find(|l| l.code.eq_ignore_ascii_case(&wanted))
}

/// The closest supported locale for an arbitrary tag such as a system locale
/// (`de_DE`, `de-DE@calendar=…`, `pt`, `en-DE`): an exact match first, then the first
/// entry with the same language, else [`DEFAULT_LOCALE`].
pub fn closest(code: &str) -> &'static str {
    let tag = code
        .split(['@', '.'])
        .next()
        .unwrap_or("")
        .trim()
        .replace('_', "-");
    if let Some(found) = find(&tag) {
        return found.code;
    }
    let language = tag.split('-').next().unwrap_or("").to_ascii_lowercase();
    if language.is_empty() {
        return DEFAULT_LOCALE;
    }
    // Prefer the "home" country of the language when there are several.
    let preferred = match language.as_str() {
        "en" => "en-US",
        "de" => "de-DE",
        "es" => "es-ES",
        "fr" => "fr-FR",
        "pt" => "pt-BR",
        "nl" => "nl-NL",
        "zh" => "zh-CN",
        "nb" | "no" | "nn" => "nb-NO",
        _ => "",
    };
    if !preferred.is_empty() {
        return preferred;
    }
    LOCALES
        .iter()
        .find(|l| l.code.split('-').next() == Some(language.as_str()))
        .map_or(DEFAULT_LOCALE, |l| l.code)
}

/// The store market (two-letter region) for a locale: `de-DE` → `DE`.
pub fn market_for(code: &str) -> String {
    let code = code.trim();
    if !code.contains(['-', '_']) {
        return "US".to_owned();
    }
    code.rsplit(['-', '_'])
        .next()
        .filter(|r| r.len() == 2 && r.chars().all(|c| c.is_ascii_alphabetic()))
        .map(str::to_ascii_uppercase)
        .unwrap_or_else(|| "US".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_has_unique_well_formed_codes() {
        let mut seen = std::collections::HashSet::new();
        for l in LOCALES {
            assert!(seen.insert(l.code), "duplicate {}", l.code);
            let mut parts = l.code.split('-');
            assert_eq!(parts.next().map(str::len), Some(2), "{}", l.code);
            assert_eq!(parts.next().map(str::len), Some(2), "{}", l.code);
            assert!(!l.native.is_empty() && !l.english.is_empty());
        }
        assert!(find(DEFAULT_LOCALE).is_some());
    }

    #[test]
    fn find_is_lenient() {
        assert_eq!(find("de_de").map(|l| l.code), Some("de-DE"));
        assert_eq!(find(" EN-gb ").map(|l| l.code), Some("en-GB"));
        assert!(find("xx-YY").is_none());
    }

    #[test]
    fn closest_maps_system_locales() {
        assert_eq!(closest("de_DE"), "de-DE");
        assert_eq!(closest("de_AT"), "de-AT");
        assert_eq!(closest("en_DE"), "en-US");
        assert_eq!(closest("pt"), "pt-BR");
        assert_eq!(closest("sv"), "sv-SE");
        assert_eq!(closest("ja_JP@calendar=japanese"), "ja-JP");
        assert_eq!(closest("en_US.UTF-8"), "en-US");
        assert_eq!(closest("xx"), DEFAULT_LOCALE);
        assert_eq!(closest(""), DEFAULT_LOCALE);
    }

    #[test]
    fn market_is_the_region() {
        assert_eq!(market_for("de-DE"), "DE");
        assert_eq!(market_for("en_gb"), "GB");
        assert_eq!(market_for("en"), "US");
    }
}
