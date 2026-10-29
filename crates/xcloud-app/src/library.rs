//! Port of qt/src/app/LibraryController.{h,cpp} (with its `GameListModel`) onto the
//! snapshot/command contract in [`crate::state`].
//!
//! The Qt controller was a `QObject` whose `Q_PROPERTY`s QML read synchronously when a
//! signal fired. Here the private browse state lives in [`Inner`] behind a mutex, and
//! every step that changed a property in the C++ (`itemsChanged`, `loadingChanged`,
//! `statusChanged`, `errorChanged`) writes one complete [`LibrarySnapshot`] and calls
//! the [`Repaint`] callback. The UI never blocks on the controller: it reads the latest
//! snapshot once per frame and sends [`LibraryCommand`]s over a channel.
//!
//! `refresh(force)` follows the C++ step by step: `/v2/titles`, then `/v2/titles/mru`,
//! publish the library **before** catalog hydration, then enrich it batch by batch via
//! [`GamePassCatalogClient::hydrate_products`] (MRU product ids first), republishing on
//! every batch. A load generation counter discards late results of a superseded load.
//!
//! The filter/sort pass (`LibraryFilter::matches` + the two `std::stable_sort`s) lives
//! in `xcloud_proto::library_filter`; the search query debounce is the UI's job.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::Receiver;
use parking_lot::{Mutex, RwLock};
use xcloud_api::catalog::{CatalogQuery, GamePassCatalogClient};
use xcloud_api::client::XCloudApiClient;
use xcloud_proto::library_filter::{LibraryFilter, compare_names};
use xcloud_proto::models::{CatalogProduct, CloudLibraryItem, LibraryCard, XCloudTitlesResponse};

use crate::state::{FeatureGroup, LibraryCommand, LibrarySnapshot, Repaint, Shelf};

const LOG: &str = "Library";

/// `statusText` while the title list loads.
pub const STATUS_LOADING_GAMES: &str = "Loading your games";
/// `statusText` while the catalog enriches an already visible library.
pub const STATUS_LOADING_DETAILS: &str = "Loading game details\u{2026}";
/// `getCloudTitlesMru(25)`.
pub const DEFAULT_MRU_LIMIT: u32 = 25;

/// Shelf ids and titles, exactly as `LibraryController::buildShelves` named them.
pub const SHELF_MRU: &str = "mru";
pub const SHELF_MRU_TITLE: &str = "Jump back in";
pub const SHELF_RECENTLY_ADDED: &str = "recentlyAdded";
pub const SHELF_RECENTLY_ADDED_TITLE: &str = "Recently added";
/// Feature shelves are `feature:<normalised feature name>`.
pub const SHELF_FEATURE_PREFIX: &str = "feature:";

/// Feature group titles, in the order `featureGroups` lists them.
pub const GROUP_PLAY_TOGETHER: &str = "Play together";
pub const GROUP_PICTURE_AND_SOUND: &str = "Picture and sound";
pub const GROUP_MORE: &str = "More";

/// `items.mid(0, 20)`: the home view shows a small preview per shelf.
const SHELF_PREVIEW_LEN: usize = 20;
/// "Recently added" needs at least this many dated titles.
const MIN_RECENTLY_ADDED: usize = 5;
/// A feature shelf needs at least this many titles ...
const MIN_FEATURE_SHELF: usize = 4;
/// ... and at most this many feature shelves are shown.
const MAX_FEATURE_SHELVES: usize = 4;
/// `GameListModel::setItems` diffs lists up to this size row by row.
const MODEL_DIFF_LIMIT: usize = 20;
/// A catalog cache file younger than this is trusted: only products missing from it
/// are fetched. Older files still show at once and are refreshed in the background.
const CATALOG_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

// ----------------------------------------------------------------------------- GameListModel

/// What `GameListModel::set_items` had to do to the rows; the Qt model emitted
/// insert/move/dataChanged signals for `Incremental` and `modelReset` for `Reset`.
/// A view that keeps per-row state (textures, scroll anchors) only has to drop it
/// on `Reset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelUpdate {
    Unchanged,
    Incremental,
    Reset,
}

/// Port of `GameListModel`: a card list that is updated row by row (keyed by
/// `title_id`) so an MRU shelf survives catalog hydration without a reset.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GameListModel {
    items: Vec<LibraryCard>,
}

impl GameListModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn items(&self) -> &[LibraryCard] {
        &self.items
    }

    pub fn row_count(&self) -> usize {
        self.items.len()
    }

    pub fn row(&self, index: usize) -> Option<&LibraryCard> {
        self.items.get(index)
    }

    /// `GameListModel::setItems`. Afterwards the rows equal `items`; the return
    /// value says whether the Qt model would have reset.
    pub fn set_items(&mut self, items: Vec<LibraryCard>) -> ModelUpdate {
        if self.items == items {
            return ModelUpdate::Unchanged;
        }
        // Small shelf lists change rank and grow during catalog hydration.
        // Move/insert rows so the existing MRU shelf and its textures survive.
        if self.items.len() <= MODEL_DIFF_LIMIT && items.len() <= MODEL_DIFF_LIMIT {
            let target_len = items.len();
            for (i, card) in items.into_iter().enumerate() {
                let existing =
                    (i..self.items.len()).find(|&j| self.items[j].title_id == card.title_id);
                match existing {
                    None => self.items.insert(i, card),
                    Some(j) => {
                        if j != i {
                            let moved = self.items.remove(j);
                            self.items.insert(i, moved);
                        }
                        if self.items[i] != card {
                            self.items[i] = card;
                        }
                    }
                }
            }
            self.items.truncate(target_len);
            return ModelUpdate::Incremental;
        }
        let same_order = self.items.len() == items.len()
            && self
                .items
                .iter()
                .zip(&items)
                .all(|(old, new)| old.title_id == new.title_id);
        self.items = items;
        if same_order {
            ModelUpdate::Incremental
        } else {
            ModelUpdate::Reset
        }
    }
}

// ----------------------------------------------------------------------------- helpers

/// `featureNamesFor`: the normalised, de-duplicated feature names of an item.
pub fn feature_names_for(item: &CloudLibraryItem) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for attribute in &item.attributes {
        let name = LibraryFilter::normalized_feature(&attribute.localized_name);
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// `groupTitleFor`: which `featureGroups` entry a feature belongs to.
pub fn group_title_for(feature: &str) -> &'static str {
    let lower = feature.to_lowercase();
    let has = |needle: &str| lower.contains(needle);
    if has("co-op") || has("coop") || has("multiplayer") || has("cross-") || has("single player") {
        return GROUP_PLAY_TOGETHER;
    }
    if has("4k")
        || has("hdr")
        || has("dolby")
        || has("dts")
        || has("spatial")
        || has("fps")
        || has("ray tracing")
        || has("optimized")
        || has("refresh")
        || has("1080")
        || has("120")
    {
        return GROUP_PICTURE_AND_SOUND;
    }
    GROUP_MORE
}

/// Stable id for a feature group (the C++ map only carried the title).
fn group_id_for(title: &str) -> &'static str {
    match title {
        GROUP_PLAY_TOGETHER => "playTogether",
        GROUP_PICTURE_AND_SOUND => "pictureAndSound",
        _ => "more",
    }
}

// ----------------------------------------------------------------------------- Inner

