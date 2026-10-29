//! Port of qt/src/app/LibraryFilter.{h,cpp}
//! (plus the filter/sort pass of `LibraryController::rebuildFiltered`).
//!
//! Browse filter/sort state. The Qt class is a `QObject` with `Q_PROPERTY`s and a
//! `changed()` signal; here it is a plain value type whose setters return `true`
//! when the value changed so the caller can rebuild its filtered list.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::models::{CloudLibraryItem, LibraryAttribute};

/// The default sort: recently played titles first, then alphabetical.
pub const SORT_RECENT_FIRST: &str = "recentFirst";
/// Pure alphabetical sort.
pub const SORT_ALPHABETICAL: &str = "alphabetical";

/// Library filter state. Mirrors the Swift LibraryFilter value type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LibraryFilter {
    pub recent_only: bool,
    pub touch_only: bool,
    /// Localized store genre; empty means any.
    pub genre: String,
    /// Normalised feature name (see [`normalized_feature`]); empty means any.
    pub feature: String,
    /// Minimum advertised player count; 0 means any.
    pub min_players: u32,
    /// [`SORT_RECENT_FIRST`] or [`SORT_ALPHABETICAL`].
    pub sort: String,
}

impl Default for LibraryFilter {
    fn default() -> Self {
        Self {
            recent_only: false,
            touch_only: false,
            genre: String::new(),
            feature: String::new(),
            min_players: 0,
            sort: SORT_RECENT_FIRST.to_owned(),
        }
    }
}

impl LibraryFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn recent_only(&self) -> bool {
        self.recent_only
    }

    /// Returns `true` when the value changed (the Qt `changed()` signal).
    pub fn set_recent_only(&mut self, value: bool) -> bool {
        set_if_changed(&mut self.recent_only, value)
    }

    pub fn touch_only(&self) -> bool {
        self.touch_only
    }

    pub fn set_touch_only(&mut self, value: bool) -> bool {
        set_if_changed(&mut self.touch_only, value)
    }

    pub fn feature(&self) -> &str {
        &self.feature
    }

    pub fn set_feature(&mut self, value: impl Into<String>) -> bool {
        set_if_changed(&mut self.feature, value.into())
    }

    pub fn min_players(&self) -> u32 {
        self.min_players
    }

    pub fn set_min_players(&mut self, value: u32) -> bool {
        set_if_changed(&mut self.min_players, value)
    }

    pub fn sort(&self) -> &str {
        &self.sort
    }

    pub fn set_sort(&mut self, value: impl Into<String>) -> bool {
        set_if_changed(&mut self.sort, value.into())
    }

    /// True when any filter or a non-default sort is set.
    pub fn is_active(&self) -> bool {
        self.recent_only
            || self.touch_only
            || !self.genre.is_empty()
            || !self.feature.is_empty()
            || self.min_players > 0
            || self.sort != SORT_RECENT_FIRST
    }

    /// Resets every field. Returns `true` when the filter was active.
    pub fn clear(&mut self) -> bool {
        let changed = self.is_active();
        *self = Self::default();
        changed
    }

    /// True when the sort puts recently played titles first.
    pub fn recent_first(&self) -> bool {
        self.sort == SORT_RECENT_FIRST
    }

    /// Returns true when `item` satisfies every active filter.
    pub fn matches(&self, item: &CloudLibraryItem) -> bool {
        if !self.genre.is_empty() && !item.categories.iter().any(|genre| genre == &self.genre) {
            return false;
        }
        if self.recent_only && !item.is_in_mru {
            return false;
        }
        if self.touch_only
            && !item
                .supported_input_types
                .iter()
                .any(|input| input.to_lowercase().contains("touch"))
        {
            return false;
        }
        if !self.feature.is_empty()
            && !item
                .attributes
                .iter()
                .any(|attribute| normalized_feature(&attribute.localized_name) == self.feature)
        {
            return false;
        }
        if self.min_players > 0 && max_players_for(&item.attributes) < self.min_players {
            return false;
        }
        true
    }

    /// The filter + sort pass of `LibraryController::rebuildFiltered`: keeps the
    /// items whose name contains the trimmed `query` (case-insensitive) and that
    /// pass [`matches`](Self::matches), then sorts them with [`sort_items`](Self::sort_items).
    pub fn apply(&self, items: &[CloudLibraryItem], query: &str) -> Vec<CloudLibraryItem> {
        let trimmed = query.trim().to_lowercase();
        let mut filtered: Vec<CloudLibraryItem> = items
            .iter()
            .filter(|item| trimmed.is_empty() || item.name.to_lowercase().contains(&trimmed))
            .filter(|item| self.matches(item))
            .cloned()
            .collect();
        self.sort_items(&mut filtered);
        filtered
    }

    /// Stable sort: recently played first when the sort is [`SORT_RECENT_FIRST`],
    /// then by name, case-insensitively.
    pub fn sort_items(&self, items: &mut [CloudLibraryItem]) {
        let recent_first = self.recent_first();
        items.sort_by(|lhs, rhs| {
            if recent_first && lhs.is_in_mru != rhs.is_in_mru {
                return rhs.is_in_mru.cmp(&lhs.is_in_mru);
            }
            compare_names(&lhs.name, &rhs.name)
        });
    }

    /// "Online co-op (2-12)" becomes "Online co-op".
    pub fn normalized_feature(localized_name: &str) -> String {
        normalized_feature(localized_name)
    }

    /// Largest player count advertised by any multiplayer or co-op attribute of a
    /// card map (`{"attributes": [{"localizedName": "..."}]}`), at least 1.
    pub fn max_players(item: &Value) -> u32 {
        max_players(item)
    }
}

