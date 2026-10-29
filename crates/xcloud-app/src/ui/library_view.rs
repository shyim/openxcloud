//! The signed-in browse experience: top bar, collapsible sidebar and the Home,
//! Library, shelf, search and game pages.
//!
//! Layout (Xbox-app inspired):
//!
//! ```text
//! ┌ logo  ⧉ ────────────── ‹  ( ⌕ Search your games ) ─────────────── ⚙  ◯ ┐
//! │ ┌─────────────┐  ambient glow from the featured game                     │
//! │ │ ⌂ Home      │  Jump back in   ▢ ▢ ▢ ▢ ▢                                 │
//! │ │ ▤ Library   │  ┌──────── featured ────────┐┌──────── featured ───────┐  │
//! │ └─────────────┘  └──────────────────────────┘└──────────────────────────┘  │
//! │ ┌ Recent games┐  • • • •                                                  │
//! │ │ ▢ Forza …   │  Recently added            See all ›                      │
//! │ │ ▢ Halo …    │  ▢ ▢ ▢ ▢ ▢ ▢                                               │
//! └─┴─────────────┴────────────────────────────────────────────────────────────┘
//! ```
//!
//! The view keeps only navigation state (route, history, search text with its 120 ms
//! debounce, a mirror of the filter, carousel position); everything else is read from
//! the [`LibrarySnapshot`] each frame and changed through [`LibraryCommand`]s.

use std::collections::HashMap;

use egui::{
    Align, Align2, Color32, CornerRadius, Id, Key, Layout, Modifiers, Rect, RichText, Sense,
    Stroke, StrokeKind, Ui, UiBuilder, Vec2, pos2, vec2,
};
use xcloud_proto::library_filter::{LibraryFilter, SORT_ALPHABETICAL, SORT_RECENT_FIRST};
use xcloud_proto::models::LibraryCard;

use crate::library::{SHELF_MRU, SHELF_RECENTLY_ADDED};
use crate::state::{
    LibraryCommand, LibraryHandle, LibrarySnapshot, SessionCommand, SessionHandle, Shelf,
};
use crate::theme::{self, Weight, font_w};
use crate::ui::icons::{self, Icon};
use crate::ui::kit::{self, Button, Tone};
use crate::ui::widgets::{self, ArtworkKind, ArtworkSource};
use crate::user_data::{self, UserData, UserDataHandle};

/// Search debounce in seconds.
pub const SEARCH_DEBOUNCE_SECS: f64 = 0.120;

/// Accessible label of the top-bar settings button.
pub const SETTINGS_LABEL: &str = "Open settings";
/// Accessible label of the account button.
pub const ACCOUNT_LABEL: &str = "Account";
/// Search field hint.
pub const SEARCH_HINT: &str = "Search your games";

/// What the shell must do after a frame.
#[derive(Debug, Clone, PartialEq)]
pub enum LibraryAction {
    SignIn,
    Play(Box<LibraryCard>),
    /// The settings button was clicked: toggle the settings sheet.
    OpenSettings,
}

/// Top-level destinations in the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    #[default]
    Home,
    Library,
}

/// Library listing tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LibraryTab {
    Recent,
    #[default]
    All,
    Favorites,
}

/// Grid density on the Library page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Density {
    #[default]
    Large,
    Compact,
}

impl Density {
    fn min_tile(self) -> f32 {
        match self {
            Density::Large => 196.0,
            Density::Compact => 138.0,
        }
    }
}

/// Where the content area is.
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    Page(Page),
    /// "See all" on a home shelf: id and title.
    Shelf(String, String),
    /// A game's detail page.
    Game(Box<LibraryCard>),
    /// "See all" on the Picked for you shelf.
    Picks,
}

/// Which cards a grid page shows. Local lists are intersected with the controller's
/// filtered `items`, so search and filters keep working on them.
/// Title of the recommendation shelf and page.
const PICKS_TITLE: &str = "Picked for you";
/// Tiles in the Home favourites and picks shelves.
const HOME_SHELF_LEN: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GridList {
    /// The controller's filtered, sorted list.
    All,
    /// Played here first, then the service's recent list.
    Recent,
    Favorites,
    Picks,
}

impl Default for Route {
    fn default() -> Self {
        Route::Page(Page::Home)
    }
}

enum Nav {
    SignIn,
    Go(Route),
    Back,
    Play(Box<LibraryCard>),
    Settings,
}

struct RecommendationCache {
    key: (u64, u64, u64),
    indices: Vec<usize>,
}

#[derive(Default)]
pub struct LibraryView {
    route: Route,
    history: Vec<Route>,
    tab: LibraryTab,
    density: Density,
    sidebar_collapsed: bool,
    search_text: String,
    search_changed_at: Option<f64>,
    focus_search: bool,
    /// Local mirror of the controller's filter.
    filter: LibraryFilter,
    seen_filter: LibraryFilter,
    favorites_only: bool,
    recommendations: std::cell::RefCell<Option<RecommendationCache>>,
    /// The shelf the controller was last told to browse.
    sent_shelf: Option<String>,
    carousel: usize,
    /// Pending horizontal scroll offsets for shelf rows (arrow buttons).
    shelf_scroll: HashMap<String, f32>,
    settings_open: bool,
    /// Vertical scroll of the game page last frame (fades the fixed backdrop).
    game_scroll: f32,
    /// Favourites and play history (shared with the shell, which saves it).
    user: UserDataHandle,
}

/// What sits behind the whole window this frame.
enum Backdrop<'a> {
    /// Hero art across the top of the window.
    Art {
        card: &'a LibraryCard,
        height: f32,
        opacity: f32,
    },
    /// The neutral lime glow.
    Glow,
}

impl LibraryView {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn route(&self) -> &Route {
        &self.route
    }

    /// The top-level page, for callers that only care about Home vs Library.
    pub fn page(&self) -> Page {
        match self.route {
            Route::Page(page) => page,
            _ => Page::Library,
        }
    }

    pub fn tab(&self) -> LibraryTab {
        self.tab
    }

    pub fn search_text(&self) -> &str {
        &self.search_text
    }

    pub fn filter(&self) -> &LibraryFilter {
        &self.filter
    }

    /// Shares the favourites/history store with the view.
    pub fn set_user_data(&mut self, user: UserDataHandle) {
        self.user = user;
        *self.recommendations.get_mut() = None;
    }

    pub fn user_data(&self) -> &UserDataHandle {
        &self.user
    }