/// The C++ member state. Owned by the controller and its load tasks behind a mutex;
/// `to_snapshot` projects it onto the UI contract.
#[derive(Debug, Default)]
struct Inner {
    /// `m_rawItems`: server order, enriched in place by catalog batches.
    raw_items: Vec<CloudLibraryItem>,
    /// `m_itemMaps`: the card of every raw item, by title id.
    cards: HashMap<String, LibraryCard>,
    /// `m_mruTitleIds`.
    mru_title_ids: HashSet<String>,
    mru_order: Vec<String>,
    browse_shelf: String,
    query: String,
    filter: LibraryFilter,
    /// `m_filteredItems` (`gameModel`).
    filtered: Vec<LibraryCard>,
    /// `m_allItems`: every card, alphabetical.
    all_items: Vec<LibraryCard>,
    /// `m_shelves`: id/title plus the ≤20-card preview.
    shelves: Vec<Shelf>,
    /// `m_shelfItems`: the full card list of every shelf.
    shelf_items: HashMap<String, Vec<LibraryCard>>,
    feature_groups: Vec<FeatureGroup>,
    is_loading: bool,
    status_text: String,
    last_error: String,
    authentication_required: bool,
    did_load: bool,
    /// Incremented on every refresh so late results of a previous load cannot
    /// overwrite a newer one.
    load_generation: u64,
    /// Every catalog product seen so far, by product id. Re-applied when titles are
    /// reloaded (refresh, language switch) so cards keep their names and art while
    /// the new catalog data loads, and if it fails.
    catalog_cache: HashMap<String, CatalogProduct>,
}

impl Inner {
    fn card_for(&self, item: &CloudLibraryItem) -> LibraryCard {
        self.cards
            .get(&item.title_id)
            .cloned()
            .unwrap_or_else(|| item.to_card())
    }

    /// The second half of `refresh`: adopts the title and MRU responses and returns
    /// the product ids to hydrate, MRU ids first so recent games are enriched first.
    fn adopt_titles(
        &mut self,
        titles: &XCloudTitlesResponse,
        mru: Option<&XCloudTitlesResponse>,
    ) -> Vec<String> {
        self.mru_title_ids.clear();
        self.mru_order.clear();
        let mut product_ids: Vec<String> = Vec::new();
        if let Some(mru) = mru {
            for entry in &mru.results {
                if self.mru_title_ids.insert(entry.title_id.clone()) {
                    self.mru_order.push(entry.title_id.clone());
                }
            }
            for entry in &mru.results {
                if !entry.product_id.is_empty() {
                    product_ids.push(entry.product_id.clone());
                }
            }
        }
        self.raw_items.clear();
        self.cards.clear();
        for dto in &titles.results {
            let item =
                CloudLibraryItem::from_title_dto(dto, self.mru_title_ids.contains(&dto.title_id));
            self.cards.insert(item.title_id.clone(), item.to_card());
            if !dto.product_id.is_empty() {
                product_ids.push(dto.product_id.clone());
            }
            self.raw_items.push(item);
        }
        product_ids
    }

    /// `applyCatalogBatch`: a product matches an item by xCloud title id, falling
    /// back to the product id. Does not rebuild; the caller does.
    fn apply_catalog_batch(&mut self, products: &[CatalogProduct]) {
        for product in products {
            if !product.product_id.is_empty() {
                self.catalog_cache
                    .insert(product.product_id.clone(), product.clone());
            }
        }
        let by_product: HashMap<&str, &CatalogProduct> = products
            .iter()
            .filter(|product| !product.product_id.is_empty())
            .map(|product| (product.product_id.as_str(), product))
            .collect();
        let by_title: HashMap<&str, &CatalogProduct> = products
            .iter()
            .filter(|product| !product.xcloud_title_id.is_empty())
            .map(|product| (product.xcloud_title_id.as_str(), product))
            .collect();
        for item in &mut self.raw_items {
            let product = by_title
                .get(item.title_id.as_str())
                .or_else(|| by_product.get(item.product_id.as_str()));
            if let Some(product) = product {
                item.apply_catalog_product(product);
                self.cards.insert(item.title_id.clone(), item.to_card());
            }
        }
    }

    /// `rebuildFiltered(rebuildCatalog)`: recomputes the grid; with `rebuild_shelves`
    /// also the shelves, the alphabetical `all_items` and the feature groups.
    fn rebuild_filtered(&mut self, rebuild_shelves: bool) {
        if rebuild_shelves {
            self.build_shelves();
        }
        let trimmed = self.query.trim();
        let use_shelf =
            !self.browse_shelf.is_empty() && trimmed.is_empty() && !self.filter.is_active();
        // The sort is stable and the shelf predicate is per item, so restricting the
        // sorted list to the shelf equals the C++ "restrict, then sort".
        let mut filtered = self.filter.apply(&self.raw_items, &self.query);
        if use_shelf {
            let shelf_ids: HashSet<&str> = self
                .shelf_items
                .get(&self.browse_shelf)
                .map(|cards| cards.iter().map(|card| card.title_id.as_str()).collect())
                .unwrap_or_default();
            filtered.retain(|item| shelf_ids.contains(item.title_id.as_str()));
        }
        self.filtered = filtered.iter().map(|item| self.card_for(item)).collect();

        if !rebuild_shelves {
            return;
        }

        // Build the full alphabetical list and the feature groups once per rebuild.
        let mut sorted: Vec<&CloudLibraryItem> = self.raw_items.iter().collect();
        sorted.sort_by(|lhs, rhs| compare_names(&lhs.name, &rhs.name));
        let mut features: HashSet<String> = HashSet::new();
        let mut all_items = Vec::with_capacity(sorted.len());
        for item in sorted {
            all_items.push(self.card_for(item));
            features.extend(feature_names_for(item));
        }
        self.all_items = all_items;

        let mut grouped: HashMap<&'static str, Vec<String>> = HashMap::new();
        for feature in features {
            grouped
                .entry(group_title_for(&feature))
                .or_default()
                .push(feature);
        }
        self.feature_groups.clear();
        for title in [GROUP_PLAY_TOGETHER, GROUP_PICTURE_AND_SOUND, GROUP_MORE] {
            let Some(mut list) = grouped.remove(title) else {
                continue;
            };
            list.sort_by(|lhs, rhs| compare_names(lhs, rhs));
            self.feature_groups.push(FeatureGroup {
                id: group_id_for(title).to_owned(),
                title: title.to_owned(),
                count: list.len(),
                features: list,
            });
        }
    }

    fn append_shelf(&mut self, id: String, title: String, items: Vec<LibraryCard>) {
        let preview = items.iter().take(SHELF_PREVIEW_LEN).cloned().collect();
        self.shelves.push(Shelf {
            id: id.clone(),
            title,
            items: preview,
        });
        self.shelf_items.insert(id, items);
    }

    /// `buildShelves`: "Jump back in" (MRU, server order), "Recently added" (newest
    /// release dates, ≥5 dated titles) and up to four feature shelves (the most
    /// common capabilities with ≥4 titles, skipping anything that says "buy").
    fn build_shelves(&mut self) {
        self.shelves.clear();
        self.shelf_items.clear();

        let recent: Vec<LibraryCard> = self
            .mru_order
            .iter()
            .filter_map(|id| self.cards.get(id).cloned())
            .collect();
        if !recent.is_empty() {
            self.append_shelf(SHELF_MRU.to_owned(), SHELF_MRU_TITLE.to_owned(), recent);
        }

        let mut by_date: Vec<&CloudLibraryItem> = self
            .raw_items
            .iter()
            .filter(|item| !item.release_date.is_empty())
            .collect();
        if by_date.len() >= MIN_RECENTLY_ADDED {
            by_date.sort_by(|lhs, rhs| rhs.release_date.cmp(&lhs.release_date));
            let items: Vec<LibraryCard> = by_date
                .iter()
                .take(SHELF_PREVIEW_LEN)
                .map(|item| self.card_for(item))
                .collect();
            self.append_shelf(
                SHELF_RECENTLY_ADDED.to_owned(),
                SHELF_RECENTLY_ADDED_TITLE.to_owned(),
                items,
            );
        }

        let mut buckets: HashMap<String, Vec<LibraryCard>> = HashMap::new();
        for item in &self.raw_items {
            for feature in feature_names_for(item) {
                buckets
                    .entry(feature)
                    .or_default()
                    .push(self.card_for(item));
            }
        }
        let mut ordered: Vec<(String, Vec<LibraryCard>)> = buckets
            .into_iter()
            .filter(|(_, items)| items.len() >= MIN_FEATURE_SHELF)
            .collect();
        ordered.sort_by(|lhs, rhs| {
            rhs.1
                .len()
                .cmp(&lhs.1.len())
                .then_with(|| compare_names(&lhs.0, &rhs.0))
        });
        for (title, items) in ordered.into_iter().take(MAX_FEATURE_SHELVES) {
            if title.to_lowercase().contains("buy") {
                continue;
            }
            self.append_shelf(format!("{SHELF_FEATURE_PREFIX}{title}"), title, items);
        }
    }