fn set_if_changed<T: PartialEq>(slot: &mut T, value: T) -> bool {
    if *slot == value {
        return false;
    }
    *slot = value;
    true
}

/// Case-insensitive name ordering used by every library sort.
pub fn compare_names(lhs: &str, rhs: &str) -> std::cmp::Ordering {
    lhs.to_lowercase().cmp(&rhs.to_lowercase())
}

/// `LibraryFilter::normalizedFeature`: removes a trailing parenthesised suffix
/// (`\s*\([^)]*\)\s*$`) and trims. "Online co-op (2-12)" becomes "Online co-op".
pub fn normalized_feature(localized_name: &str) -> String {
    let trimmed = localized_name.trim_end();
    if let Some(inner) = trimmed.strip_suffix(')')
        && let Some(open) = inner.rfind('(')
        && !inner[open + 1..].contains(')')
    {
        return trimmed[..open].trim().to_owned();
    }
    trimmed.trim().to_owned()
}

/// Finds the first `(N)` or `(N-M)` group (`\((\d+)(?:\s*-\s*(\d+))?\)`) and
/// returns both counts (`(N)` yields `(N, N)`).
pub fn player_range(text: &str) -> Option<(u32, u32)> {
    let bytes = text.as_bytes();
    let mut start = 0;
    while let Some(offset) = text[start..].find('(') {
        let open = start + offset;
        if let Some(range) = parse_player_range(bytes, open + 1) {
            return Some(range);
        }
        start = open + 1;
    }
    None
}

fn parse_player_range(bytes: &[u8], mut index: usize) -> Option<(u32, u32)> {
    let first = take_digits(bytes, &mut index)?;
    let second = if bytes.get(index) == Some(&b')') {
        first
    } else {
        skip_spaces(bytes, &mut index);
        if bytes.get(index) != Some(&b'-') {
            return None;
        }
        index += 1;
        skip_spaces(bytes, &mut index);
        let second = take_digits(bytes, &mut index)?;
        if bytes.get(index) != Some(&b')') {
            return None;
        }
        second
    };
    Some((first, second))
}

fn take_digits(bytes: &[u8], index: &mut usize) -> Option<u32> {
    let start = *index;
    while bytes.get(*index).is_some_and(u8::is_ascii_digit) {
        *index += 1;
    }
    if *index == start {
        return None;
    }
    // Only ASCII digits were consumed, so this is valid UTF-8; saturate on overflow like
    // QString::toInt would return 0 on overflow but a huge count is never meaningful.
    std::str::from_utf8(&bytes[start..*index])
        .ok()
        .map(|digits| digits.parse::<u32>().unwrap_or(u32::MAX))
}