    /// The cards of a local list, in its order, restricted to what the controller's
    /// current search and filters let through.
    fn grid_items<'a>(
        &self,
        snapshot: &'a LibrarySnapshot,
        list: GridList,
    ) -> Vec<&'a LibraryCard> {
        let user = self.user.read();
        if matches!(list, GridList::All | GridList::Favorites) {
            return snapshot
                .items
                .iter()
                .filter(|card| {
                    !(self.favorites_only || list == GridList::Favorites)
                        || user.is_favorite(&card.title_id)
                })
                .collect();
        }
        let visible: std::collections::HashSet<&str> =
            snapshot.items.iter().map(|c| c.title_id.as_str()).collect();
        let ordered: Vec<&'a LibraryCard> = match list {
            GridList::All => snapshot.items.iter().collect(),
            GridList::Recent => user_data::jump_back_in(
                &user,
                snapshot.shelf_items(SHELF_MRU),
                &snapshot.all_items,
                usize::MAX,
            ),
            GridList::Favorites => user_data::favorite_cards(&user, &snapshot.items),
            GridList::Picks => self.recommended_cards(snapshot, &user, 60),
        };
        ordered
            .into_iter()
            .filter(|c| visible.contains(c.title_id.as_str()))
            .filter(|c| !self.favorites_only || user.is_favorite(&c.title_id))
            .collect()
    }

    /// Re-score only when catalog/history changes, or once per minute for recency.
    fn recommended_cards<'a>(
        &self,
        snapshot: &'a LibrarySnapshot,
        user: &UserData,
        limit: usize,
    ) -> Vec<&'a LibraryCard> {
        let now = user_data::now_secs();
        let key = (snapshot.revision, user.revision, now / 60);
        let mut cache = self.recommendations.borrow_mut();
        if cache.as_ref().is_none_or(|cached| cached.key != key) {
            let by_id: HashMap<&str, usize> = snapshot
                .all_items
                .iter()
                .enumerate()
                .map(|(i, card)| (card.title_id.as_str(), i))
                .collect();
            let indices = user_data::recommendations(
                user,
                snapshot.shelf_items(SHELF_MRU),
                &snapshot.all_items,
                now,
                60,
            )
            .iter()
            .filter_map(|card| by_id.get(card.title_id.as_str()).copied())
            .collect();
            *cache = Some(RecommendationCache { key, indices });
        }
        cache
            .as_ref()
            .unwrap()
            .indices
            .iter()
            .take(limit)
            .filter_map(|&i| snapshot.all_items.get(i))
            .collect()
    }

    pub fn sidebar_collapsed(&self) -> bool {
        self.sidebar_collapsed
    }

    pub fn set_sidebar_collapsed(&mut self, collapsed: bool) {
        self.sidebar_collapsed = collapsed;
    }

    /// Switches the Library grid between large and compact tiles.
    pub fn set_compact_grid(&mut self, compact: bool) {
        self.density = if compact {
            Density::Compact
        } else {
            Density::Large
        };
    }

    /// Tells the view whether the settings sheet is open (highlights the button).
    pub fn set_settings_open(&mut self, open: bool) {
        self.settings_open = open;
    }

    /// A non-blank query is being shown.
    pub fn searching(&self) -> bool {
        !self.search_text.trim().is_empty()
    }

    /// Navigates to a top-level page, clearing search and history.
    pub fn navigate(&mut self, page: Page, library: &LibraryHandle) {
        self.history.clear();
        self.clear_search(library);
        self.go(Route::Page(page), library, false);
    }

    /// Opens a game's detail page.
    /// Opens the Library page on `tab`.
    pub fn open_library_tab(&mut self, tab: LibraryTab, library: &LibraryHandle) {
        self.tab = tab;
        self.navigate(Page::Library, library);
        self.apply_route(library);
    }

    /// Opens the "Picked for you" page.
    pub fn open_picks(&mut self, library: &LibraryHandle) {
        self.go(Route::Picks, library, true);
    }

    pub fn open_game(&mut self, card: &LibraryCard, library: &LibraryHandle) {
        self.go(Route::Game(Box::new(card.clone())), library, true);
    }

    fn go(&mut self, route: Route, library: &LibraryHandle, remember: bool) {
        if route == self.route {
            return;
        }
        if remember {
            self.history.push(std::mem::take(&mut self.route));
        }
        self.route = route;
        self.apply_route(library);
    }

    /// Search results replace the content, except over a game page opened from them.
    fn showing_results(&self) -> bool {
        self.searching() && !matches!(self.route, Route::Game(_))
    }

    fn back(&mut self, library: &LibraryHandle) {
        // A game page opened from search results goes back to the results.
        if matches!(self.route, Route::Game(_)) {
            self.route = self.history.pop().unwrap_or_default();
            self.apply_route(library);
            return;
        }
        if self.searching() {
            self.clear_search(library);
            return;
        }
        if let Some(previous) = self.history.pop() {
            self.route = previous;
            self.apply_route(library);
        }
    }

    fn can_go_back(&self) -> bool {
        self.searching() || !self.history.is_empty()
    }

    /// Keeps the controller's browse shelf and recent-only filter in step with the route.
    fn apply_route(&mut self, library: &LibraryHandle) {
        let shelf = match &self.route {
            Route::Shelf(id, _) => id.clone(),
            _ => String::new(),
        };
        if self.sent_shelf.as_deref() != Some(shelf.as_str()) {
            library.send(LibraryCommand::SetBrowseShelf(shelf.clone()));
            self.sent_shelf = Some(shelf);
        }
        // "Recently played" is a local list now (history + service), never the
        // controller's MRU-only filter.
        if self.filter.recent_only {
            let mut filter = self.filter.clone();
            filter.recent_only = false;
            self.set_filter(filter, library);
        }
    }

    fn set_tab(&mut self, tab: LibraryTab, library: &LibraryHandle) {
        self.tab = tab;
        self.apply_route(library);
    }

    fn clear_search(&mut self, library: &LibraryHandle) {
        self.search_changed_at = None;
        if !self.search_text.is_empty() {
            self.search_text.clear();
            library.send(LibraryCommand::SetQuery(String::new()));
        }
    }

    fn set_filter(&mut self, filter: LibraryFilter, library: &LibraryHandle) {
        if filter != self.filter {
            self.filter = filter.clone();
            library.send(LibraryCommand::SetFilter(filter));
        }
    }

    fn sync(&mut self, snapshot: &LibrarySnapshot) {
        if snapshot.filter != self.seen_filter {
            self.seen_filter = snapshot.filter.clone();
            self.filter = snapshot.filter.clone();
        }
        if let Route::Shelf(id, title) = &mut self.route
            && let Some(shelf) = snapshot.shelves.iter().find(|s| &s.id == id)
        {
            *title = shelf.title.clone();
        }
        // Keep the detail page's card fresh as catalog batches arrive.
        if let Route::Game(card) = &mut self.route
            && let Some(fresh) = snapshot.item_by_id(&card.title_id)
            && fresh != card.as_ref()
        {
            **card = fresh.clone();
        }
    }

    fn flush_search(&mut self, ui: &Ui, library: &LibraryHandle) {
        let Some(changed_at) = self.search_changed_at else {
            return;
        };
        let now = ui.input(|i| i.time);
        let elapsed = now - changed_at;
        if elapsed >= SEARCH_DEBOUNCE_SECS {
            self.search_changed_at = None;
            library.send(LibraryCommand::SetQuery(self.search_text.clone()));
        } else {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs_f64(
                    SEARCH_DEBOUNCE_SECS - elapsed,
                ));
        }
    }

    pub fn show(
        &mut self,
        ui: &mut Ui,
        library: &LibraryHandle,
        session: &SessionHandle,
        artwork: &dyn ArtworkSource,
    ) -> Option<LibraryAction> {
        let nav =
            library.with(|snapshot| self.show_snapshot(ui, snapshot, library, session, artwork));
        self.flush_search(ui, library);
        match nav {
            Some(Nav::Go(route)) => {
                let remember = !matches!(route, Route::Page(_));
                if let Route::Page(page) = route {
                    self.navigate(page, library);
                } else {
                    self.go(route, library, remember);
                }
                None
            }
            Some(Nav::Back) => {
                self.back(library);
                None
            }
            Some(Nav::Play(card)) => Some(LibraryAction::Play(card)),
            Some(Nav::Settings) => Some(LibraryAction::OpenSettings),
            Some(Nav::SignIn) => Some(LibraryAction::SignIn),
            None => None,
        }
    }

    fn show_snapshot(
        &mut self,
        ui: &mut Ui,
        snapshot: &LibrarySnapshot,
        library: &LibraryHandle,
        session: &SessionHandle,
        artwork: &dyn ArtworkSource,
    ) -> Option<Nav> {
        self.sync(snapshot);
        if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::K)) {
            self.focus_search = true;
        }
        let mut nav = None;
        if self.can_go_back()
            && ui.input_mut(|i| {
                i.consume_key(Modifiers::NONE, Key::Escape)
                    || i.consume_key(Modifiers::COMMAND, Key::OpenBracket)
            })
        {
            nav = Some(Nav::Back);
        }

        let full = ui.max_rect();
        ui.painter().rect_filled(full, 0.0, theme::BACKGROUND);

        let top = Rect::from_min_size(full.min, vec2(full.width(), theme::HEADER_HEIGHT));
        let sidebar_t = ui.ctx().animate_bool_with_time(
            Id::unique("sidebar_collapsed"),
            self.sidebar_collapsed,
            0.16,
        );
        let wide = full.width() >= 1100.0;
        let expanded_w = if wide { theme::SIDEBAR_WIDTH } else { 248.0 };
        let sidebar_w = expanded_w + (theme::SIDEBAR_COLLAPSED_WIDTH - expanded_w) * sidebar_t;
        let body_top = top.bottom();
        let sidebar = Rect::from_min_max(
            pos2(full.left() + 12.0, body_top),
            pos2(full.left() + 12.0 + sidebar_w, full.bottom() - 12.0),
        );
        let content = Rect::from_min_max(
            pos2(sidebar.right() + 12.0, body_top),
            pos2(full.right(), full.bottom()),
        );

        let backdrop = self.backdrop(snapshot, content);
        paint_window_backdrop(ui, full, backdrop, artwork);

        // Content first so the top bar's popups draw above it.
        let content_nav = ui
            .scope_builder(UiBuilder::new().max_rect(content), |ui| {
                ui.set_clip_rect(content);
                self.content(ui, snapshot, library, artwork)
            })
            .inner;
        if nav.is_none() {
            nav = content_nav;
        }
        let side_nav = ui
            .scope_builder(UiBuilder::new().max_rect(sidebar), |ui| {
                self.sidebar(ui, snapshot, sidebar_t > 0.5, artwork)
            })
            .inner;
        if side_nav.is_some() {
            nav = side_nav;
        }
        let top_nav = self.top_bar(ui, top, snapshot, library, session);
        if top_nav.is_some() {
            nav = top_nav;
        }
        nav
    }

    /// The backdrop for the current route.
    fn backdrop<'a>(&'a self, snapshot: &'a LibrarySnapshot, content: Rect) -> Backdrop<'a> {
        if snapshot.is_empty() || self.showing_results() {
            return Backdrop::Glow;
        }
        match &self.route {
            Route::Page(Page::Home) => {
                let featured = featured_cards(snapshot, &self.user.read());
                let per_page = self.per_page(content.width() - 56.0);
                match featured.get(self.carousel * per_page).copied() {
                    Some(card) => Backdrop::Art {
                        card,
                        height: 560.0,
                        opacity: 0.34,
                    },
                    None => Backdrop::Glow,
                }
            }
            Route::Game(card) => {
                // The page's card may be a stale copy; prefer the snapshot's.
                let card = snapshot.item_by_id(&card.title_id).unwrap_or(card.as_ref());
                let hero_h = game_hero_height(content.width());
                let fade = (1.0 - self.game_scroll / hero_h).clamp(0.0, 1.0);
                Backdrop::Art {
                    card,
                    height: theme::HEADER_HEIGHT + hero_h,
                    opacity: fade,
                }
            }
            _ => Backdrop::Glow,
        }
    }

    // ------------------------------------------------------------------ top bar

    fn top_bar(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        snapshot: &LibrarySnapshot,
        library: &LibraryHandle,
        session: &SessionHandle,
    ) -> Option<Nav> {
        let mut nav = None;
        let cy = rect.center().y;
        // Left: logo + sidebar toggle.
        icons::paint(
            ui.painter(),
            icons::centered(pos2(rect.left() + 38.0, cy), 30.0),
            Icon::Logo,
            theme::ACCENT,
        );
        let toggle = Rect::from_center_size(pos2(rect.left() + 88.0, cy), vec2(38.0, 38.0));
        let (icon, label) = if self.sidebar_collapsed {
            (Icon::SidebarExpand, "Expand sidebar")
        } else {
            (Icon::SidebarCollapse, "Collapse sidebar")
        };
        if kit::place(ui, toggle, |ui| kit::icon_button(ui, icon, 38.0, label)).clicked() {
            self.sidebar_collapsed = !self.sidebar_collapsed;
        }

        // Centre: back + search.
        let search_w = (rect.width() * 0.36).clamp(320.0, 520.0);
        let search = Rect::from_center_size(rect.center(), vec2(search_w, 42.0));
        let back = Rect::from_center_size(pos2(search.left() - 30.0, cy), vec2(36.0, 36.0));
        if self.can_go_back() {
            if kit::place(ui, back, |ui| {
                kit::icon_button(ui, Icon::ChevronLeft, 36.0, "Back")
            })
            .clicked()
            {
                nav = Some(Nav::Back);
            }
        } else {
            icons::paint(
                ui.painter(),
                back.shrink(9.0),
                Icon::ChevronLeft,
                theme::TEXT_TERTIARY,
            );
        }
        self.search_box(ui, search, library);

        // Right: loading spinner, settings, account.
        let account = Rect::from_center_size(pos2(rect.right() - 36.0, cy), vec2(38.0, 38.0));
        let settings = Rect::from_center_size(pos2(account.left() - 28.0, cy), vec2(38.0, 38.0));
        if snapshot.is_loading {
            let spin = Rect::from_center_size(pos2(settings.left() - 26.0, cy), vec2(18.0, 18.0));
            kit::place(ui, spin, |ui| {
                ui.add(egui::Spinner::new().size(16.0).color(theme::TEXT_SECONDARY))
            });
        }
        if kit::place(ui, settings, |ui| {
            kit::icon_button_toned(
                ui,
                Icon::Settings,
                38.0,
                SETTINGS_LABEL,
                Tone::Ghost,
                self.settings_open,
            )
        })
        .clicked()
        {
            nav = Some(Nav::Settings);
        }
        let account_response = kit::place(ui, account, |ui| avatar_button(ui, 34.0));
        egui::Popup::menu(&account_response)
            .align(egui::RectAlign::BOTTOM_END)
            .gap(8.0)
            .width(220.0)
            .show(|ui| {
                ui.spacing_mut().item_spacing = vec2(0.0, 2.0);
                if menu_item(ui, Icon::Refresh, "Refresh library").clicked() {
                    library.send(LibraryCommand::Refresh { force: true });
                    ui.close();
                }
                if menu_item(ui, Icon::Settings, "Settings").clicked() {
                    nav = Some(Nav::Settings);
                    ui.close();
                }
                ui.add_space(4.0);
                widgets::hairline(ui);
                ui.add_space(4.0);
                if menu_item(ui, Icon::SignOut, "Sign out").clicked() {
                    session.send(SessionCommand::SignOut);
                    ui.close();
                }
            });
        nav
    }

    fn search_box(&mut self, ui: &mut Ui, rect: Rect, library: &LibraryHandle) {
        let field_id = Id::unique("library_search");
        let focused = ui.ctx().memory(|m| m.has_focus(field_id));
        let hovered = ui.rect_contains_pointer(rect);
        let t = kit::hover_t(ui, field_id.with("box"), hovered || focused);
        let radius = CornerRadius::same(21);
        ui.painter().rect_filled(
            rect,
            radius,
            kit::mix(theme::SURFACE, theme::SURFACE_HIGH, t),
        );
        let stroke = if focused {
            Stroke::new(1.0, theme::TEXT_SECONDARY.gamma_multiply(0.6))
        } else {
            Stroke::new(1.0, theme::BORDER)
        };
        ui.painter()
            .rect_stroke(rect, radius, stroke, StrokeKind::Inside);
        icons::paint(
            ui.painter(),
            icons::centered(pos2(rect.left() + 24.0, rect.center().y), 18.0),
            Icon::Search,
            theme::TEXT_SECONDARY,
        );
        let has_text = !self.search_text.is_empty();
        let inner = Rect::from_min_max(
            pos2(rect.left() + 44.0, rect.top()),
            pos2(
                rect.right() - if has_text { 40.0 } else { 16.0 },
                rect.bottom(),
            ),
        );
        let mut changed = false;
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(inner)
                .layout(Layout::left_to_right(Align::Center)),
            |ui| {
                let edit = egui::TextEdit::singleline(&mut self.search_text)
                    .id(field_id)
                    .frame(egui::Frame::NONE)
                    .margin(egui::Margin::ZERO)
                    .align(Align2::LEFT_CENTER)
                    .hint_text(
                        RichText::new(SEARCH_HINT)
                            .font(theme::font(15.0))
                            .color(theme::TEXT_SECONDARY),
                    )
                    .font(theme::font(15.0))
                    .text_color(theme::TEXT_PRIMARY)
                    .desired_width(inner.width());
                let response = ui.add(edit);
                // The hint is not an accessible name; give the field one.
                let value = self.search_text.clone();
                response.widget_info(|| {
                    let mut info =
                        egui::WidgetInfo::labeled(egui::Role::TextInput, true, SEARCH_HINT);
                    info.current_text_value = Some(value.clone());
                    info
                });
                if self.focus_search {
                    response.request_focus();
                    self.focus_search = false;
                }
                changed = response.changed();
            },
        );
        if !focused && !has_text && ui.is_rect_visible(rect) {
            let hint = "⌘K";
            ui.painter().text(
                pos2(rect.right() - 18.0, rect.center().y),
                Align2::RIGHT_CENTER,
                hint,
                theme::font(12.0),
                theme::TEXT_TERTIARY,
            );
        }
        if changed {
            self.search_changed_at = Some(ui.input(|i| i.time));
            if matches!(self.route, Route::Game(_)) {
                self.route = self.history.pop().unwrap_or_default();
                self.apply_route(library);
            }
        }
        if has_text {
            let clear = Rect::from_center_size(
                pos2(rect.right() - 22.0, rect.center().y),
                vec2(28.0, 28.0),
            );
            if kit::place(ui, clear, |ui| {
                kit::icon_button(ui, Icon::Close, 28.0, "Clear search")
            })
            .clicked()
            {
                self.clear_search(library);
            }
        }
    }

    // ------------------------------------------------------------------ sidebar

    fn sidebar(
        &mut self,
        ui: &mut Ui,
        snapshot: &LibrarySnapshot,
        compact: bool,
        artwork: &dyn ArtworkSource,
    ) -> Option<Nav> {
        let mut nav = None;
        let full = ui.max_rect();
        let pad = 12.0;
        let nav_card =
            Rect::from_min_size(full.min, vec2(full.width(), 52.0 * 3.0 + pad * 2.0 + 8.0));
        kit::card(ui, nav_card);
        ui.scope_builder(UiBuilder::new().max_rect(nav_card.shrink(pad)), |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 4.0);
            let current = match self.route {
                Route::Page(page) if !self.searching() => Some(page),
                _ => None,
            };
            for (page, icon, label) in [
                (Page::Home, Icon::Home, "Home"),
                (Page::Library, Icon::Library, "Library"),
            ] {
                if kit::nav_item(
                    ui,
                    icon,
                    label,
                    current == Some(page)
                        && (page != Page::Library || self.tab != LibraryTab::Favorites),
                    compact,
                )
                .clicked()
                {
                    if page == Page::Library {
                        self.tab = LibraryTab::All;
                    }
                    nav = Some(Nav::Go(Route::Page(page)));
                }
            }
            if kit::nav_item(
                ui,
                Icon::Heart,
                "Favourites",
                current == Some(Page::Library) && self.tab == LibraryTab::Favorites,
                compact,
            )
            .clicked()
            {
                self.tab = LibraryTab::Favorites;
                nav = Some(Nav::Go(Route::Page(Page::Library)));
            }
        });

        let user = self.user.read();
        let recent = user_data::jump_back_in(
            &user,
            snapshot.shelf_items(SHELF_MRU),
            &snapshot.all_items,
            8,
        );
        drop(user);
        let recent_card = Rect::from_min_max(pos2(full.left(), nav_card.bottom() + 12.0), full.max);
        kit::card(ui, recent_card);
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(recent_card.shrink2(vec2(if compact { 10.0 } else { 18.0 }, 16.0))),
            |ui| {
                ui.set_clip_rect(recent_card);
                ui.spacing_mut().item_spacing = vec2(0.0, 4.0);
                if !compact {
                    let (head, heading) =
                        ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::hover());
                    heading.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::Role::Heading, true, "Last played")
                    });
                    icons::paint(
                        ui.painter(),
                        icons::centered(pos2(head.left() + 10.0, head.center().y), 20.0),
                        Icon::Clock,
                        theme::TEXT_SECONDARY,
                    );
                    ui.painter().text(
                        pos2(head.left() + 32.0, head.center().y),
                        Align2::LEFT_CENTER,
                        "Last played",
                        font_w(16.0, Weight::Semibold),
                        theme::TEXT_PRIMARY,
                    );
                    ui.add_space(4.0);
                }
                if recent.is_empty() {
                    if !compact {
                        widgets::paint_wrapped(
                            ui,
                            ui.cursor().min + vec2(2.0, 4.0),
                            if snapshot.is_loading {
                                "Loading your games…"
                            } else {
                                "Games you play show up here."
                            },
                            theme::font(13.5),
                            theme::TEXT_SECONDARY,
                            ui.available_width(),
                            3,
                        );
                    }
                    return;
                }
                let rows = ((ui.available_height() - 64.0) / 68.0).floor().max(1.0) as usize;
                for card in recent.iter().take(rows.min(8)) {
                    let response = if compact {
                        compact_thumb(ui, card, artwork)
                    } else {
                        kit::game_row(ui, card, artwork)
                    };
                    if response.clicked() {
                        nav = Some(Nav::Go(Route::Game(Box::new((*card).clone()))));
                    }
                }
                if !compact {
                    ui.add_space(6.0);
                    if see_all_row(ui, "View play history").clicked() {
                        self.tab = LibraryTab::Recent;
                        nav = Some(Nav::Go(Route::Page(Page::Library)));
                    }
                }
            },
        );
        nav
    }

    // ------------------------------------------------------------------ content

    fn content(
        &mut self,
        ui: &mut Ui,
        snapshot: &LibrarySnapshot,
        library: &LibraryHandle,
        artwork: &dyn ArtworkSource,
    ) -> Option<Nav> {
        if snapshot.is_empty() || snapshot.authentication_required {
            return self.empty_state(ui, snapshot, library);
        }
        if self.showing_results() {
            let title = format!("Results for “{}”", self.search_text.trim());
            return self.grid_page(ui, snapshot, library, artwork, &title, false, GridList::All);
        }
        match self.route.clone() {
            Route::Page(Page::Home) => self.home(ui, snapshot, artwork),
            Route::Page(Page::Library) => {
                let list = match self.tab {
                    LibraryTab::Recent => GridList::Recent,
                    LibraryTab::All => GridList::All,
                    LibraryTab::Favorites => GridList::Favorites,
                };
                self.grid_page(ui, snapshot, library, artwork, "", true, list)
            }
            Route::Shelf(_, title) => {
                self.grid_page(ui, snapshot, library, artwork, &title, false, GridList::All)
            }
            Route::Picks => self.grid_page(
                ui,
                snapshot,
                library,
                artwork,
                PICKS_TITLE,
                false,
                GridList::Picks,
            ),
            Route::Game(card) => {
                let user = self.user.clone();
                let (nav, scroll) = game_page(ui, &card, &user, artwork);
                self.game_scroll = scroll;
                nav
            }
        }
    }

    fn empty_state(
        &mut self,
        ui: &mut Ui,
        snapshot: &LibrarySnapshot,
        library: &LibraryHandle,
    ) -> Option<Nav> {
        let mut action = None;
        let full = ui.max_rect();
        let rect = Rect::from_center_size(
            full.center() - vec2(0.0, 40.0),
            vec2(420.0_f32.min(full.width()), 220.0),
        );
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(rect)
                .layout(Layout::top_down(Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing = vec2(0.0, 14.0);
                if snapshot.is_loading {
                    ui.add(egui::Spinner::new().size(34.0).color(theme::ACCENT));
                } else {
                    let (r, _) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::hover());
                    icons::paint(ui.painter(), r, Icon::Cloud, theme::TEXT_SECONDARY);
                }
                let (title, subtitle) = if snapshot.is_loading {
                    (
                        "Getting your games ready",
                        "Loading your cloud library…".to_owned(),
                    )
                } else if snapshot.authentication_required {
                    (
                        "Please sign in again",
                        "Your session expired or was rejected by Xbox.".to_owned(),
                    )
                } else if snapshot.last_error.is_empty() {
                    (
                        "Your library is empty",
                        "Refresh to load your cloud games.".to_owned(),
                    )
                } else {
                    ("We couldn't load your library", snapshot.last_error.clone())
                };
                ui.label(RichText::new(title).font(font_w(24.0, Weight::Semibold)));
                ui.label(
                    RichText::new(subtitle)
                        .font(theme::font(15.0))
                        .color(theme::TEXT_SECONDARY),
                );
                if !snapshot.is_loading
                    && Button::new(if snapshot.authentication_required {
                        "Sign in again"
                    } else {
                        "Refresh library"
                    })
                    .icon(Icon::Refresh)
                    .primary()
                    .show(ui)
                    .clicked()
                {
                    if snapshot.authentication_required {
                        action = Some(Nav::SignIn);
                    } else {
                        library.send(LibraryCommand::Refresh { force: true });
                    }
                }
            },
        );
        action
    }

    fn home(
        &mut self,
        ui: &mut Ui,
        snapshot: &LibrarySnapshot,
        artwork: &dyn ArtworkSource,
    ) -> Option<Nav> {
        let mut nav = None;
        let full = ui.max_rect();
        let _ = full;
        let featured = featured_cards(snapshot, &self.user.read());

        egui::ScrollArea::vertical()
            .id_salt("home_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let margin = 28.0;
                let width = ui.available_width() - margin * 2.0;
                let left = ui.max_rect().left() + margin;
                let section = |ui: &mut Ui, h: f32| {
                    let (r, _) =
                        ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
                    Rect::from_min_size(pos2(left, r.top()), vec2(width, h))
                };
                ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                ui.add_space(if featured.is_empty() { 40.0 } else { 120.0 });

                // Jump back in.
                let user = self.user.clone();
                let (count, tile) = jump_tiles(width);
                let mru = user_data::jump_back_in(
                    &user.read(),
                    snapshot.shelf_items(SHELF_MRU),
                    &snapshot.all_items,
                    count,
                );
                if !mru.is_empty() {
                    let header = section(ui, 52.0);
                    kit::place(ui, header, |ui| {
                        kit::section_header(ui, "Jump back in", None)
                    });
                    let row = section(ui, tile + 4.0);
                    kit::place(ui, row, |ui| {
                        ui.spacing_mut().item_spacing = vec2(16.0, 0.0);
                        for card in &mru {
                            let favorite = user.read().is_favorite(&card.title_id);
                            let click =
                                kit::game_tile(ui, card, tile, false, Some(favorite), artwork);
                            if click.favorite {
                                user.write().toggle_favorite(&card.title_id);
                            } else if click.play {
                                nav = Some(Nav::Play(Box::new((*card).clone())));
                            } else if click.open {
                                nav = Some(Nav::Go(Route::Game(Box::new((*card).clone()))));
                            }
                        }
                    });
                    ui.add_space(28.0);
                }

                // Featured carousel.
                if !featured.is_empty() {
                    let per_page = self.per_page(width);
                    let pages = featured.len().div_ceil(per_page);
                    self.carousel = self.carousel.min(pages - 1);
                    let height = (width / per_page as f32 * 0.52).clamp(240.0, 420.0);
                    let strip = section(ui, height);
                    let gap = 16.0;
                    let card_w = (strip.width() - gap * (per_page as f32 - 1.0)) / per_page as f32;
                    for i in 0..per_page {
                        let Some(card) = featured.get(self.carousel * per_page + i) else {
                            break;
                        };
                        let r = Rect::from_min_size(
                            pos2(strip.left() + i as f32 * (card_w + gap), strip.top()),
                            vec2(card_w, height),
                        );
                        let subtitle = if card.short_description.is_empty() {
                            card.publisher_name.clone()
                        } else {
                            first_sentence(&card.short_description)
                        };
                        if kit::hero_card(ui, r, card, &subtitle, artwork).clicked() {
                            nav = Some(Nav::Go(Route::Game(Box::new((*card).clone()))));
                        }
                    }
                    if pages > 1 {
                        let arrow = 44.0;
                        let left_btn = Rect::from_center_size(
                            pos2(strip.left() + 30.0, strip.center().y),
                            vec2(arrow, arrow),
                        );
                        let right_btn = Rect::from_center_size(
                            pos2(strip.right() - 30.0, strip.center().y),
                            vec2(arrow, arrow),
                        );
                        if self.carousel > 0
                            && kit::place(ui, left_btn, |ui| {
                                kit::icon_button_toned(
                                    ui,
                                    Icon::ChevronLeft,
                                    arrow,
                                    "Previous",
                                    Tone::Glass,
                                    false,
                                )
                            })
                            .clicked()
                        {
                            self.carousel -= 1;
                        }
                        if self.carousel + 1 < pages
                            && kit::place(ui, right_btn, |ui| {
                                kit::icon_button_toned(
                                    ui,
                                    Icon::ChevronRight,
                                    arrow,
                                    "Next",
                                    Tone::Glass,
                                    false,
                                )
                            })
                            .clicked()
                        {
                            self.carousel += 1;
                        }
                        ui.add_space(14.0);
                        let dots = section(ui, 20.0);
                        kit::place(ui, dots, |ui| {
                            if let Some(i) = kit::page_dots(
                                ui,
                                Id::unique("carousel_dots"),
                                pages,
                                self.carousel,
                            ) {
                                self.carousel = i;
                            }
                        });
                    }
                    ui.add_space(30.0);
                }

                // Local shelves: favourites, then picks built from what you play.
                let local = {
                    let data = user.read();
                    let favorites: Vec<LibraryCard> =
                        user_data::favorite_cards(&data, &snapshot.all_items)
                            .into_iter()
                            .take(HOME_SHELF_LEN)
                            .cloned()
                            .collect();
                    let picks: Vec<LibraryCard> = self
                        .recommended_cards(snapshot, &data, HOME_SHELF_LEN)
                        .into_iter()
                        .cloned()
                        .collect();
                    [
                        (
                            Shelf {
                                id: "local:favorites".into(),
                                title: "Your favourites".into(),
                                items: favorites,
                            },
                            Route::Page(Page::Library),
                        ),
                        (
                            Shelf {
                                id: "local:picks".into(),
                                title: PICKS_TITLE.into(),
                                items: picks,
                            },
                            Route::Picks,
                        ),
                    ]
                };
                for (shelf, target) in &local {
                    if shelf.items.is_empty() {
                        continue;
                    }
                    let header = section(ui, 52.0);
                    let see_all = kit::place(ui, header, |ui| {
                        kit::section_header(ui, &shelf.title, Some("See all"))
                    });
                    if see_all {
                        if shelf.id == "local:favorites" {
                            self.tab = LibraryTab::Favorites;
                        }
                        nav = Some(Nav::Go(target.clone()));
                    }
                    match self.shelf_row(ui, shelf, left, width, artwork) {
                        Some((card, true)) => nav = Some(Nav::Play(Box::new(card))),
                        Some((card, false)) => nav = Some(Nav::Go(Route::Game(Box::new(card)))),
                        None => {}
                    }
                    ui.add_space(28.0);
                }

                // Shelves.
                for shelf in &snapshot.shelves {
                    if shelf.id == SHELF_MRU || shelf.items.is_empty() {
                        continue;
                    }
                    let header = section(ui, 52.0);
                    let see_all = kit::place(ui, header, |ui| {
                        kit::section_header(ui, &shelf.title, Some("See all"))
                    });
                    if see_all {
                        nav = Some(Nav::Go(Route::Shelf(shelf.id.clone(), shelf.title.clone())));
                    }
                    match self.shelf_row(ui, shelf, left, width, artwork) {
                        Some((card, true)) => nav = Some(Nav::Play(Box::new(card))),
                        Some((card, false)) => nav = Some(Nav::Go(Route::Game(Box::new(card)))),
                        None => {}
                    }
                    ui.add_space(28.0);
                }
                ui.add_space(24.0);
            });
        nav
    }

    fn per_page(&self, width: f32) -> usize {
        if width >= 900.0 { 2 } else { 1 }
    }

    /// One horizontally scrolling shelf with edge arrows. Returns a clicked card.
    fn shelf_row(
        &mut self,
        ui: &mut Ui,
        shelf: &Shelf,
        left: f32,
        width: f32,
        artwork: &dyn ArtworkSource,
    ) -> Option<(LibraryCard, bool)> {
        let tile = theme::TILE_WIDTH;
        let height = tile + 54.0;
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
        let row = Rect::from_min_size(pos2(left - 6.0, row.top()), vec2(width + 12.0, height));
        if !ui.is_rect_visible(row) {
            return None;
        }
        let mut clicked = None;
        let user = self.user.clone();
        let id = Id::unique(("shelf_row", &shelf.id));
        let pending = self.shelf_scroll.remove(&shelf.id);
        let output = kit::place_with(ui, row, Layout::top_down(Align::Min), |ui| {
            let mut area = egui::ScrollArea::horizontal()
                .id_salt(id)
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .auto_shrink([false, true]);
            if let Some(x) = pending {
                area = area.horizontal_scroll_offset(x);
            }
            area.show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = vec2(16.0, 0.0);
                    ui.add_space(6.0);
                    for card in &shelf.items {
                        let favorite = user.read().is_favorite(&card.title_id);
                        let click = kit::game_tile(ui, card, tile, true, Some(favorite), artwork);
                        if click.favorite {
                            user.write().toggle_favorite(&card.title_id);
                        } else if click.play {
                            clicked = Some((card.clone(), true));
                        } else if click.open {
                            clicked = Some((card.clone(), false));
                        }
                    }
                    ui.add_space(6.0);
                });
            })
        });
        let offset = output.state.clamped_offset().x;
        let max = (output.content_size.x - output.inner_rect.width()).max(0.0);
        let hovered = ui.rect_contains_pointer(row);
        if hovered && max > 0.0 {
            let arrow = 44.0;
            let cy = row.top() + tile / 2.0;
            let step = width * 0.8;
            if offset > 1.0 {
                let r = Rect::from_center_size(pos2(row.left() + 26.0, cy), vec2(arrow, arrow));
                if kit::place(ui, r, |ui| {
                    kit::icon_button_toned(
                        ui,
                        Icon::ChevronLeft,
                        arrow,
                        "Scroll left",
                        Tone::Glass,
                        false,
                    )
                })
                .clicked()
                {
                    self.shelf_scroll
                        .insert(shelf.id.clone(), (offset - step).max(0.0));
                }
            }
            if offset < max - 1.0 {
                let r = Rect::from_center_size(pos2(row.right() - 26.0, cy), vec2(arrow, arrow));
                if kit::place(ui, r, |ui| {
                    kit::icon_button_toned(
                        ui,
                        Icon::ChevronRight,
                        arrow,
                        "Scroll right",
                        Tone::Glass,
                        false,
                    )
                })
                .clicked()
                {
                    self.shelf_scroll
                        .insert(shelf.id.clone(), (offset + step).min(max));
                }
            }
        }
        clicked
    }

    // ------------------------------------------------------------------ grid pages

    #[allow(clippy::too_many_arguments)]
    fn grid_page(
        &mut self,
        ui: &mut Ui,
        snapshot: &LibrarySnapshot,
        library: &LibraryHandle,
        artwork: &dyn ArtworkSource,
        title: &str,
        library_tabs: bool,
        list: GridList,
    ) -> Option<Nav> {
        let mut nav = None;
        let items = self.grid_items(snapshot, list);
        let user = self.user.clone();
        let full = ui.max_rect();
        let inner = Rect::from_min_max(
            pos2(full.left() + 28.0, full.top() + 20.0),
            pos2(full.right() - 28.0, full.bottom()),
        );
        ui.scope_builder(UiBuilder::new().max_rect(inner), |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            if library_tabs {
                let tabs = ["Recently played", "My games", "Favourites"];
                let selected = match self.tab {
                    LibraryTab::Recent => 0,
                    LibraryTab::All => 1,
                    LibraryTab::Favorites => 2,
                };
                if let Some(i) = kit::segmented(ui, Id::unique("library_tabs"), &tabs, selected) {
                    let tab = match i {
                        0 => LibraryTab::Recent,
                        1 => LibraryTab::All,
                        _ => LibraryTab::Favorites,
                    };
                    self.set_tab(tab, library);
                }
            } else {
                let (r, heading) =
                    ui.allocate_exact_size(vec2(ui.available_width(), 44.0), Sense::hover());
                heading.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Heading, true, title));
                ui.painter().text(
                    pos2(r.left(), r.center().y),
                    Align2::LEFT_CENTER,
                    title,
                    font_w(28.0, Weight::Bold),
                    theme::TEXT_PRIMARY,
                );
            }
            ui.add_space(22.0);
            self.toolbar(ui, snapshot, library, items.len());
            ui.add_space(22.0);

            if items.is_empty() {
                let rect = ui.available_rect_before_wrap();
                let center = pos2(rect.center().x, rect.top() + 120.0);
                let filtered = self.searching() || self.filter_count() > 0;
                let (headline, hint) = match list {
                    _ if snapshot.is_loading => ("Loading…", ""),
                    GridList::Favorites if !filtered => (
                        "No favourites yet",
                        "Tap the heart on any game to keep it here.",
                    ),
                    GridList::Recent if !filtered => {
                        ("Nothing played yet", "Games you play show up here.")
                    }
                    GridList::Picks if !filtered => (
                        "No picks yet",
                        "Play or favourite a few games and we'll suggest more like them.",
                    ),
                    _ => (
                        "No games match",
                        "Try another search or clear your filters.",
                    ),
                };
                let heading = ui.interact(
                    Rect::from_center_size(center, vec2(360.0, 30.0)),
                    Id::unique("grid_empty"),
                    Sense::hover(),
                );
                heading
                    .widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, headline));
                ui.painter().text(
                    center,
                    Align2::CENTER_CENTER,
                    headline,
                    font_w(20.0, Weight::Semibold),
                    theme::TEXT_PRIMARY,
                );
                ui.painter().text(
                    center + vec2(0.0, 30.0),
                    Align2::CENTER_CENTER,
                    hint,
                    theme::font(14.0),
                    theme::TEXT_SECONDARY,
                );
                return;
            }

            let gap = 18.0;
            let width = ui.available_width();
            let min_tile = self.density.min_tile();
            let columns = (((width + gap) / (min_tile + gap)).floor() as usize).max(1);
            let tile = ((width - gap * (columns as f32 - 1.0)) / columns as f32).floor();
            let rows = items.len().div_ceil(columns);
            egui::ScrollArea::vertical()
                .id_salt(("grid", title, library_tabs, list as u8))
                .auto_shrink([false, false])
                .show_rows(ui, tile + gap, rows, |ui, range| {
                    for row in range {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing = vec2(gap, 0.0);
                            for card in items.iter().skip(row * columns).take(columns) {
                                let favorite = user.read().is_favorite(&card.title_id);
                                let click =
                                    kit::game_tile(ui, card, tile, false, Some(favorite), artwork);
                                if click.favorite {
                                    user.write().toggle_favorite(&card.title_id);
                                } else if click.play {
                                    nav = Some(Nav::Play(Box::new((*card).clone())));
                                } else if click.open {
                                    nav = Some(Nav::Go(Route::Game(Box::new((*card).clone()))));
                                }
                            }
                        });
                        ui.add_space(gap);
                    }
                });
        });
        nav
    }

    /// Filter, sort, count and density controls above a grid.
    fn toolbar(
        &mut self,
        ui: &mut Ui,
        snapshot: &LibrarySnapshot,
        library: &LibraryHandle,
        count: usize,
    ) {
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::hover());
        ui.scope_builder(
            UiBuilder::new()
                .max_rect(row)
                .layout(Layout::left_to_right(Align::Center)),
            |ui| {
                ui.spacing_mut().item_spacing = vec2(10.0, 0.0);
                let active = self.filter_count();
                let label = if active > 0 {
                    format!("Filters · {active}")
                } else {
                    "Filters".to_owned()
                };
                let filters = kit::pill(ui, Some(Icon::Filter), &label, None, active > 0)
                    .on_hover_text("Filters");
                egui::Popup::from_toggle_button_response(&filters)
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .align(egui::RectAlign::BOTTOM_START)
                    .gap(8.0)
                    .width(280.0)
                    .show(|ui| self.filter_menu(ui, snapshot, library));

                let sort_label = if self.filter.sort == SORT_RECENT_FIRST {
                    "Recently played"
                } else {
                    "Alphabetical"
                };
                let sort = kit::pill(
                    ui,
                    Some(Icon::Sort),
                    sort_label,
                    Some(Icon::ChevronDown),
                    false,
                );
                egui::Popup::menu(&sort)
                    .align(egui::RectAlign::BOTTOM_START)
                    .gap(8.0)
                    .width(220.0)
                    .show(|ui| {
                        ui.spacing_mut().item_spacing = vec2(0.0, 2.0);
                        for (value, text) in [
                            (SORT_ALPHABETICAL, "Alphabetical"),
                            (SORT_RECENT_FIRST, "Recently played"),
                        ] {
                            let selected = self.filter.sort == value;
                            if check_item(ui, text, selected).clicked() {
                                let mut f = self.filter.clone();
                                f.sort = value.to_owned();
                                self.set_filter(f, library);
                                ui.close();
                            }
                        }
                    });

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (icon, label, next) = match self.density {
                        Density::Large => (Icon::Grid, "Compact grid", Density::Compact),
                        Density::Compact => (Icon::Grid, "Large grid", Density::Large),
                    };
                    if kit::icon_button_toned(
                        ui,
                        icon,
                        40.0,
                        label,
                        Tone::Secondary,
                        self.density == Density::Compact,
                    )
                    .clicked()
                    {
                        self.density = next;
                    }
                    ui.label(
                        RichText::new(format!("{count} games"))
                            .font(theme::font(14.0))
                            .color(theme::TEXT_SECONDARY),
                    );
                });
            },
        );
    }

    fn filter_count(&self) -> usize {
        usize::from(self.favorites_only)
            + usize::from(!self.filter.genre.is_empty())
            + usize::from(self.filter.touch_only)
            + usize::from(self.filter.min_players > 0)
            + usize::from(!self.filter.feature.is_empty())
    }

    fn filter_menu(&mut self, ui: &mut Ui, snapshot: &LibrarySnapshot, library: &LibraryHandle) {
        let mut f = self.filter.clone();
        ui.spacing_mut().item_spacing = vec2(8.0, 10.0);
        ui.add_space(4.0);
        menu_caption(ui, "Collection");
        ui.checkbox(&mut self.favorites_only, "Favourites only");
        menu_caption(ui, "Genre");
        let genres: std::collections::BTreeSet<&str> = snapshot
            .all_items
            .iter()
            .flat_map(|card| card.categories.iter().map(String::as_str))
            .filter(|genre| !genre.is_empty())
            .collect();
        egui::ComboBox::from_id_salt("filter_genre")
            .selected_text(if f.genre.is_empty() {
                "Any genre"
            } else {
                &f.genre
            })
            .width(ui.available_width())
            .height(260.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut f.genre, String::new(), "Any genre");
                for genre in genres {
                    ui.selectable_value(&mut f.genre, genre.to_owned(), genre);
                }
            });
        menu_caption(ui, "Input");
        ui.horizontal(|ui| {
            ui.label(RichText::new("Touch controls").font(theme::font(14.5)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                kit::toggle(ui, &mut f.touch_only, "Touch controls");
            });
        });
        menu_caption(ui, "Players");
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
            for (n, text) in [(0u32, "Any"), (2, "2+"), (4, "4+"), (8, "8+")] {
                if chip(ui, text, f.min_players == n).clicked() {
                    f.min_players = n;
                }
            }
        });
        menu_caption(ui, "Feature");
        let selected = if f.feature.is_empty() {
            "Any feature".to_owned()
        } else {
            f.feature.clone()
        };
        egui::ComboBox::from_id_salt("filter_feature")
            .selected_text(selected)
            .width(ui.available_width())
            .height(320.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut f.feature, String::new(), "Any feature");
                for group in &snapshot.feature_groups {
                    ui.separator();
                    ui.label(
                        RichText::new(group.title.as_str())
                            .font(theme::font(11.5))
                            .color(theme::TEXT_TERTIARY),
                    );
                    for feature in &group.features {
                        ui.selectable_value(&mut f.feature, feature.clone(), feature.as_str());
                    }
                }
            });
        if self.filter_count() > 0
            || f.touch_only
            || f.min_players > 0
            || !f.feature.is_empty()
            || !f.genre.is_empty()
        {
            ui.add_space(4.0);
            if Button::new("Clear filters")
                .height(34.0)
                .min_width(ui.available_width())
                .show(ui)
                .clicked()
            {
                self.favorites_only = false;
                f.genre.clear();
                f.touch_only = false;
                f.min_players = 0;
                f.feature.clear();
            }
        }
        ui.add_space(2.0);
        self.set_filter(f, library);
    }
}