    fn to_snapshot(&self) -> LibrarySnapshot {
        LibrarySnapshot {
            revision: 0,
            items: self.filtered.clone(),
            all_items: self.all_items.clone(),
            browse_shelf: self.browse_shelf.clone(),
            shelves: self.shelves.clone(),
            shelf_items: self.shelf_items.clone(),
            feature_groups: self.feature_groups.clone(),
            query: self.query.clone(),
            filter: self.filter.clone(),
            is_loading: self.is_loading,
            status_text: self.status_text.clone(),
            last_error: self.last_error.clone(),
            authentication_required: self.authentication_required,
        }
    }
}

/// State shared between the command loop and the load tasks.
struct Shared {
    inner: Mutex<Inner>,
    snapshot: Arc<RwLock<LibrarySnapshot>>,
    repaint: Repaint,
}

impl Shared {
    /// Publishes `inner` as the new snapshot and asks the UI to redraw. The caller
    /// holds the `inner` lock so two steps can never publish out of order.
    fn publish(&self, inner: &Inner) {
        let mut next = inner.to_snapshot();
        let mut snapshot = self.snapshot.write();
        next.revision = snapshot.revision.wrapping_add(1);
        *snapshot = next;
        drop(snapshot);
        (self.repaint)();
    }
}

// ----------------------------------------------------------------------------- controller

/// Owns the browse state: titles, catalog enrichment, MRU, search, filters and shelves.
pub struct LibraryController {
    api: Arc<XCloudApiClient>,
    catalog: Option<Arc<GamePassCatalogClient>>,
    shared: Arc<Shared>,
    commands: Receiver<LibraryCommand>,
    runtime: tokio::runtime::Handle,
    /// Store language and market for catalog hydration (follows the app locale).
    catalog_query: Mutex<CatalogQuery>,
    /// Where catalog data is kept between launches (`None`: memory only).
    catalog_cache_dir: Option<PathBuf>,
}

/// The catalog file of `query` in `dir`.
pub fn catalog_cache_path(dir: &Path, query: &CatalogQuery) -> PathBuf {
    dir.join(format!("catalog-{}-{}.json", query.language, query.market))
}

/// `<cache dir>/catalog`, next to the artwork cache.
pub fn default_catalog_cache_dir() -> Option<PathBuf> {
    crate::settings::project_dirs().map(|d| d.cache_dir().join("catalog"))
}

/// A catalog file on disk: the products of one language/market and when they were saved.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct CatalogFile {
    saved_at: u64,
    products: Vec<CatalogProduct>,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reads a catalog file; a missing or unreadable file is an empty cache.
fn load_catalog_file(path: &Path) -> CatalogFile {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!(target: LOG, "ignoring catalog cache {}: {error}", path.display());
            CatalogFile::default()
        }),
        Err(_) => CatalogFile::default(),
    }
}

/// Writes a catalog file atomically.
fn save_catalog_file(path: &Path, file: &CatalogFile) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes = serde_json::to_vec(file).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// The catalog query for an app locale such as `de-DE`.
pub fn catalog_query_for(locale: &str) -> CatalogQuery {
    let code = xcloud_proto::locales::find(locale)
        .map_or(xcloud_proto::locales::DEFAULT_LOCALE, |l| l.code);
    CatalogQuery {
        market: xcloud_proto::locales::market_for(code),
        language: code.to_owned(),
        ..CatalogQuery::default()
    }
}

impl LibraryController {
    /// `snapshot` and `commands` come from [`crate::state::LibraryHandle::new`]; the
    /// catalog is optional like the C++ pointer (no catalog ⇒ no enrichment). Network
    /// work runs on `runtime`.
    pub fn new(
        api: Arc<XCloudApiClient>,
        catalog: Option<Arc<GamePassCatalogClient>>,
        snapshot: Arc<RwLock<LibrarySnapshot>>,
        commands: Receiver<LibraryCommand>,
        repaint: Repaint,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        let inner = Inner {
            filter: snapshot.read().filter.clone(),
            ..Inner::default()
        };
        Self {
            api,
            catalog,
            shared: Arc::new(Shared {
                inner: Mutex::new(inner),
                snapshot,
                repaint,
            }),
            commands,
            runtime,
            catalog_query: Mutex::new(CatalogQuery::default()),
            catalog_cache_dir: None,
        }
    }

