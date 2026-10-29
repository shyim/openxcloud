//! Per-user data kept on this Mac: favourites and the local play history, plus the
//! recommendations derived from them.
//!
//! Stored as JSON next to the settings (`<config dir>/user_data.json`). The UI and
//! the shell share one [`UserDataHandle`]; every mutation bumps
//! [`UserData::revision`] and the shell saves when the revision moves (see
//! [`UserDataStore::save_if_changed`]).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use xcloud_proto::models::LibraryCard;

const LOG: &str = "UserData";
/// File name under the config directory.
pub const USER_DATA_FILE_NAME: &str = "user_data.json";
/// Most history entries kept.
const MAX_HISTORY: usize = 200;

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One game's local play history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PlayRecord {
    pub title_id: String,
    /// Unix seconds of the most recent launch.
    pub last_played: u64,
    /// Launches from this app.
    pub sessions: u32,
    /// Connected streaming time in seconds.
    pub seconds_played: u64,
}

/// Favourites and play history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UserData {
    pub favorites: BTreeSet<String>,
    /// Most recently played first.
    pub history: Vec<PlayRecord>,
    /// Bumped on every change; not persisted.
    #[serde(skip)]
    pub revision: u64,
}

impl UserData {
    pub fn is_favorite(&self, title_id: &str) -> bool {
        self.favorites.contains(title_id)
    }

    /// Adds or removes a favourite. Returns the new state.
    pub fn toggle_favorite(&mut self, title_id: &str) -> bool {
        let now = if self.favorites.remove(title_id) {
            false
        } else {
            self.favorites.insert(title_id.to_owned());
            true
        };
        self.revision += 1;
        now
    }

    /// Records a launch of `title_id` at `now` (unix seconds) and moves it to the
    /// front of the history.
    pub fn record_launch(&mut self, title_id: &str, now: u64) {
        if title_id.is_empty() {
            return;
        }
        let mut record = match self.history.iter().position(|r| r.title_id == title_id) {
            Some(i) => self.history.remove(i),
            None => PlayRecord {
                title_id: title_id.to_owned(),
                ..PlayRecord::default()
            },
        };
        record.sessions = record.sessions.saturating_add(1);
        record.last_played = now;
        self.history.insert(0, record);
        self.history.truncate(MAX_HISTORY);
        self.revision += 1;
    }

    /// Adds connected streaming time to `title_id`'s record.
    pub fn add_play_time(&mut self, title_id: &str, seconds: u64) {
        if seconds == 0 {
            return;
        }
        if let Some(record) = self.history.iter_mut().find(|r| r.title_id == title_id) {
            record.seconds_played = record.seconds_played.saturating_add(seconds);
            self.revision += 1;
        }
    }

    pub fn record(&self, title_id: &str) -> Option<&PlayRecord> {
        self.history.iter().find(|r| r.title_id == title_id)
    }
}

/// The shared handle the UI and shell use.
pub type UserDataHandle = Arc<RwLock<UserData>>;

/// "Jump back in": games played from this app (most recent first), then the
/// service's recently played list, without duplicates. Only games present in
/// `all` (the library) are returned, as cards from `all`.
pub fn jump_back_in<'a>(
    user: &UserData,
    server_recent: &'a [LibraryCard],
    all: &'a [LibraryCard],
    limit: usize,
) -> Vec<&'a LibraryCard> {
    let by_id: HashMap<&str, &LibraryCard> = all.iter().map(|c| (c.title_id.as_str(), c)).collect();
    let mut out: Vec<&LibraryCard> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut history: Vec<&PlayRecord> = user.history.iter().collect();
    history.sort_by_key(|r| std::cmp::Reverse(r.last_played));
    for record in history {
        if out.len() >= limit {
            return out;
        }
        if let Some(card) = by_id.get(record.title_id.as_str())
            && seen.insert(card.title_id.as_str())
        {
            out.push(card);
        }
    }
    for card in server_recent {
        if out.len() >= limit {
            break;
        }
        let card = by_id.get(card.title_id.as_str()).copied().unwrap_or(card);
        if seen.insert(card.title_id.as_str()) {
            out.push(card);
        }
    }
    out
}

/// The favourite games, in library order.
pub fn favorite_cards<'a>(user: &UserData, all: &'a [LibraryCard]) -> Vec<&'a LibraryCard> {
    all.iter()
        .filter(|c| user.is_favorite(&c.title_id))
        .collect()
}

/// Matching keys of a card: genres (`g:`) and capability features (`f:`).
fn attribute_keys(card: &LibraryCard) -> Vec<String> {
    let genres = card
        .categories
        .iter()
        .map(|c| format!("g:{}", c.trim().to_lowercase()));
    let features = card
        .attributes
        .iter()
        .map(|a| a.name.trim().to_lowercase())
        .filter(|n| !n.is_empty())
        .map(|n| format!("f:{n}"));
    genres.chain(features).collect()
}

/// Genres are the strong signal; capability features ("Online co-op", "4K") a weak one.
fn key_weight(key: &str) -> f64 {
    if key.starts_with("g:") { 3.0 } else { 0.5 }
}