// ----------------------------------------------------------------------------- pages

/// Up to ten games with hero art for the featured carousel: recently added first,
/// then the rest of the library, skipping the "Jump back in" games (already shown).
pub fn featured_cards<'a>(snapshot: &'a LibrarySnapshot, user: &UserData) -> Vec<&'a LibraryCard> {
    let mut mru: Vec<&str> = snapshot
        .shelf_items(SHELF_MRU)
        .iter()
        .map(|c| c.title_id.as_str())
        .collect();
    let played: Vec<&str> = user.history.iter().map(|r| r.title_id.as_str()).collect();
    mru.extend(
        snapshot
            .all_items
            .iter()
            .filter(|c| played.contains(&c.title_id.as_str()))
            .map(|c| c.title_id.as_str()),
    );
    let mut out: Vec<&LibraryCard> = Vec::new();
    let candidates = snapshot
        .shelf_items(SHELF_RECENTLY_ADDED)
        .iter()
        .chain(snapshot.all_items.iter());
    for card in candidates {
        if out.len() >= 10 {
            break;
        }
        if card.hero_image_url.is_empty()
            || card.name.is_empty()
            || card.name == card.product_id
            || mru.contains(&card.title_id.as_str())
            || out.iter().any(|c| c.title_id == card.title_id)
        {
            continue;
        }
        out.push(card);
    }
    out
}