    /// Keeps catalog data in `dir` between launches, so names and art show at once.
    pub fn with_catalog_cache_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.catalog_cache_dir = dir;
        self
    }

    /// Serves commands until every sender is dropped. Blocks the calling thread;
    /// network work is spawned on the runtime.
    pub fn run(self) {
        tracing::debug!(target: LOG, "library controller running");
        for command in self.commands.iter() {
            self.handle_command(command);
        }
        tracing::debug!(target: LOG, "library controller stopped");
    }

    /// [`run`](Self::run) on a dedicated thread.
    pub fn spawn(self) -> std::io::Result<thread::JoinHandle<()>> {
        thread::Builder::new()
            .name("library-controller".to_owned())
            .spawn(move || self.run())
    }

    pub fn handle_command(&self, command: LibraryCommand) {
        match command {
            LibraryCommand::Refresh { force } => self.refresh(force),
            LibraryCommand::SetQuery(query) => self.set_query(&query),
            LibraryCommand::SetFilter(filter) => self.set_filter(filter),
            LibraryCommand::SetBrowseShelf(id) => self.set_browse_shelf(&id),
            LibraryCommand::SetLocale(locale) => self.set_locale(&locale),
        }
    }

    /// Switches the store language and market. A library that was already loaded is
    /// reloaded so titles and descriptions follow the new language.
    pub fn set_locale(&self, locale: &str) {
        let query = catalog_query_for(locale);
        {
            let mut current = self.catalog_query.lock();
            if *current == query {
                return;
            }
            tracing::info!(target: LOG, "catalog language {} market {}", query.language, query.market);
            *current = query;
        }
        let loaded = self.shared.inner.lock().did_load;
        if loaded {
            self.refresh(true);
        }
    }

    /// The catalog query in use.
    pub fn catalog_query(&self) -> CatalogQuery {
        self.catalog_query.lock().clone()
    }

    /// The latest published snapshot.
    pub fn snapshot(&self) -> LibrarySnapshot {
        self.shared.snapshot.read().clone()
    }

    /// `refresh(force)`: loads the library. When `force` is false a previously
    /// loaded library is kept; a load already in progress is never duplicated.
    pub fn refresh(&self, force: bool) {
        let generation = {
            let mut inner = self.shared.inner.lock();
            if inner.is_loading || (inner.did_load && !force) {
                return;
            }
            inner.load_generation += 1;
            inner.is_loading = true;
            inner.last_error.clear();
            inner.authentication_required = false;
            inner.status_text = STATUS_LOADING_GAMES.to_owned();
            self.shared.publish(&inner);
            inner.load_generation
        };
        let api = self.api.clone();
        let catalog = self.catalog.clone();
        let shared = self.shared.clone();
        let query = self.catalog_query.lock().clone();
        let cache_path = self
            .catalog_cache_dir
            .as_deref()
            .map(|dir| catalog_cache_path(dir, &query));
        self.runtime.spawn(async move {
            load_library(api, catalog, shared, generation, query, cache_path).await
        });
    }

    /// `setQuery`: filters the grid immediately (debounce is the UI's job).
    pub fn set_query(&self, query: &str) {
        let mut inner = self.shared.inner.lock();
        if inner.query == query {
            return;
        }
        inner.query = query.to_owned();
        inner.rebuild_filtered(false);
        self.shared.publish(&inner);
    }

    /// `LibraryFilter::changed` → `rebuildFiltered(false)`.
    pub fn set_filter(&self, filter: LibraryFilter) {
        let mut inner = self.shared.inner.lock();
        if inner.filter == filter {
            return;
        }
        inner.filter = filter;
        inner.rebuild_filtered(false);
        self.shared.publish(&inner);
    }

    /// `setBrowseShelf`: restricts the grid to one shelf (empty id ⇒ all titles).
    /// Ignored while a query or filter is active, like the C++.
    pub fn set_browse_shelf(&self, id: &str) {
        let mut inner = self.shared.inner.lock();
        if inner.browse_shelf == id {
            return;
        }
        inner.browse_shelf = id.to_owned();
        inner.rebuild_filtered(false);
        self.shared.publish(&inner);
    }

    /// `shelfItems`: the full card list of a shelf (the snapshot carries a ≤20 preview).
    pub fn shelf_items(&self, shelf_id: &str) -> Vec<LibraryCard> {
        self.shared
            .inner
            .lock()
            .shelf_items
            .get(shelf_id)
            .cloned()
            .unwrap_or_default()
    }

    /// `itemById`: the card of one title, built from the raw item.
    pub fn item_by_id(&self, title_id: &str) -> Option<LibraryCard> {
        self.shared
            .inner
            .lock()
            .raw_items
            .iter()
            .find(|item| item.title_id == title_id)
            .map(CloudLibraryItem::to_card)
    }

    pub fn is_loading(&self) -> bool {
        self.shared.inner.lock().is_loading
    }

    pub fn status_text(&self) -> String {
        self.shared.inner.lock().status_text.clone()
    }

    pub fn last_error(&self) -> String {
        self.shared.inner.lock().last_error.clone()
    }
}

/// The `Result<T>::error` string the C++ showed verbatim in the empty-state view
/// (`LibraryView.qml`: `library.lastError`): `"HTTP %1: %2"` for a non-2xx
/// (`XCloudApiClient.cpp`) and the bare transport message otherwise
/// (`HttpTransport.cpp`: `reply->errorString()` / `"Request timed out"`). The
/// Rust [`xcloud_api::Error`] `Display` adds a variant prefix, so it is unwrapped here.
fn library_error_text(error: &xcloud_api::Error) -> String {
    use xcloud_api::Error;
    match error {
        Error::Status { status, body } => format!("HTTP {status}: {body}"),
        Error::Http(message)
        | Error::Timeout(message)
        | Error::Auth(message)
        | Error::TokenStore(message)
        | Error::Other(message) => message.clone(),
        Error::Json(error) => error.to_string(),
    }
}

/// The asynchronous body of `refresh`. Every step re-checks `generation` so a
/// superseded load never publishes.
async fn load_library(
    api: Arc<XCloudApiClient>,
    catalog: Option<Arc<GamePassCatalogClient>>,
    shared: Arc<Shared>,
    generation: u64,
    query: CatalogQuery,
    cache_path: Option<PathBuf>,
) {
    // The catalog of the last launch, read while the titles load.
    let disk = match cache_path.clone() {
        Some(path) => tokio::task::spawn_blocking(move || load_catalog_file(&path))
            .await
            .unwrap_or_default(),
        None => CatalogFile::default(),
    };
    let disk_fresh = !disk.products.is_empty()
        && unix_now().saturating_sub(disk.saved_at) < CATALOG_CACHE_TTL.as_secs();
    let titles = match api.get_cloud_titles().await {
        Ok(titles) => titles,
        Err(error) => {
            let mut inner = shared.inner.lock();
            if inner.load_generation != generation {
                return;
            }
            inner.is_loading = false;
            inner.status_text.clear();
            inner.authentication_required =
                matches!(error, xcloud_api::Error::Status { status: 401, .. });
            inner.last_error = library_error_text(&error);
            shared.publish(&inner);
            return;
        }
    };
    if shared.inner.lock().load_generation != generation {
        return;
    }

    let mru = api.get_cloud_titles_mru(DEFAULT_MRU_LIMIT).await;
    let all_ids: Vec<String>;
    let product_ids = {
        let mut inner = shared.inner.lock();
        if inner.load_generation != generation {
            return;
        }
        let mru = match &mru {
            Ok(mru) => Some(mru),
            Err(error) => {
                tracing::warn!(target: LOG, "MRU fetch failed: {error}");
                None
            }
        };
        let product_ids = inner.adopt_titles(&titles, mru);
        for product in &disk.products {
            if !product.product_id.is_empty() {
                inner
                    .catalog_cache
                    .insert(product.product_id.clone(), product.clone());
            }
        }
        if !inner.catalog_cache.is_empty() {
            let cached: Vec<CatalogProduct> = inner.catalog_cache.values().cloned().collect();
            inner.apply_catalog_batch(&cached);
        }

        // Publish the library as soon as the titles arrive. Catalog enrichment
        // fetches ~28 batches and must never hold the library hostage behind a
        // "Loading artwork" screen.
        tracing::info!(
            target: LOG,
            "library: {} titles, {} product ids",
            inner.raw_items.len(),
            product_ids.len()
        );
        inner.rebuild_filtered(true);
        inner.is_loading = false;
        inner.did_load = true;
        // A fresh cache only needs the products it does not have yet.
        let product_ids: Vec<String> = if disk_fresh {
            let cached: HashSet<&str> = disk
                .products
                .iter()
                .map(|p| p.product_id.as_str())
                .collect();
            let missing: Vec<String> = product_ids
                .iter()
                .filter(|id| !cached.contains(id.as_str()))
                .cloned()
                .collect();
            tracing::info!(
                target: LOG,
                "catalog cache: {} products, {} missing",
                disk.products.len(),
                missing.len()
            );
            all_ids = product_ids;
            missing
        } else {
            all_ids = product_ids.clone();
            product_ids
        };
        let hydrate = !product_ids.is_empty() && catalog.is_some();
        inner.status_text = if hydrate {
            STATUS_LOADING_DETAILS.to_owned()
        } else {
            String::new()
        };
        shared.publish(&inner);
        if !hydrate {
            return;
        }
        product_ids
    };
    let Some(catalog) = catalog else {
        return;
    };

    // Persist successful batches immediately. A single writer coalesces snapshots,
    // avoiding competing atomic renames and preserving progress across app restarts.
    let (cache_tx, mut cache_rx) = tokio::sync::watch::channel::<Option<CatalogFile>>(None);
    let writer_path = cache_path.clone();
    let cache_writer = tokio::spawn(async move {
        while cache_rx.changed().await.is_ok() {
            let file = cache_rx.borrow_and_update().clone();
            if let (Some(path), Some(file)) = (writer_path.clone(), file) {
                let saved =
                    tokio::task::spawn_blocking(move || save_catalog_file(&path, &file)).await;
                if let Ok(Err(error)) = saved {
                    tracing::warn!(target: LOG, "cannot save catalog progress: {error}");
                }
            }
        }
    });
    let saved_at = if disk_fresh {
        disk.saved_at
    } else {
        unix_now()
    };
    let started = std::time::Instant::now();
    let result = catalog
        .hydrate_products(&product_ids, &query, |products| {
            let mut inner = shared.inner.lock();
            if inner.load_generation != generation {
                return;
            }
            inner.apply_catalog_batch(products);
            inner.rebuild_filtered(true);
            shared.publish(&inner);
            tracing::debug!(target: LOG, count = products.len(), elapsed_ms = started.elapsed().as_millis(), "catalog batch ready");
            if cache_path.is_some() {
                cache_tx.send_replace(Some(CatalogFile {
                    saved_at, products: inner.catalog_cache.values().cloned().collect(),
                }));
            }
        })
        .await;
    drop(cache_tx);
    let _ = cache_writer.await;

    {
        let mut inner = shared.inner.lock();
        if inner.load_generation != generation {
            return;
        }
        inner.status_text.clear();
        shared.publish(&inner);
    }
    let fetched = match result {
        Ok(products) => products,
        Err(error) => {
            tracing::warn!(target: LOG, "Catalog hydration failed: {error}");
            return;
        }
    };
    tracing::info!(
        target: LOG,
        "catalog {}/{}: {} of {} products in {:.1}s",
        query.language,
        query.market,
        fetched.len(),
        product_ids.len(),
        started.elapsed().as_secs_f32()
    );
    if fetched.is_empty() {
        return;
    }
    let Some(path) = cache_path else {
        return;
    };
    // Keep what this launch loaded plus the still-listed products of the old file.
    let wanted: HashSet<&str> = all_ids.iter().map(String::as_str).collect();
    let mut merged: HashMap<String, CatalogProduct> = disk
        .products
        .into_iter()
        .filter(|p| wanted.contains(p.product_id.as_str()))
        .map(|p| (p.product_id.clone(), p))
        .collect();
    for product in fetched {
        if !product.product_id.is_empty() {
            merged.insert(product.product_id.clone(), product);
        }
    }
    let file = CatalogFile {
        saved_at: if disk_fresh {
            disk.saved_at
        } else {
            unix_now()
        },
        products: merged.into_values().collect(),
    };
    let saved = tokio::task::spawn_blocking(move || save_catalog_file(&path, &file)).await;
    if let Ok(Err(error)) = saved {
        tracing::warn!(target: LOG, "cannot save the catalog cache: {error}");
    }
}