/// "Picked for you": games not yet played or favourited, ranked by how much they have
/// in common with the games the user plays (local history, weighted by sessions,
/// play time and recency), favourites, and the service's recently played list.
///
/// Shared genres and features count by rarity (`ln(N / df)`), genres six times as much
/// as capability features, so "Xbox achievements" on every game adds nothing while a
/// shared niche genre counts a lot; the same developer or publisher adds a bonus.
pub fn recommendations<'a>(
    user: &UserData,
    server_recent: &[LibraryCard],
    all: &'a [LibraryCard],
    now: u64,
    limit: usize,
) -> Vec<&'a LibraryCard> {
    if all.is_empty() {
        return Vec::new();
    }
    let by_id: HashMap<&str, &LibraryCard> = all.iter().map(|c| (c.title_id.as_str(), c)).collect();

    // Seeds and their weights.
    let mut seeds: HashMap<&str, f64> = HashMap::new();
    for record in &user.history {
        let days = now.saturating_sub(record.last_played) as f64 / 86_400.0;
        let recency = 0.5_f64.powf(days / 14.0);
        let engagement = 1.0
            + f64::from(record.sessions).ln_1p()
            + (record.seconds_played as f64 / 1800.0).min(4.0);
        *seeds.entry(record.title_id.as_str()).or_default() += engagement * (0.25 + recency);
    }
    for id in &user.favorites {
        *seeds.entry(id.as_str()).or_default() += 2.0;
    }
    for card in server_recent {
        *seeds.entry(card.title_id.as_str()).or_default() += 0.75;
    }
    let seed_cards: Vec<(&LibraryCard, f64)> = seeds
        .iter()
        .filter_map(|(id, w)| by_id.get(id).map(|c| (*c, *w)))
        .collect();
    if seed_cards.is_empty() {
        return Vec::new();
    }

    // Feature rarity across the library.
    let mut df: HashMap<String, usize> = HashMap::new();
    for card in all {
        let keys: HashSet<String> = attribute_keys(card).into_iter().collect();
        for key in keys {
            *df.entry(key).or_default() += 1;
        }
    }
    let n = all.len() as f64;
    let idf = |key: &str| df.get(key).map_or(0.0, |&d| (n / d as f64).ln().max(0.0));

    let seed_profiles: Vec<(HashSet<String>, &LibraryCard, f64)> = seed_cards
        .iter()
        .map(|(c, w)| (attribute_keys(c).into_iter().collect(), *c, *w))
        .collect();

    let excluded: HashSet<&str> = seeds.keys().copied().collect();
    let mut scored: Vec<(&LibraryCard, f64)> = all
        .iter()
        .filter(|c| {
            !excluded.contains(c.title_id.as_str()) && !c.name.is_empty() && c.name != c.product_id
        })
        .filter_map(|candidate| {
            let keys: HashSet<String> = attribute_keys(candidate).into_iter().collect();
            let mut score = 0.0;
            for (seed_keys, seed, weight) in &seed_profiles {
                let shared: f64 = keys
                    .intersection(seed_keys)
                    .map(|k| idf(k) * key_weight(k))
                    .sum();
                let mut s = shared;
                if !candidate.publisher_name.is_empty()
                    && candidate.publisher_name == seed.publisher_name
                {
                    s += 0.75;
                }
                if !candidate.developer_name.is_empty()
                    && candidate.developer_name == seed.developer_name
                {
                    s += 1.0;
                }
                score += s * weight;
            }
            (score > 0.0).then_some((candidate, score))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.name.cmp(&b.0.name))
    });
    scored.into_iter().take(limit).map(|(c, _)| c).collect()
}

// ----------------------------------------------------------------------------- persistence

/// `<config dir>/user_data.json`.
pub fn user_data_path() -> Option<PathBuf> {
    crate::settings::project_dirs().map(|d| d.config_dir().join(USER_DATA_FILE_NAME))
}

/// Reads `path`; a missing or malformed file is empty user data.
pub fn load_from(path: &Path) -> UserData {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
            tracing::warn!(target: LOG, "{} is malformed: {error}; starting empty", path.display());
            UserData::default()
        }),
        Err(_) => UserData::default(),
    }
}

/// Writes `data` atomically (temp file + rename).
pub fn save_to(path: &Path, data: &UserData) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(data).map_err(io::Error::other)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

/// Owns the file and the shared handle; saves when the data changed.
#[derive(Debug)]
pub struct UserDataStore {
    path: Option<PathBuf>,
    handle: UserDataHandle,
    saved_revision: u64,
}

impl UserDataStore {
    /// Loads from [`user_data_path`].
    pub fn open() -> Self {
        match user_data_path() {
            Some(path) => Self::at(path),
            None => Self::in_memory(UserData::default()),
        }
    }

    pub fn at(path: PathBuf) -> Self {
        let data = load_from(&path);
        tracing::info!(
            target: LOG,
            "{} favourites, {} played games",
            data.favorites.len(),
            data.history.len()
        );
        Self {
            path: Some(path),
            handle: Arc::new(RwLock::new(data)),
            saved_revision: 0,
        }
    }