/// How many "Jump back in" tiles fit in `width`, and their edge: about 210 pt each,
/// filling the row exactly.
fn jump_tiles(width: f32) -> (usize, f32) {
    let gap = 16.0;
    let count = (((width + gap) / (210.0 + gap)).floor() as usize).max(1);
    let tile = (width - gap * (count as f32 - 1.0)) / count as f32;
    (count, tile.clamp(140.0, 280.0))
}

/// "Last played 3 days ago · 2 h 15 min" for the game page.
fn play_summary(record: &user_data::PlayRecord, now: u64) -> String {
    let ago = now.saturating_sub(record.last_played);
    let when = match ago {
        0..3_600 => "Last played just now".to_string(),
        3_600..86_400 => {
            let h = ago / 3_600;
            format!("Last played {h} hour{} ago", if h == 1 { "" } else { "s" })
        }
        86_400..172_800 => "Last played yesterday".to_string(),
        _ => format!("Last played {} days ago", ago / 86_400),
    };
    let minutes = record.seconds_played / 60;
    match minutes {
        0 => when,
        1..60 => format!("{when} · {minutes} min"),
        _ => format!("{when} · {} h {} min", minutes / 60, minutes % 60),
    }
}

fn first_sentence(text: &str) -> String {
    let t = text.trim();
    match t.find(". ") {
        Some(i) if i < 140 => t[..=i].to_owned(),
        _ => t.chars().take(140).collect(),
    }
}