// ----------------------------------------------------------------------------- tests

/// Port of qt/tests/tst_librarycontroller.cpp.
#[cfg(test)]
mod tests {
    #[test]
    fn recent_shelf_preserves_service_order_without_duplicates() {
        let mut inner = Inner::default();
        let title = |id: &str| xcloud_proto::models::XCloudTitleDTO {
            title_id: id.into(),
            product_id: id.into(),
            ..Default::default()
        };
        let titles = XCloudTitlesResponse {
            results: vec![title("A"), title("B"), title("C")],
        };
        let mru = XCloudTitlesResponse {
            results: vec![title("C"), title("A"), title("C")],
        };
        inner.adopt_titles(&titles, Some(&mru));
        inner.build_shelves();
        let ids: Vec<_> = inner.shelf_items[SHELF_MRU]
            .iter()
            .map(|c| c.title_id.as_str())
            .collect();
        assert_eq!(ids, ["C", "A"]);
    }

    use std::time::{Duration, Instant};

    use serde_json::{Value, json};
    use xcloud_api::transport::FakeTransport;

    use super::*;
    use crate::state::LibraryHandle;

    struct Harness {
        transport: Arc<FakeTransport>,
        controller: LibraryController,
        handle: LibraryHandle,
        /// Every snapshot the controller published, in order (the `Repaint` spy).
        published: Arc<Mutex<Vec<LibrarySnapshot>>>,
    }

    impl Harness {
        fn new() -> Self {
            let transport = Arc::new(FakeTransport::new());
            let api = Arc::new(XCloudApiClient::new(transport.clone()));
            api.update_credentials("https://host", "GS");
            let catalog = Arc::new(GamePassCatalogClient::new(transport.clone()));
            let (handle, snapshot, commands) = LibraryHandle::new(LibrarySnapshot::default());
            let published: Arc<Mutex<Vec<LibrarySnapshot>>> = Arc::new(Mutex::new(Vec::new()));
            let repaint: Repaint = {
                let published = published.clone();
                let snapshot = snapshot.clone();
                Arc::new(move || published.lock().push(snapshot.read().clone()))
            };
            let controller = LibraryController::new(
                api,
                Some(catalog),
                snapshot,
                commands,
                repaint,
                tokio::runtime::Handle::current(),
            );
            Self {
                transport,
                controller,
                handle,
                published,
            }
        }

        fn with_cache_dir(self, dir: &Path) -> Self {
            Self {
                controller: self
                    .controller
                    .with_catalog_cache_dir(Some(dir.to_path_buf())),
                ..self
            }
        }

        fn game_count(&self) -> usize {
            self.handle.read().game_count()
        }

        fn item_name(&self, title_id: &str) -> String {
            self.controller
                .item_by_id(title_id)
                .map(|card| card.name)
                .unwrap_or_default()
        }

        /// The distinct shelf lists that were published, like the C++ test's
        /// `publishedShelves` collected on `itemsChanged` (status-only publishes
        /// repeat the previous shelves and are not counted).
        fn published_shelves(&self) -> Vec<Vec<Shelf>> {
            let mut out: Vec<Vec<Shelf>> = Vec::new();
            for snapshot in self.published.lock().iter() {
                if out.last() != Some(&snapshot.shelves) {
                    out.push(snapshot.shelves.clone());
                }
            }
            out
        }

        fn catalog_batches(&self) -> Vec<Vec<String>> {
            self.transport
                .requests()
                .iter()
                .filter(|request| request.url.contains("catalog.gamepass.com"))
                .map(|request| {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    body["Products"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|id| id.as_str().unwrap().to_owned())
                        .collect()
                })
                .collect()
        }
    }