    pub fn in_memory(data: UserData) -> Self {
        Self {
            path: None,
            handle: Arc::new(RwLock::new(data)),
            saved_revision: 0,
        }
    }

    pub fn handle(&self) -> UserDataHandle {
        Arc::clone(&self.handle)
    }

    /// Saves when the revision moved since the last save. The lock is released
    /// before the disk write.
    pub fn save_if_changed(&mut self) -> io::Result<bool> {
        let (revision, snapshot) = {
            let data = self.handle.read();
            if data.revision == self.saved_revision {
                return Ok(false);
            }
            (data.revision, data.clone())
        };
        if let Some(path) = &self.path {
            save_to(path, &snapshot)?;
        }
        self.saved_revision = revision;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xcloud_proto::models::LibraryAttribute;

    fn card(id: &str, publisher: &str, attrs: &[&str]) -> LibraryCard {
        LibraryCard {
            title_id: id.into(),
            product_id: format!("P{id}"),
            name: format!("Game {id}"),
            publisher_name: publisher.into(),
            attributes: attrs
                .iter()
                .map(|a| LibraryAttribute::new(*a, *a))
                .collect(),
            ..Default::default()
        }
    }

    fn genre(mut card: LibraryCard, genres: &[&str]) -> LibraryCard {
        card.categories = genres.iter().map(|g| (*g).to_owned()).collect();
        card
    }

    #[test]
    fn launches_move_to_the_front_and_count() {
        let mut data = UserData::default();
        data.record_launch("A", 100);
        data.record_launch("B", 200);
        data.record_launch("A", 300);
        let ids: Vec<&str> = data.history.iter().map(|r| r.title_id.as_str()).collect();
        assert_eq!(ids, ["A", "B"]);
        assert_eq!(data.record("A").unwrap().sessions, 2);
        assert_eq!(data.record("A").unwrap().last_played, 300);
        data.add_play_time("A", 90);
        assert_eq!(data.record("A").unwrap().seconds_played, 90);
        assert!(data.revision > 0);
    }

    #[test]
    fn favourites_toggle() {
        let mut data = UserData::default();
        assert!(data.toggle_favorite("A"));
        assert!(data.is_favorite("A"));
        assert!(!data.toggle_favorite("A"));
        assert!(!data.is_favorite("A"));
    }

    #[test]
    fn jump_back_in_puts_local_history_before_the_server_list() {
        let all = vec![card("A", "", &[]), card("B", "", &[]), card("C", "", &[])];
        let mut data = UserData::default();
        data.record_launch("C", 10);
        let server = vec![card("A", "", &[]), card("C", "", &[])];
        let ids: Vec<&str> = jump_back_in(&data, &server, &all, 10)
            .iter()
            .map(|c| c.title_id.as_str())
            .collect();
        assert_eq!(ids, ["C", "A"]);
        // Games no longer in the library are skipped.
        data.record_launch("GONE", 20);
        assert_eq!(jump_back_in(&data, &[], &all, 10).len(), 1);
    }

    #[test]
    fn recommendations_favour_rare_shared_features_and_skip_played_games() {
        let common = "Xbox achievements";
        let all = vec![
            genre(card("PLAYED", "Studio X", &[common]), &["Racing & flying"]),
            genre(card("RACER", "Other", &[common]), &["Racing & flying"]),
            genre(card("SAMEPUB", "Studio X", &[common]), &["Puzzle"]),
            genre(card("UNRELATED", "Else", &[common]), &["Puzzle"]),
            genre(card("SHOOTER", "Else", &[common]), &["Shooter"]),
        ];
        let mut data = UserData::default();
        data.record_launch("PLAYED", 1_000);
        let ids: Vec<&str> = recommendations(&data, &[], &all, 1_000, 10)
            .iter()
            .map(|c| c.title_id.as_str())
            .collect();
        // Same genre beats same publisher.
        assert_eq!(ids, ["RACER", "SAMEPUB"], "{ids:?}");
        // Sharing only a feature every game has is not a recommendation.
        assert!(!ids.contains(&"UNRELATED"));
        assert!(!ids.contains(&"PLAYED"));
    }

    #[test]
    fn no_seeds_means_no_recommendations() {
        let all = vec![card("A", "", &["Racing"])];
        assert!(recommendations(&UserData::default(), &[], &all, 0, 10).is_empty());
    }

    #[test]
    fn store_saves_only_after_a_change() {
        let dir = std::env::temp_dir().join(format!("openxcloud-user-{}", std::process::id()));
        let path = dir.join(USER_DATA_FILE_NAME);
        let mut store = UserDataStore::at(path.clone());
        assert!(!store.save_if_changed().unwrap());
        store.handle().write().toggle_favorite("A");
        assert!(store.save_if_changed().unwrap());
        assert!(!store.save_if_changed().unwrap());
        let loaded = load_from(&path);
        assert!(loaded.is_favorite("A"));
        let _ = fs::remove_dir_all(&dir);
    }
}