/// Hero height of the game page for a content area `width` wide.
fn game_hero_height(width: f32) -> f32 {
    (width * 0.40).clamp(320.0, 500.0)
}

/// Paints the window-wide backdrop: hero art fading into the canvas (the Xbox
/// "ambient" header), or the lime glow. Drawn before the sidebar and top bar so both
/// float on top of it.
fn paint_window_backdrop(ui: &Ui, full: Rect, backdrop: Backdrop<'_>, artwork: &dyn ArtworkSource) {
    let bg = theme::BACKGROUND;
    let glow = |ui: &Ui| {
        widgets::radial_glow(
            ui,
            pos2(full.center().x + full.width() * 0.08, full.top() - 60.0),
            vec2(full.width() * 0.62, 420.0),
            theme::ACCENT,
            0.16,
        );
    };
    match backdrop {
        Backdrop::Glow => glow(ui),
        Backdrop::Art {
            card,
            height,
            opacity,
        } => {
            let area = Rect::from_min_size(full.min, vec2(full.width(), height.min(full.height())));
            let drew = opacity > 0.01
                && widgets::remote_image_opacity(
                    ui,
                    artwork,
                    &kit::hero_urls(card),
                    ArtworkKind::Hero,
                    area,
                    0.0,
                    opacity,
                );
            if !drew {
                glow(ui);
                return;
            }
            widgets::vertical_gradient(
                ui,
                Rect::from_min_max(
                    pos2(area.left(), area.top() + area.height() * 0.30),
                    area.max,
                ),
                Color32::TRANSPARENT,
                bg,
            );
            widgets::horizontal_gradient(
                ui,
                area,
                &[
                    (0.0, bg.gamma_multiply(0.92)),
                    (0.30, bg.gamma_multiply(0.35)),
                    (0.70, Color32::TRANSPARENT),
                    (1.0, bg.gamma_multiply(0.25)),
                ],
            );
            // Keep the top bar legible over bright art.
            widgets::vertical_gradient(
                ui,
                Rect::from_min_size(area.min, vec2(area.width(), theme::HEADER_HEIGHT + 60.0)),
                bg.gamma_multiply(0.88),
                Color32::TRANSPARENT,
            );
        }
    }
}