    /// `QTRY_VERIFY_WITH_TIMEOUT`: polls until `condition` holds.
    async fn wait_for(timeout_ms: u64, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "condition not met within {timeout_ms} ms"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn enqueue_enriched_library(transport: &FakeTransport) {
        transport.enqueue_json(
            "/v2/titles",
            r#"
        {"results":[
          {"titleId":"T1","details":{"productId":"P1","hasEntitlement":true,"supportedInputTypes":["Controller"]}},
          {"titleId":"T2","details":{"productId":"P2","name":"Beta","supportedInputTypes":["Touch"]}}
        ]}"#,
            200,
        );
        transport.enqueue_json(
            "/v2/titles/mru",
            r#"{"results":[{"titleId":"T1","details":{"productId":"P1","name":"Alpha"}}]}"#,
            200,
        );
        transport.enqueue_json(
            "catalog.gamepass.com",
            r#"
        {"Products":{
          "P1":{"ProductId":"P1","XCloudTitleId":"T1","ProductTitle":"Alpha Deluxe","PublisherName":"Pub",
                "ProductDescriptionShort":"desc","Image_Poster":{"URL":"https://img.example/p1.png"},
                "Attributes":[{"Name":"capability","LocalizedName":"Online co-op (2-12)"}]},
          "P2":{"ProductId":"P2","XCloudTitleId":"T2","ProductTitle":"Beta",
                "Image_Hero":{"URL":"//img.example/p2.png"}}
        }}"#,
            200,
        );
    }

    fn row(id: &str) -> LibraryCard {
        LibraryCard {
            title_id: id.to_owned(),
            ..LibraryCard::default()
        }
    }

    #[tokio::test]
    async fn model_updates_without_resetting_shelves() {
        // The GameListModel half: rows move/insert/remove, never reset.
        let mut model = GameListModel::new();
        assert_eq!(
            model.set_items(vec![row("a"), row("b")]),
            ModelUpdate::Incremental
        );
        assert_eq!(
            model.set_items(vec![row("b"), row("c"), row("a")]),
            ModelUpdate::Incremental
        );
        assert_eq!(model.row_count(), 3);
        assert_eq!(model.row(0), Some(&row("b")));
        assert_eq!(model.items(), &[row("b"), row("c"), row("a")]);
        assert_eq!(model.set_items(vec![row("a")]), ModelUpdate::Incremental);
        assert_eq!(model.row_count(), 1);
        assert_eq!(model.row(0), Some(&row("a")));
        assert_eq!(model.set_items(vec![]), ModelUpdate::Incremental);
        assert_eq!(model.row_count(), 0);
        assert_eq!(model.set_items(vec![]), ModelUpdate::Unchanged);

        // The controller half: a catalog batch keeps shelf identity and order.
        let h = Harness::new();
        enqueue_enriched_library(&h.transport);
        h.controller.refresh(true);
        wait_for(8000, || h.item_name("T1") == "Alpha Deluxe").await;
        wait_for(8000, || h.controller.status_text().is_empty()).await;

        let published = h.published_shelves();
        let before = published
            .iter()
            .find(|shelves| !shelves.is_empty())
            .unwrap();
        let after = published.last().unwrap();
        assert_ne!(before, after, "the catalog batch changed the shelf content");
        assert_eq!(
            before.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            after.iter().map(|s| s.id.as_str()).collect::<Vec<_>>()
        );
        for (old, new) in before.iter().zip(after) {
            assert_eq!(old.title, new.title);
            assert_eq!(
                old.items.iter().map(|c| &c.title_id).collect::<Vec<_>>(),
                new.items.iter().map(|c| &c.title_id).collect::<Vec<_>>()
            );
            let mut model = GameListModel::new();
            model.set_items(old.items.clone());
            assert_eq!(model.set_items(new.items.clone()), ModelUpdate::Incremental);
            assert_eq!(model.items(), new.items.as_slice());
        }
        assert_eq!(before[0].items[0].name, "P1");
        assert_eq!(after[0].items[0].name, "Alpha Deluxe");
    }

    #[tokio::test]
    async fn names_survive_a_reload_whose_catalog_fails() {
        let h = Harness::new();
        enqueue_enriched_library(&h.transport);
        h.controller.refresh(true);
        wait_for(8000, || h.item_name("T1") == "Alpha Deluxe").await;

        // Switch language: titles reload, the new catalog request fails everywhere.
        h.transport.enqueue_json(
            "/v2/titles",
            r#"{"results":[{"titleId":"T1","details":{"productId":"P1"}}]}"#,
            200,
        );
        h.transport
            .enqueue_json("/v2/titles/mru", r#"{"results":[]}"#, 200);
        for _ in 0..4 {
            h.transport
                .enqueue_json("catalog.gamepass.com", r#"{"error":"x"}"#, 504);
        }
        h.controller
            .handle_command(LibraryCommand::SetLocale("de-DE".into()));
        assert_eq!(h.controller.catalog_query().market, "DE");
        wait_for(8000, || h.handle.read().all_items.len() == 1).await;
        assert_eq!(
            h.item_name("T1"),
            "Alpha Deluxe",
            "cached catalog data is kept"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn catalog_cache_restores_names_on_the_next_launch() {
        let dir = std::env::temp_dir().join(format!(
            "openxcloud-catalog-cache-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let file = catalog_cache_path(&dir, &CatalogQuery::default());

        // First launch: the catalog loads from the network and is written to disk.
        let h = Harness::new().with_cache_dir(&dir);
        enqueue_enriched_library(&h.transport);
        h.controller.refresh(true);
        wait_for(8000, || file.exists()).await;
        let saved = load_catalog_file(&file);
        assert_eq!(saved.products.len(), 2);

        // Next launch: names come from disk and nothing is fetched (the file is fresh).
        let h = Harness::new().with_cache_dir(&dir);
        h.transport.enqueue_json(
            "/v2/titles",
            r#"{"results":[{"titleId":"T1","details":{"productId":"P1"}},{"titleId":"T2","details":{"productId":"P2"}}]}"#,
            200,
        );
        h.transport
            .enqueue_json("/v2/titles/mru", r#"{"results":[]}"#, 200);
        h.controller.refresh(true);
        wait_for(8000, || h.item_name("T1") == "Alpha Deluxe").await;
        wait_for(8000, || !h.controller.is_loading()).await;
        assert!(
            h.catalog_batches().is_empty(),
            "a fresh cache is not refetched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn loads_and_enriches_library() {
        let h = Harness::new();
        enqueue_enriched_library(&h.transport);

        h.controller.refresh(true);
        wait_for(8000, || !h.handle.read().all_items.is_empty()).await;
        assert!(!h.controller.is_loading());
        assert_eq!(h.handle.read().all_items.len(), 2);

        // Catalog enrichment is applied after the library is already visible.
        wait_for(8000, || h.item_name("T1") == "Alpha Deluxe").await;

        let first = h.controller.item_by_id("T1").unwrap();
        assert_eq!(first.name, "Alpha Deluxe");
        assert_eq!(first.publisher_name, "Pub");
        assert_eq!(first.poster_image_url, "https://img.example/p1.png");
        assert_eq!(first.max_players, 12);
        assert!(first.is_in_mru);

        let second = h.controller.item_by_id("T2").unwrap();
        assert_eq!(second.hero_image_url, "https://img.example/p2.png");
        let snapshot = h.handle.read();
        assert_eq!(snapshot.game_count(), 2);
        assert_eq!(snapshot.items[0].name, "Alpha Deluxe");

        assert!(!snapshot.shelves.is_empty());
        assert_eq!(snapshot.shelves[0].id, SHELF_MRU);
        assert_eq!(snapshot.shelves[0].items.len(), 1);
        assert!(!snapshot.feature_groups.is_empty());
        assert_eq!(snapshot.feature_groups[0].title, GROUP_PLAY_TOGETHER);
        assert_eq!(snapshot.feature_groups[0].features, vec!["Online co-op"]);
        assert_eq!(snapshot.shelf_items(SHELF_MRU).len(), 1);
        assert_eq!(snapshot.item_by_id("T2").unwrap().name, "Beta");

        let published: Vec<Vec<Shelf>> = h
            .published_shelves()
            .into_iter()
            .filter(|shelves| !shelves.is_empty())
            .collect();
        assert_eq!(published.len(), 2);
        let initial_items = &published[0][0].items;
        assert_eq!(initial_items[0].name, "P1");
        let enriched_items = &published[1][0].items;
        assert_eq!(enriched_items[0].name, "Alpha Deluxe");
        assert_eq!(
            enriched_items[0].poster_image_url,
            "https://img.example/p1.png"
        );

        h.controller.set_browse_shelf(SHELF_MRU);
        assert_eq!(h.game_count(), 1);
        h.controller.set_query("Beta");
        assert_eq!(h.game_count(), 1);
        assert_eq!(h.handle.read().items[0].title_id, "T2");
        h.controller.set_query("");
        assert_eq!(h.game_count(), 1);
        h.controller.set_browse_shelf("");
        assert_eq!(h.game_count(), 2);
        assert_eq!(h.controller.shelf_items(SHELF_MRU).len(), 1);
        assert!(h.controller.shelf_items("nope").is_empty());
        assert!(h.controller.item_by_id("nope").is_none());
    }

    #[tokio::test]
    async fn publishes_before_catalog() {
        let h = Harness::new();
        h.transport.enqueue_json(
            "/v2/titles",
            r#"
        {"results":[
          {"titleId":"T1","details":{"productId":"P1","name":"Alpha"}},
          {"titleId":"T2","details":{"productId":"P2","name":"Beta"}}
        ]}"#,
            200,
        );
        h.transport
            .enqueue_json("/v2/titles/mru", r#"{"results":[]}"#, 200);
        h.transport.enqueue_json_delayed(
            "catalog.gamepass.com",
            r#"
        {"Products":{
          "P1":{"ProductId":"P1","XCloudTitleId":"T1","ProductTitle":"Alpha Deluxe"},
          "P2":{"ProductId":"P2","XCloudTitleId":"T2","ProductTitle":"Beta Deluxe"}
        }}"#,
            400,
        );

        assert_eq!(h.handle.read().status_text, "");
        h.controller.refresh(true);
        // The first publish is the loading state.
        assert_eq!(h.handle.read().status_text, STATUS_LOADING_GAMES);
        assert!(h.handle.read().is_loading);
        wait_for(3000, || !h.handle.read().all_items.is_empty()).await;

        // Titles are published before the catalog answers; the UI is never held at
        // "Loading artwork".
        let snapshot = h.handle.read();
        assert_eq!(snapshot.all_items.len(), 2);
        assert_eq!(h.item_name("T1"), "Alpha");
        assert_eq!(snapshot.item_by_id("T1").unwrap().name, "Alpha");
        assert!(!snapshot.is_loading);
        assert!(!h.controller.is_loading());
        assert_eq!(snapshot.status_text, STATUS_LOADING_DETAILS);
        assert_eq!(h.controller.status_text(), STATUS_LOADING_DETAILS);
        drop(snapshot);

        wait_for(3000, || h.item_name("T1") == "Alpha Deluxe").await;
        wait_for(3000, || h.handle.read().status_text.is_empty()).await;
        assert_eq!(
            h.handle.read().item_by_id("T2").unwrap().name,
            "Beta Deluxe"
        );
    }

    #[tokio::test]
    async fn prioritizes_recent_and_publishes_each_batch() {
        let dir = std::env::temp_dir().join(format!(
            "openxcloud-progress-{}-{}",
            std::process::id(),
            unix_now()
        ));
        let file = catalog_cache_path(&dir, &CatalogQuery::default());
        let h = Harness::new().with_cache_dir(&dir);
        let titles: Vec<Value> = (0..101)
            .map(|i| json!({"titleId": format!("T{i}"), "details": {"productId": format!("P{i}")}}))
            .collect();
        h.transport
            .enqueue_json("/v2/titles", &json!({"results": titles}).to_string(), 200);
        h.transport.enqueue_json(
            "/v2/titles/mru",
            r#"{"results":[{"titleId":"T100","details":{"productId":"P100"}}]}"#,
            200,
        );
        h.transport.enqueue_json_delayed(
            "catalog.gamepass.com",
            r#"{"Products":{"P100":{"ProductTitle":"Recent Game","Image_Poster":{"URL":"https://img/recent"}}}}"#,
            50,
        );
        h.transport.enqueue_json_delayed(
            "catalog.gamepass.com",
            r#"{"Products":{"P99":{"ProductTitle":"Later Game"}}}"#,
            600,
        );

        h.controller.refresh(false);
        wait_for(5000, || h.item_name("T100") == "Recent Game").await;
        assert!(!h.controller.is_loading());
        assert_eq!(h.controller.status_text(), STATUS_LOADING_DETAILS);
        assert_eq!(h.item_name("T99"), "P99");
        wait_for(500, || file.exists()).await;
        assert_eq!(
            load_catalog_file(&file).products[0].product_title,
            "Recent Game"
        );
        assert_eq!(
            h.controller.shelf_items(SHELF_MRU)[0].poster_image_url,
            "https://img/recent"
        );
        assert_eq!(
            h.handle.read().shelf_items(SHELF_MRU)[0].poster_image_url,
            "https://img/recent"
        );
        // Only the first batch has been applied: the grid shows the enriched MRU
        // title first and the raw ones after it.
        let snapshot = h.handle.read();
        assert_eq!(snapshot.items.len(), 101);
        assert_eq!(snapshot.items[0].name, "Recent Game");
        assert_eq!(snapshot.all_items.len(), 101);
        drop(snapshot);

        let batches = h.catalog_batches();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 12);
        assert_eq!(batches[0][0], "P100");
        assert_eq!(batches[1].len(), 89);
        assert_eq!(batches[1][0], "P11");

        wait_for(5000, || h.item_name("T99") == "Later Game").await;
        wait_for(5000, || h.controller.status_text().is_empty()).await;
        assert_eq!(h.item_name("T100"), "Recent Game");
        assert_eq!(
            h.handle.read().item_by_id("T99").unwrap().name,
            "Later Game"
        );

        // Every batch was published separately: 101 raw items, then the "Recent Game"
        // batch, then the "Later Game" batch.
        let names_of_t99: Vec<String> = h
            .published
            .lock()
            .iter()
            .filter_map(|s| s.item_by_id("T99").map(|c| c.name.clone()))
            .collect();
        assert!(names_of_t99.contains(&"P99".to_owned()));
        assert!(names_of_t99.contains(&"Later Game".to_owned()));
        let names_of_t100: Vec<String> = h
            .published
            .lock()
            .iter()
            .filter_map(|s| s.item_by_id("T100").map(|c| c.name.clone()))
            .collect();
        assert_eq!(names_of_t100.first().map(String::as_str), Some("P100"));

        // A second refresh without force keeps the loaded library (m_didLoad).
        let publishes = h.published.lock().len();
        h.controller.refresh(false);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(h.published.lock().len(), publishes);
        assert_eq!(h.transport.request_count(), 4);
    }

    #[tokio::test]
    async fn search_and_sort() {
        let h = Harness::new();
        h.transport.enqueue_json(
            "/v2/titles",
            r#"
        {"results":[
          {"titleId":"T1","details":{"productId":"P1","name":"Zelda"}},
          {"titleId":"T2","details":{"productId":"P2","name":"Halo"}}
        ]}"#,
            200,
        );
        h.transport
            .enqueue_json("/v2/titles/mru", r#"{"results":[]}"#, 200);

        h.controller.refresh(true);
        wait_for(8000, || !h.handle.read().all_items.is_empty()).await;
        // No canned catalog reply: hydration fails fast and the status clears.
        wait_for(8000, || h.controller.status_text().is_empty()).await;

        let shelves_before = h.handle.read().shelves.clone();
        let publishes_before = h.published.lock().len();
        h.controller.set_query("hal");
        assert_eq!(h.game_count(), 1);
        let snapshot = h.handle.read();
        assert_eq!(
            snapshot.shelves, shelves_before,
            "shelvesChanged must not fire"
        );
        assert_eq!(h.published.lock().len(), publishes_before + 1);
        assert_eq!(snapshot.items.len(), 1);
        assert_eq!(snapshot.items[0].name, "Halo");
        assert_eq!(snapshot.query, "hal");
        drop(snapshot);

        h.controller.set_query("");
        let snapshot = h.handle.read();
        assert_eq!(snapshot.items.len(), 2);
        assert_eq!(snapshot.items[0].name, "Halo");
        drop(snapshot);

        // Alphabetical sort and the recent-only filter go through SetFilter.
        let mut filter = LibraryFilter::default();
        filter.set_sort("alphabetical");
        h.controller
            .handle_command(LibraryCommand::SetFilter(filter.clone()));
        assert_eq!(h.handle.read().items[0].name, "Halo");
        assert_eq!(h.handle.read().filter, filter);
        filter.set_recent_only(true);
        h.controller.set_filter(filter);
        assert_eq!(h.game_count(), 0);
        h.controller
            .handle_command(LibraryCommand::SetFilter(LibraryFilter::default()));
        assert_eq!(h.game_count(), 2);
    }

    #[tokio::test]
    async fn title_fetch_error_is_published() {
        let h = Harness::new();
        h.transport.enqueue_error("/v2/titles", "offline");
        h.controller.refresh(true);
        wait_for(3000, || !h.handle.read().is_loading).await;
        let snapshot = h.handle.read();
        // The bare transport message, like the C++ `reply->errorString()`.
        assert_eq!(snapshot.last_error, "offline");
        assert_eq!(snapshot.status_text, "");
        assert!(snapshot.is_empty());
        assert_eq!(h.controller.last_error(), snapshot.last_error);
        drop(snapshot);

        // The failed load did not count as loaded: a plain refresh retries.
        h.transport.enqueue_json(
            "/v2/titles",
            r#"{"results":[{"titleId":"T1","details":{"productId":"P1","name":"A"}}]}"#,
            200,
        );
        h.transport
            .enqueue_json("/v2/titles/mru", r#"{"results":[]}"#, 200);
        h.controller
            .handle_command(LibraryCommand::Refresh { force: false });
        wait_for(3000, || !h.handle.read().all_items.is_empty()).await;
        assert_eq!(h.handle.read().last_error, "");
    }

    #[tokio::test]
    async fn unauthorized_library_response_requires_sign_in() {
        // `XCloudApiClient.cpp`: `QStringLiteral("HTTP %1: %2").arg(status).arg(bodyText())`.
        let h = Harness::new();
        h.transport
            .enqueue_json("/v2/titles", r#"{"error":"expired"}"#, 401);
        h.controller.refresh(true);
        wait_for(3000, || !h.handle.read().is_loading).await;
        assert_eq!(
            h.handle.read().last_error,
            r#"HTTP 401: {"error":"expired"}"#
        );
        assert!(h.handle.read().authentication_required);
    }

    #[test]
    fn error_text_matches_the_cpp_result_strings() {
        use xcloud_api::Error;
        assert_eq!(
            library_error_text(&Error::Status {
                status: 503,
                body: "busy".to_owned()
            }),
            "HTTP 503: busy"
        );
        assert_eq!(
            library_error_text(&Error::Status {
                status: 404,
                body: String::new()
            }),
            "HTTP 404: "
        );
        assert_eq!(
            library_error_text(&Error::Http("Host not found".to_owned())),
            "Host not found"
        );
        assert_eq!(
            library_error_text(&Error::Timeout("Request timed out".to_owned())),
            "Request timed out"
        );
        assert_eq!(library_error_text(&Error::Other("boom".to_owned())), "boom");
    }

    #[tokio::test]
    async fn snapshot_shelf_items_returns_the_full_list_not_the_preview() {
        // `shelves` carry `items.mid(0, 20)`; `shelfItems(id)` returns `m_shelfItems`.
        let h = Harness::new();
        let titles: Vec<Value> = (0..25)
            .map(|i| {
                json!({"titleId": format!("T{i:02}"),
                       "details": {"productId": format!("P{i:02}"), "name": format!("Game {i:02}")}})
            })
            .collect();
        h.transport
            .enqueue_json("/v2/titles", &json!({"results": titles}).to_string(), 200);
        h.transport.enqueue_json(
            "/v2/titles/mru",
            &json!({"results": titles}).to_string(),
            200,
        );
        h.controller.refresh(true);
        wait_for(8000, || !h.handle.read().all_items.is_empty()).await;
        wait_for(8000, || h.controller.status_text().is_empty()).await;

        let snapshot = h.handle.read();
        assert_eq!(snapshot.shelves[0].id, SHELF_MRU);
        assert_eq!(snapshot.shelves[0].items.len(), SHELF_PREVIEW_LEN);
        assert_eq!(snapshot.shelf_items(SHELF_MRU).len(), 25);
        assert_eq!(
            snapshot.shelf_items(SHELF_MRU),
            h.controller.shelf_items(SHELF_MRU).as_slice()
        );
        assert_eq!(
            snapshot.shelf_items(SHELF_MRU)[..SHELF_PREVIEW_LEN],
            snapshot.shelves[0].items[..]
        );
        assert_eq!(snapshot.shelf_items(SHELF_MRU)[24].title_id, "T24");
        assert!(snapshot.shelf_items("nope").is_empty());
        drop(snapshot);

        // Browsing the shelf serves all of it, not the preview.
        h.controller.set_browse_shelf(SHELF_MRU);
        assert_eq!(h.game_count(), 25);
    }

    #[test]
    fn feature_grouping_matches_the_cpp_tables() {
        assert_eq!(group_title_for("Online co-op (2-12)"), GROUP_PLAY_TOGETHER);
        assert_eq!(
            group_title_for("Cross-platform multiplayer"),
            GROUP_PLAY_TOGETHER
        );
        assert_eq!(group_title_for("Single player"), GROUP_PLAY_TOGETHER);
        assert_eq!(group_title_for("4K Ultra HD"), GROUP_PICTURE_AND_SOUND);
        assert_eq!(group_title_for("Dolby Atmos"), GROUP_PICTURE_AND_SOUND);
        assert_eq!(group_title_for("120 fps"), GROUP_PICTURE_AND_SOUND);
        assert_eq!(group_title_for("Xbox Play Anywhere"), GROUP_MORE);
        assert_eq!(group_id_for(GROUP_MORE), "more");

        let item = CloudLibraryItem {
            attributes: vec![
                xcloud_proto::models::LibraryAttribute::new("a", "Online co-op (2-12)"),
                xcloud_proto::models::LibraryAttribute::new("b", "Online co-op (2-4)"),
                xcloud_proto::models::LibraryAttribute::new("c", ""),
                xcloud_proto::models::LibraryAttribute::new("d", "HDR10"),
            ],
            ..CloudLibraryItem::default()
        };
        assert_eq!(feature_names_for(&item), vec!["Online co-op", "HDR10"]);
    }

    #[test]
    fn large_model_resets_only_on_reorder() {
        let many: Vec<LibraryCard> = (0..30).map(|i| row(&format!("t{i}"))).collect();
        let mut model = GameListModel::new();
        assert_eq!(model.set_items(many.clone()), ModelUpdate::Reset);
        let mut renamed = many.clone();
        renamed[3].name = "changed".to_owned();
        assert_eq!(model.set_items(renamed.clone()), ModelUpdate::Incremental);
        assert_eq!(model.items(), renamed.as_slice());
        let mut reversed = renamed;
        reversed.reverse();
        assert_eq!(model.set_items(reversed.clone()), ModelUpdate::Reset);
        assert_eq!(model.items(), reversed.as_slice());
    }

    #[test]
    fn catalog_query_follows_the_locale() {
        let q = catalog_query_for("de-DE");
        assert_eq!((q.language.as_str(), q.market.as_str()), ("de-DE", "DE"));
        let q = catalog_query_for("pt_br");
        assert_eq!((q.language.as_str(), q.market.as_str()), ("pt-BR", "BR"));
        let q = catalog_query_for("xx-YY");
        assert_eq!((q.language.as_str(), q.market.as_str()), ("en-US", "US"));
        assert_eq!(q.hydration, CatalogQuery::default().hydration);
    }
}