fn skip_spaces(bytes: &[u8], index: &mut usize) {
    while bytes.get(*index).is_some_and(u8::is_ascii_whitespace) {
        *index += 1;
    }
}

/// Largest player count advertised by any attribute, at least 1.
pub fn max_players_for(attributes: &[LibraryAttribute]) -> u32 {
    attributes
        .iter()
        .filter_map(|attribute| player_range(&attribute.localized_name))
        .map(|(first, second)| first.max(second))
        .fold(1, u32::max)
}

/// `LibraryFilter::maxPlayers`: the card-map variant of [`max_players_for`].
pub fn max_players(item: &Value) -> u32 {
    item.get("attributes")
        .and_then(Value::as_array)
        .map(|attributes| {
            attributes
                .iter()
                .filter_map(|value| value.get("localizedName").and_then(Value::as_str))
                .filter_map(player_range)
                .map(|(first, second)| first.max(second))
                .fold(1, u32::max)
        })
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    //! Port of qt/tests/tst_libraryfilter.cpp
    use serde_json::json;

    use super::*;

    fn make_item(
        name: &str,
        mru: bool,
        inputs: &[&str],
        attributes: Vec<LibraryAttribute>,
    ) -> CloudLibraryItem {
        CloudLibraryItem {
            title_id: name.to_owned(),
            product_id: name.to_owned(),
            name: name.to_owned(),
            is_in_mru: mru,
            supported_input_types: inputs.iter().map(|input| (*input).to_owned()).collect(),
            attributes,
            ..Default::default()
        }
    }

    #[test]
    fn genre_combines_with_search_and_input_and_clears() {
        let mut game = make_item("Racing game", false, &["touch"], vec![]);
        game.categories = vec!["Racing".into()];
        let mut filter = LibraryFilter {
            genre: "Racing".into(),
            touch_only: true,
            ..Default::default()
        };
        assert_eq!(filter.apply(&[game.clone()], "racing").len(), 1);
        assert!(filter.apply(&[game.clone()], "shooter").is_empty());
        game.supported_input_types.clear();
        assert!(!filter.matches(&game));
        game.categories.clear();
        filter.touch_only = false;
        assert!(!filter.matches(&game));
        assert!(filter.clear());
        assert!(filter.matches(&game));
        let old: LibraryFilter = serde_json::from_str(r#"{"touchOnly":true}"#).unwrap();
        assert!(old.genre.is_empty());
    }

    #[test]
    fn normalized_feature_strips_counts() {
        assert_eq!(
            LibraryFilter::normalized_feature("Online co-op (2-12)"),
            "Online co-op"
        );
        assert_eq!(
            LibraryFilter::normalized_feature("4K Ultra HD"),
            "4K Ultra HD"
        );
        assert_eq!(LibraryFilter::normalized_feature("  HDR (HDR10)  "), "HDR");
    }

    #[test]
    fn recent_filter() {
        let mut filter = LibraryFilter::new();
        assert!(filter.set_recent_only(true));
        assert!(filter.matches(&make_item("A", true, &[], vec![])));
        assert!(!filter.matches(&make_item("B", false, &[], vec![])));
        assert!(filter.is_active());
    }

    #[test]
    fn touch_filter() {
        let mut filter = LibraryFilter::new();
        filter.set_touch_only(true);
        assert!(filter.matches(&make_item("A", false, &["Touch"], vec![])));
        assert!(!filter.matches(&make_item("B", false, &["Controller"], vec![])));
    }

    #[test]
    fn feature_filter() {
        let mut filter = LibraryFilter::new();
        filter.set_feature("Online co-op");
        let attributes = vec![LibraryAttribute::new("capability", "Online co-op (2-12)")];
        assert!(filter.matches(&make_item("A", false, &[], attributes)));
        assert!(!filter.matches(&make_item("B", false, &[], vec![])));
    }

    #[test]
    fn player_filter() {
        let mut filter = LibraryFilter::new();
        filter.set_min_players(4);
        let attributes = vec![LibraryAttribute::new("capability", "Online co-op (2-8)")];
        assert!(filter.matches(&make_item("A", false, &[], attributes.clone())));

        filter.set_min_players(16);
        assert!(!filter.matches(&make_item("A", false, &[], attributes)));

        let map = json!({"attributes": [{"localizedName": "Online co-op (2-12)"}]});
        assert_eq!(LibraryFilter::max_players(&map), 12);
    }

    #[test]
    fn clear_resets_state() {
        let mut filter = LibraryFilter::new();
        filter.set_recent_only(true);
        filter.set_min_players(4);
        filter.set_feature("HDR");
        filter.set_sort("alphabetical");
        assert!(filter.is_active());
        assert!(filter.clear());
        assert!(!filter.is_active());
        assert_eq!(filter.min_players(), 0);
        assert!(filter.feature().is_empty());
        assert_eq!(filter.sort(), SORT_RECENT_FIRST);
        // Clearing an inactive filter reports no change.
        assert!(!filter.clear());
    }

    #[test]
    fn setters_report_changes() {
        let mut filter = LibraryFilter::new();
        assert!(!filter.set_recent_only(false));
        assert!(filter.set_recent_only(true));
        assert!(!filter.set_sort(SORT_RECENT_FIRST));
        assert!(filter.set_sort(SORT_ALPHABETICAL));
        assert!(!filter.set_sort(SORT_ALPHABETICAL));
        assert!(filter.set_feature("HDR"));
        assert!(!filter.set_feature("HDR"));
        assert!(filter.set_min_players(2));
        assert!(!filter.set_min_players(2));
        assert!(filter.set_touch_only(true));
        assert!(!filter.set_touch_only(true));
    }

    #[test]
    fn player_range_parsing() {
        assert_eq!(player_range("Online co-op (2-12)"), Some((2, 12)));
        assert_eq!(player_range("Local multiplayer (4)"), Some((4, 4)));
        assert_eq!(player_range("Co-op (2 - 4)"), Some((2, 4)));
        assert_eq!(player_range("4K Ultra HD"), None);
        assert_eq!(player_range("HDR (HDR10)"), None);
        assert_eq!(player_range("(x) then (2-3)"), Some((2, 3)));
        assert_eq!(player_range("(2-)"), None);
        assert_eq!(max_players_for(&[]), 1);
        assert_eq!(
            max_players_for(&[
                LibraryAttribute::new("a", "Online co-op (2-8)"),
                LibraryAttribute::new("b", "Local co-op (12)"),
            ]),
            12
        );
        assert_eq!(max_players(&json!({})), 1);
        assert_eq!(max_players(&json!({"attributes": [{"name": "x"}]})), 1);
    }

    #[test]
    fn apply_filters_queries_and_sorts() {
        // Mirrors the rebuildFiltered expectations in tst_librarycontroller.cpp:
        // recent titles lead, then alphabetical (case-insensitive); the query is
        // a case-insensitive substring match on the name.
        let items = vec![
            make_item("beta", false, &["Touch"], vec![]),
            make_item("Alpha Deluxe", true, &["Controller"], vec![]),
            make_item("gamma", false, &[], vec![]),
        ];
        let filter = LibraryFilter::new();
        let names: Vec<String> = filter
            .apply(&items, "")
            .into_iter()
            .map(|item| item.name)
            .collect();
        assert_eq!(names, vec!["Alpha Deluxe", "beta", "gamma"]);

        let mut alphabetical = LibraryFilter::new();
        alphabetical.set_sort(SORT_ALPHABETICAL);
        let mut rest = items.clone();
        rest.push(make_item("aardvark", false, &[], vec![]));
        let names: Vec<String> = alphabetical
            .apply(&rest, " ")
            .into_iter()
            .map(|item| item.name)
            .collect();
        assert_eq!(names, vec!["aardvark", "Alpha Deluxe", "beta", "gamma"]);

        let queried = filter.apply(&items, "BETA");
        assert_eq!(queried.len(), 1);
        assert_eq!(queried[0].title_id, "beta");

        let mut touch = LibraryFilter::new();
        touch.set_touch_only(true);
        assert_eq!(touch.apply(&items, "").len(), 1);
    }
}