/// The game detail page: backdrop, box art, title, Play, description, details.
fn game_page(
    ui: &mut Ui,
    card: &LibraryCard,
    user: &UserDataHandle,
    artwork: &dyn ArtworkSource,
) -> (Option<Nav>, f32) {
    let mut nav = None;
    let full = ui.max_rect();
    let output = egui::ScrollArea::vertical()
        .id_salt(("game_page", &card.title_id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            let width = ui.available_width();
            let hero_h = game_hero_height(full.width());
            let (hero, _) = ui.allocate_exact_size(vec2(width, hero_h), Sense::hover());
            let hero = Rect::from_min_max(
                pos2(full.left(), hero.top()),
                pos2(full.right(), hero.bottom()),
            );

            // Box art + title block, overlapping the bottom of the hero.
            let margin = 36.0;
            let art = 200.0_f32.min(width * 0.22);
            let art_rect = Rect::from_min_size(
                pos2(hero.left() + margin, hero.bottom() - art * 0.72),
                Vec2::splat(art),
            );
            ui.painter().rect_filled(
                art_rect.translate(vec2(0.0, 8.0)).expand(2.0),
                CornerRadius::same(14),
                Color32::from_black_alpha(90),
            );
            ui.painter()
                .rect_filled(art_rect, CornerRadius::same(12), theme::SURFACE_HIGH);
            widgets::remote_image(
                ui,
                artwork,
                &kit::box_art_urls(card),
                ArtworkKind::Tile,
                art_rect,
                12.0,
            );

            let text_left = art_rect.right() + 28.0;
            let text_w = (hero.right() - margin - text_left).max(200.0);
            let title_font = font_w(if width > 900.0 { 40.0 } else { 30.0 }, Weight::Bold);
            let title_galley =
                widgets::layout_font(ui, &card.name, title_font, theme::TEXT_PRIMARY, text_w, 2);
            let meta = meta_line(card);
            let block_h = title_galley.size().y + 8.0 + 20.0 + 18.0 + 50.0;
            let mut y = art_rect.bottom() - block_h;
            ui.painter().galley(
                pos2(text_left, y),
                title_galley.clone(),
                theme::TEXT_PRIMARY,
            );
            y += title_galley.size().y + 8.0;
            widgets::paint_elided(
                ui,
                pos2(text_left, y),
                &meta,
                theme::font(15.0),
                theme::TEXT_SECONDARY,
                text_w,
            );
            y += 20.0 + 18.0;
            let play_rect = Rect::from_min_size(pos2(text_left, y), vec2(180.0, 50.0));
            if kit::place(ui, play_rect, |ui| {
                Button::new("Play")
                    .icon(Icon::Play)
                    .primary()
                    .height(50.0)
                    .min_width(180.0)
                    .show(ui)
            })
            .clicked()
            {
                nav = Some(Nav::Play(Box::new(card.clone())));
            }
            let favorite = user.read().is_favorite(&card.title_id);
            let heart_rect =
                Rect::from_min_size(pos2(play_rect.right() + 12.0, y), vec2(50.0, 50.0));
            if kit::place(ui, heart_rect, |ui| {
                kit::icon_button_tinted(
                    ui,
                    if favorite {
                        Icon::HeartFilled
                    } else {
                        Icon::Heart
                    },
                    50.0,
                    if favorite {
                        "Remove from favourites"
                    } else {
                        "Add to favourites"
                    },
                    Tone::Secondary,
                    favorite,
                    favorite.then_some(kit::FAVORITE),
                )
            })
            .clicked()
            {
                user.write().toggle_favorite(&card.title_id);
            }
            let summary = user
                .read()
                .record(&card.title_id)
                .map(|r| play_summary(r, user_data::now_secs()));
            let (icon, line) = match &summary {
                Some(text) => (Icon::Clock, text.as_str()),
                None => (Icon::Cloud, "Play with Cloud Gaming"),
            };
            let cloud =
                Rect::from_min_size(pos2(heart_rect.right() + 16.0, y + 13.0), vec2(320.0, 24.0));
            icons::paint(
                ui.painter(),
                icons::centered(pos2(cloud.left() + 10.0, cloud.center().y), 20.0),
                icon,
                theme::TEXT_SECONDARY,
            );
            ui.painter().text(
                pos2(cloud.left() + 28.0, cloud.center().y),
                Align2::LEFT_CENTER,
                line,
                theme::font(14.0),
                theme::TEXT_SECONDARY,
            );

            // Body below the hero.
            let body_top = art_rect.bottom() + 40.0;
            let consumed = body_top - hero.bottom();
            ui.add_space(consumed.max(0.0));
            let body_w = width - margin * 2.0;
            let left = full.left() + margin;
            let column_gap = 40.0;
            let two_col = body_w > 860.0;
            let main_w = if two_col { body_w * 0.62 } else { body_w };
            let side_w = if two_col {
                body_w - main_w - column_gap
            } else {
                body_w
            };

            let top_y = ui.cursor().min.y;
            let mut main_y = top_y;
            if !card.short_description.is_empty() {
                let label = widgets::paint_elided(
                    ui,
                    pos2(left, main_y),
                    "About",
                    font_w(20.0, Weight::Semibold),
                    theme::TEXT_PRIMARY,
                    main_w,
                );
                main_y = label.bottom() + 12.0;
                let r = widgets::paint_wrapped(
                    ui,
                    pos2(left, main_y),
                    &card.short_description,
                    theme::font(15.5),
                    theme::HERO_BODY_TEXT,
                    main_w,
                    8,
                );
                main_y = r.bottom() + 28.0;
            }
            let chips: Vec<&str> = card
                .attributes
                .iter()
                .map(|a| {
                    if a.localized_name.is_empty() {
                        a.name.as_str()
                    } else {
                        a.localized_name.as_str()
                    }
                })
                .filter(|s| !s.is_empty())
                .collect();
            if !chips.is_empty() {
                let label = widgets::paint_elided(
                    ui,
                    pos2(left, main_y),
                    "Features",
                    font_w(20.0, Weight::Semibold),
                    theme::TEXT_PRIMARY,
                    main_w,
                );
                main_y = label.bottom() + 12.0;
                let mut x = left;
                let font = font_w(13.0, Weight::Medium);
                for chip_text in chips {
                    let w = ui
                        .painter()
                        .layout_no_wrap(chip_text.to_owned(), font.clone(), Color32::WHITE)
                        .size()
                        .x
                        + 24.0;
                    if x + w > left + main_w {
                        x = left;
                        main_y += 38.0;
                    }
                    let r = Rect::from_min_size(pos2(x, main_y), vec2(w, 30.0));
                    ui.painter()
                        .rect_filled(r, CornerRadius::same(15), theme::SURFACE_HIGH);
                    ui.painter().text(
                        r.center(),
                        Align2::CENTER_CENTER,
                        chip_text,
                        font.clone(),
                        theme::TEXT_PRIMARY,
                    );
                    x += w + 8.0;
                }
                main_y += 30.0 + 28.0;
            }

            // Details card.
            let (side_left, mut side_y) = if two_col {
                (left + main_w + column_gap, top_y)
            } else {
                (left, main_y)
            };
            let rows = detail_rows(card);
            if !rows.is_empty() {
                let card_h = 24.0 + rows.len() as f32 * 54.0 + 8.0;
                let r = Rect::from_min_size(pos2(side_left, side_y), vec2(side_w, card_h));
                kit::card(ui, r);
                let mut y = r.top() + 20.0;
                for (k, v) in rows {
                    ui.painter().text(
                        pos2(r.left() + 22.0, y),
                        Align2::LEFT_TOP,
                        k,
                        theme::font(12.5),
                        theme::TEXT_SECONDARY,
                    );
                    widgets::paint_elided(
                        ui,
                        pos2(r.left() + 22.0, y + 18.0),
                        &v,
                        font_w(15.0, Weight::Medium),
                        theme::TEXT_PRIMARY,
                        side_w - 44.0,
                    );
                    y += 54.0;
                }
                side_y = r.bottom() + 28.0;
            }

            let mut y = if two_col {
                main_y.max(side_y)
            } else {
                side_y.max(main_y)
            };
            // Screenshots.
            let shots: Vec<String> = card.gallery_image_urls.iter().take(8).cloned().collect();
            if !shots.is_empty() {
                let label = widgets::paint_elided(
                    ui,
                    pos2(left, y),
                    "Screenshots",
                    font_w(20.0, Weight::Semibold),
                    theme::TEXT_PRIMARY,
                    body_w,
                );
                y = label.bottom() + 14.0;
                let shot_w = ((body_w - 16.0 * 2.0) / 3.0).max(200.0);
                let shot_h = shot_w * 9.0 / 16.0;
                let mut x = left;
                for (i, url) in shots.iter().enumerate() {
                    if i > 0 && i % 3 == 0 {
                        x = left;
                        y += shot_h + 16.0;
                    }
                    let r = Rect::from_min_size(pos2(x, y), vec2(shot_w, shot_h));
                    ui.painter()
                        .rect_filled(r, CornerRadius::same(10), theme::SURFACE_HIGH);
                    widgets::remote_image(
                        ui,
                        artwork,
                        std::slice::from_ref(url),
                        ArtworkKind::Hero,
                        r,
                        10.0,
                    );
                    x += shot_w + 16.0;
                }
                y += shot_h + 32.0;
            }
            let used = y - ui.cursor().min.y;
            ui.allocate_exact_size(vec2(width, used.max(0.0) + 24.0), Sense::hover());
        });
    (nav, output.state.clamped_offset().y)
}

fn meta_line(card: &LibraryCard) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !card.publisher_name.is_empty() {
        parts.push(card.publisher_name.clone());
    }
    if let Some(year) = card
        .release_date
        .get(..4)
        .filter(|y| y.chars().all(|c| c.is_ascii_digit()))
    {
        parts.push(year.to_owned());
    }
    if card.max_players > 1 {
        parts.push(format!("Up to {} players", card.max_players));
    }
    parts.join("  ·  ")
}

fn detail_rows(card: &LibraryCard) -> Vec<(&'static str, String)> {
    let mut rows = Vec::new();
    if !card.publisher_name.is_empty() {
        rows.push(("Publisher", card.publisher_name.clone()));
    }
    if !card.developer_name.is_empty() && card.developer_name != card.publisher_name {
        rows.push(("Developer", card.developer_name.clone()));
    }
    if let Some(date) = card.release_date.get(..10) {
        rows.push(("Released", date.to_owned()));
    }
    if card.max_players > 0 {
        rows.push(("Players", card.max_players.to_string()));
    }
    let input = if card.supported_input_types.iter().any(|t| {
        t.eq_ignore_ascii_case("CustomTouchOverlay") || t.to_ascii_lowercase().contains("touch")
    }) {
        "Controller, touch"
    } else {
        "Controller"
    };
    rows.push(("Input", input.to_owned()));
    rows
}

// ----------------------------------------------------------------------------- small widgets

fn avatar_button(ui: &mut Ui, size: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, ACCOUNT_LABEL));
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let t = kit::hover_t(ui, response.id, response.hovered());
    ui.painter().circle_filled(
        rect.center(),
        size / 2.0,
        kit::mix(
            theme::SURFACE_HIGHER,
            Color32::from_rgb(0x3a, 0x3f, 0x46),
            t,
        ),
    );
    icons::paint(
        ui.painter(),
        rect.shrink(size * 0.22),
        Icon::Person,
        theme::TEXT_PRIMARY,
    );
    ui.painter()
        .circle_filled(rect.right_bottom() - vec2(5.0, 5.0), 5.0, theme::BACKGROUND);
    ui.painter()
        .circle_filled(rect.right_bottom() - vec2(5.0, 5.0), 3.5, theme::SUCCESS);
    response.on_hover_text(ACCOUNT_LABEL)
}

fn menu_item(ui: &mut Ui, icon: Icon, text: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::MenuItem, true, text));
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(8), theme::SURFACE_HIGHER);
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    icons::paint(
        ui.painter(),
        icons::centered(pos2(rect.left() + 20.0, rect.center().y), 18.0),
        icon,
        theme::TEXT_SECONDARY,
    );
    ui.painter().text(
        pos2(rect.left() + 42.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        theme::font(14.5),
        theme::TEXT_PRIMARY,
    );
    response
}

fn check_item(ui: &mut Ui, text: &str, checked: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::click());
    response
        .widget_info(|| egui::WidgetInfo::selected(egui::Role::MenuItemRadio, true, checked, text));
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(8), theme::SURFACE_HIGHER);
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    ui.painter().text(
        pos2(rect.left() + 14.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        theme::font(14.5),
        theme::TEXT_PRIMARY,
    );
    if checked {
        icons::paint(
            ui.painter(),
            icons::centered(pos2(rect.right() - 18.0, rect.center().y), 16.0),
            Icon::Check,
            theme::ACCENT,
        );
    }
    response
}

fn menu_caption(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text.to_uppercase())
            .font(font_w(11.0, Weight::Semibold))
            .color(theme::TEXT_TERTIARY)
            .extra_letter_spacing(0.8),
    );
}

fn chip(ui: &mut Ui, text: &str, selected: bool) -> egui::Response {
    let font = font_w(13.0, Weight::Medium);
    let w = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font.clone(), Color32::WHITE)
        .size()
        .x
        + 26.0;
    let (rect, response) = ui.allocate_exact_size(vec2(w.max(48.0), 32.0), Sense::click());
    response
        .widget_info(|| egui::WidgetInfo::selected(egui::Role::RadioButton, true, selected, text));
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let (fill, fg) = if selected {
        (theme::TEXT_PRIMARY, theme::ON_WHITE)
    } else if response.hovered() {
        (theme::SURFACE_HIGHER, theme::TEXT_PRIMARY)
    } else {
        (theme::SURFACE_HIGH, theme::TEXT_PRIMARY)
    };
    ui.painter().rect_filled(rect, CornerRadius::same(16), fill);
    ui.painter()
        .text(rect.center(), Align2::CENTER_CENTER, text, font, fg);
    response
}

fn see_all_row(ui: &mut Ui, text: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Link, true, text));
    let t = kit::hover_t(ui, response.id, response.hovered());
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let color = kit::mix(theme::TEXT_SECONDARY, theme::TEXT_PRIMARY, t);
    ui.painter().text(
        pos2(rect.left() + 2.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        font_w(14.0, Weight::Medium),
        color,
    );
    icons::paint(
        ui.painter(),
        icons::centered(pos2(rect.right() - 8.0, rect.center().y), 14.0),
        Icon::ChevronRight,
        color,
    );
    response
}

fn compact_thumb(ui: &mut Ui, card: &LibraryCard, artwork: &dyn ArtworkSource) -> egui::Response {
    let (row, response) = ui.allocate_exact_size(vec2(ui.available_width(), 60.0), Sense::click());
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, card.name.as_str()));
    let thumb = Rect::from_center_size(row.center(), vec2(48.0, 48.0));
    ui.painter()
        .rect_filled(thumb, CornerRadius::same(8), theme::SURFACE_HIGH);
    widgets::remote_image(
        ui,
        artwork,
        &kit::box_art_urls(card),
        ArtworkKind::Tile,
        thumb,
        8.0,
    );
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        ui.painter().rect_stroke(
            thumb,
            CornerRadius::same(8),
            Stroke::new(2.0, Color32::WHITE),
            StrokeKind::Outside,
        );
    }
    response.on_hover_text(card.name.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AuthState, Handle, SessionSnapshot};
    use crate::ui::widgets::NoArtwork;
    use crossbeam_channel::Receiver;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    fn card(id: &str, name: &str, mru: bool) -> LibraryCard {
        LibraryCard {
            title_id: id.into(),
            product_id: format!("P{id}"),
            name: name.into(),
            publisher_name: "Studio".into(),
            short_description: "A game.".into(),
            hero_image_url: format!("hero-{id}"),
            art_url: format!("art-{id}"),
            is_in_mru: mru,
            ..Default::default()
        }
    }

    fn snapshot() -> LibrarySnapshot {
        let cards = vec![
            card("1", "Alpha", true),
            card("2", "Bravo", false),
            card("3", "Charlie", true),
            card("4", "Delta", false),
        ];
        let mru: Vec<LibraryCard> = cards.iter().filter(|c| c.is_in_mru).cloned().collect();
        let shelves = vec![
            Shelf {
                id: SHELF_MRU.into(),
                title: "Jump back in".into(),
                items: mru.clone(),
            },
            Shelf {
                id: SHELF_RECENTLY_ADDED.into(),
                title: "Recently added".into(),
                items: cards.clone(),
            },
        ];
        LibrarySnapshot {
            items: cards.clone(),
            all_items: cards,
            shelf_items: shelves
                .iter()
                .map(|s| (s.id.clone(), s.items.clone()))
                .collect(),
            shelves,
            ..Default::default()
        }
    }

    struct Fixture {
        library: LibraryHandle,
        rx: Receiver<LibraryCommand>,
        session: SessionHandle,
    }

    fn fixture() -> Fixture {
        let (library, _snap, rx) = Handle::new(snapshot());
        let session = Handle::detached(SessionSnapshot {
            state: AuthState::Authenticated,
            ..Default::default()
        });
        Fixture {
            library,
            rx,
            session,
        }
    }

    fn harness<'a>(
        view: &'a mut LibraryView,
        f: &'a Fixture,
        actions: &'a mut Vec<LibraryAction>,
    ) -> Harness<'a> {
        Harness::builder()
            .with_size(egui::vec2(1400.0, 900.0))
            .with_context_setup(theme::apply)
            .with_max_steps(24)
            .allow_missing_glyphs()
            .build_ui(move |ui| {
                if let Some(a) = view.show(ui, &f.library, &f.session, &NoArtwork) {
                    actions.push(a);
                }
            })
    }

    fn drain(rx: &Receiver<LibraryCommand>) -> Vec<LibraryCommand> {
        rx.try_iter().collect()
    }

    #[test]
    fn rejected_session_offers_sign_in_instead_of_retrying_library() {
        let mut f = fixture();
        let (library, _, rx) = Handle::new(LibrarySnapshot {
            authentication_required: true,
            last_error: "HTTP 401: ".into(),
            ..snapshot()
        });
        f.library = library;
        f.rx = rx;
        let mut view = LibraryView::new();
        let mut actions = Vec::new();
        {
            let mut h = harness(&mut view, &f, &mut actions);
            h.run_steps(2);
            h.get_by_label("Please sign in again");
            h.get_by_label("Sign in again").click();
            h.run_steps(2);
        }
        assert!(actions.contains(&LibraryAction::SignIn));
        assert!(
            !drain(&f.rx)
                .iter()
                .any(|c| matches!(c, LibraryCommand::Refresh { .. }))
        );
    }

    #[test]
    fn home_shows_sections_and_recent_games() {
        let f = fixture();
        let mut view = LibraryView::new();
        let mut actions = Vec::new();
        let mut h = harness(&mut view, &f, &mut actions);
        h.run_steps(3);
        h.get_by_label("Jump back in");
        h.get_by_label("Last played");
        assert!(h.query_all_by_label("Alpha").next().is_some());
        h.get_by_label("Recently added");
    }

    #[test]
    fn library_tabs_switch_the_recent_filter() {
        let f = fixture();
        let mut view = LibraryView::new();
        let mut actions = Vec::new();
        {
            let mut h = harness(&mut view, &f, &mut actions);
            h.run_steps(2);
            h.get_by_label("Library").click();
            h.run_steps(3);
            h.get_by_role_and_label(egui::Role::Tab, "Recently played")
                .click();
            h.run_steps(3);
        }
        assert_eq!(view.tab(), LibraryTab::Recent);
        assert_eq!(view.page(), Page::Library);
    }

    #[test]
    fn opening_a_game_and_playing_it() {
        let f = fixture();
        let mut view = LibraryView::new();
        let mut actions = Vec::new();
        {
            let mut h = harness(&mut view, &f, &mut actions);
            h.run_steps(2);
            // The sidebar's recent list opens the game page.
            // An accessibility press opens the page (a mouse click on the centre would
            // hit the play disc instead).
            h.get_all_by_label("Alpha")
                .next()
                .expect("alpha")
                .click_accesskit();
            h.run_steps(3);
            h.get_by_label("Play").click();
            h.run_steps(2);
        }
        assert!(matches!(view.route(), Route::Game(c) if c.title_id == "1"));
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, LibraryAction::Play(c) if c.title_id == "1"))
        );
    }

    #[test]
    fn back_returns_from_a_game_page() {
        let f = fixture();
        let mut view = LibraryView::new();
        view.open_game(&card("2", "Bravo", false), &f.library);
        let mut actions = Vec::new();
        {
            let mut h = harness(&mut view, &f, &mut actions);
            h.run_steps(2);
            h.get_by_label("Back").click();
            h.run_steps(2);
        }
        assert_eq!(view.route(), &Route::Page(Page::Home));
    }

    #[test]
    fn search_is_debounced_then_sent() {
        let f = fixture();
        let mut view = LibraryView::new();
        let mut actions = Vec::new();
        {
            let mut h = harness(&mut view, &f, &mut actions);
            h.run_steps(2);
            h.get_by_label(SEARCH_HINT).click();
            h.run_steps(1);
            h.get_by_label(SEARCH_HINT).type_text("forza");
            h.run_steps(1);
            assert!(
                !drain(&f.rx)
                    .iter()
                    .any(|c| matches!(c, LibraryCommand::SetQuery(q) if q == "forza")),
                "not before the debounce"
            );
            h.run_steps(20);
        }
        assert_eq!(view.search_text(), "forza");
        assert!(view.searching());
    }

    #[test]
    fn settings_button_reports_open_settings() {
        let f = fixture();
        let mut view = LibraryView::new();
        let mut actions = Vec::new();
        {
            let mut h = harness(&mut view, &f, &mut actions);
            h.run_steps(2);
            h.get_by_label(SETTINGS_LABEL).click();
            h.run_steps(2);
        }
        assert!(actions.contains(&LibraryAction::OpenSettings));
    }

    #[test]
    fn featured_skips_recent_and_needs_hero_art() {
        let snap = snapshot();
        let featured: Vec<&str> = featured_cards(&snap, &UserData::default())
            .iter()
            .map(|c| c.title_id.as_str())
            .collect();
        assert_eq!(featured, ["2", "4"]);
    }

    #[test]
    fn sidebar_collapses() {
        let f = fixture();
        let mut view = LibraryView::new();
        let mut actions = Vec::new();
        {
            let mut h = harness(&mut view, &f, &mut actions);
            h.run_steps(2);
            h.get_by_label("Collapse sidebar").click();
            h.run_steps(12);
            h.get_by_label("Expand sidebar");
        }
        assert!(view.sidebar_collapsed());
    }

    #[test]
    fn a_game_opened_from_search_results_shows_its_page_and_back_returns() {
        let f = fixture();
        let mut view = LibraryView::new();
        let mut actions = Vec::new();
        {
            let mut h = harness(&mut view, &f, &mut actions);
            h.run_steps(2);
            h.get_by_label(SEARCH_HINT).click();
            h.run_steps(1);
            h.get_by_label(SEARCH_HINT).type_text("bra");
            h.run_steps(20);
            h.get_by_label("Results for “bra”");
            // The grid tile (the sidebar only lists recent games, Bravo is not one).
            h.get_by_label("Bravo").click_accesskit();
            h.run_steps(3);
            assert!(
                h.query_by_label("Results for “bra”").is_none(),
                "results replaced"
            );
            h.get_by_label("Play");
            h.get_by_label("Back").click();
            h.run_steps(3);
            h.get_by_label("Results for “bra”");
        }
        assert_eq!(view.search_text(), "bra", "search kept");
        assert!(actions.is_empty());
    }

    #[test]
    fn the_tile_play_disc_starts_the_game() {
        let f = fixture();
        let mut view = LibraryView::new();
        view.navigate(Page::Library, &f.library);
        let mut actions = Vec::new();
        {
            let mut h = harness(&mut view, &f, &mut actions);
            h.run_steps(3);
            h.get_by_label(&kit::play_label("Delta")).click();
            h.run_steps(2);
        }
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, LibraryAction::Play(c) if c.title_id == "4")),
            "{actions:?}"
        );
        assert_eq!(
            view.route(),
            &Route::Page(Page::Library),
            "no detour via the page"
        );
    }

    fn store_with(f: impl FnOnce(&mut UserData)) -> UserDataHandle {
        let mut data = UserData::default();
        f(&mut data);
        crate::user_data::UserDataStore::in_memory(data).handle()
    }

    #[test]
    fn recommendation_cache_tracks_favorites_and_catalog_updates() {
        let view = LibraryView::new();
        let mut snap = LibrarySnapshot {
            all_items: vec![
                card("A", "Seed", false),
                card("B", "Racer", false),
                card("C", "Other", false),
            ],
            ..Default::default()
        };
        for card in &mut snap.all_items {
            card.publisher_name.clear();
        }
        snap.all_items[0].categories = vec!["Racing".into()];
        snap.all_items[1].categories = vec!["Racing".into()];
        snap.all_items[2].categories = vec!["Shooter".into()];
        let mut user = UserData::default();
        user.toggle_favorite("A");
        assert_eq!(view.recommended_cards(&snap, &user, 20)[0].title_id, "B");
        assert_eq!(view.recommended_cards(&snap, &user, 20)[0].title_id, "B");
        user.toggle_favorite("B");
        assert!(view.recommended_cards(&snap, &user, 20).is_empty());
        snap.all_items[2].publisher_name = "Publisher".into();
        snap.all_items[0].publisher_name = "Publisher".into();
        snap.revision += 1;
        assert_eq!(view.recommended_cards(&snap, &user, 20)[0].title_id, "C");
    }

    #[test]
    #[ignore = "manual performance benchmark"]
    fn cached_recommendation_performance_2800_games() {
        let view = LibraryView::new();
        let snap = LibrarySnapshot {
            all_items: (0..2800)
                .map(|i| LibraryCard {
                    title_id: format!("T{i}"),
                    name: format!("Game {i:04}"),
                    categories: vec![format!("Genre {}", i % 15), format!("Genre {}", i % 7)],
                    publisher_name: format!("Publisher {}", i % 30),
                    developer_name: format!("Developer {}", i % 80),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let mut user = UserData::default();
        for i in 0..40 {
            user.record_launch(&format!("T{}", i * 13), 1000 + i);
        }
        let now = user_data::now_secs();
        let start = std::time::Instant::now();
        for _ in 0..50 {
            std::hint::black_box(user_data::recommendations(
                &user,
                &[],
                &snap.all_items,
                now,
                60,
            ));
        }
        let raw_ms = start.elapsed().as_secs_f64() * 1000.0 / 50.0;
        assert_eq!(view.recommended_cards(&snap, &user, 20).len(), 20);
        let start = std::time::Instant::now();
        for _ in 0..50_000 {
            std::hint::black_box(view.recommended_cards(&snap, &user, 20));
        }
        let cached_ms = start.elapsed().as_secs_f64() * 1000.0 / 50_000.0;
        eprintln!("2800 games: scoring {raw_ms:.3} ms, cached redraw {cached_ms:.6} ms");
    }

    #[test]
    fn favorites_filter_intersects_results_and_updates_after_unfavorite() {
        let mut view = LibraryView::new();
        let user = store_with(|u| {
            u.toggle_favorite("3");
            u.toggle_favorite("2");
        });
        view.set_user_data(user.clone());
        view.favorites_only = true;
        let mut snap = snapshot();
        snap.items.retain(|c| c.title_id != "2");
        let ids: Vec<_> = view
            .grid_items(&snap, GridList::All)
            .iter()
            .map(|c| c.title_id.as_str())
            .collect();
        assert_eq!(ids, ["3"]);
        user.write().toggle_favorite("3");
        assert!(view.grid_items(&snap, GridList::All).is_empty());
        view.favorites_only = false;
        assert_eq!(
            view.grid_items(&snap, GridList::All).len(),
            snap.items.len()
        );
    }

    #[test]
    fn recent_tab_lists_local_history_first() {
        let f = fixture();
        let mut view = LibraryView::new();
        view.set_user_data(store_with(|u| u.record_launch("4", 100)));
        let snap = snapshot();
        let ids: Vec<&str> = view
            .grid_items(&snap, GridList::Recent)
            .iter()
            .map(|c| c.title_id.as_str())
            .collect();
        assert_eq!(ids, ["4", "1", "3"]);
        let _ = f;
    }

    #[test]
    fn heart_on_a_tile_toggles_the_favourite() {
        let f = fixture();
        let user = store_with(|_| {});
        let mut view = LibraryView::new();
        view.set_user_data(user.clone());
        view.open_library_tab(LibraryTab::All, &f.library);
        let mut actions = Vec::new();
        {
            let mut h = harness(&mut view, &f, &mut actions);
            h.run_steps(3);
            h.get_by_label("Add Bravo to favourites").click_accesskit();
            h.run_steps(2);
        }
        assert!(user.read().is_favorite("2"));
        assert!(actions.is_empty(), "the heart must not open or play");
    }

    #[test]
    fn favourites_tab_and_home_shelf() {
        let f = fixture();
        let user = store_with(|u| {
            u.toggle_favorite("3");
        });
        let mut view = LibraryView::new();
        view.set_user_data(user.clone());
        let mut actions = Vec::new();
        {
            let mut h = harness(&mut view, &f, &mut actions);
            h.run_steps(3);
            h.get_by_label("Your favourites");
        }
        view.open_library_tab(LibraryTab::Favorites, &f.library);
        let snap = snapshot();
        let ids: Vec<&str> = view
            .grid_items(&snap, GridList::Favorites)
            .iter()
            .map(|c| c.title_id.as_str())
            .collect();
        assert_eq!(ids, ["3"]);

        user.write().toggle_favorite("3");
        let mut h = harness(&mut view, &f, &mut actions);
        h.run_steps(3);
        h.get_by_label("No favourites yet");
    }

    #[test]
    fn game_page_heart_and_play_summary() {
        let f = fixture();
        let user = store_with(|u| {
            u.record_launch("2", crate::user_data::now_secs());
            u.add_play_time("2", 5_400);
        });
        let mut view = LibraryView::new();
        view.set_user_data(user.clone());
        view.open_game(&card("2", "Bravo", false), &f.library);
        let mut actions = Vec::new();
        {
            let mut h = harness(&mut view, &f, &mut actions);
            h.run_steps(3);
            h.get_by_label("Add to favourites").click();
            h.run_steps(2);
            h.get_by_label("Remove from favourites");
        }
        assert!(user.read().is_favorite("2"));
    }

    #[test]
    fn play_summary_formats() {
        let mut r = crate::user_data::PlayRecord {
            title_id: "x".into(),
            last_played: 1_000_000,
            sessions: 1,
            seconds_played: 0,
        };
        assert_eq!(play_summary(&r, 1_000_010), "Last played just now");
        r.seconds_played = 45 * 60;
        assert_eq!(
            play_summary(&r, 1_000_000 + 7_200),
            "Last played 2 hours ago · 45 min"
        );
        r.seconds_played = 8_100;
        assert_eq!(
            play_summary(&r, 1_000_000 + 3 * 86_400),
            "Last played 3 days ago · 2 h 15 min"
        );
    }
}
