//! Renders the app's screens headlessly (egui_kittest + wgpu) to PNG files for design
//! review, using real catalog data and artwork from a local folder.
//!
//! ```text
//! cargo run -p xcloud-app --example design_preview -- <games.json> <out-dir> [screen...]
//! ```
//!
//! `games.json` maps product ids to `{name, publisher, developer, desc, release, attrs,
//! title_id, tile, hero, titled, shots}` where the image fields are file paths relative
//! to the json file. Screens: home, home-collapsed, library, library-compact, game,
//! search, settings, signin, devicecode, loading, stream.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{TextureHandle, TextureOptions, vec2};
use parking_lot::RwLock;
use xcloud_app::artwork::{ArtworkKind, ArtworkState};
use xcloud_app::library::{
    SHELF_FEATURE_PREFIX, SHELF_MRU, SHELF_MRU_TITLE, SHELF_RECENTLY_ADDED,
    SHELF_RECENTLY_ADDED_TITLE,
};
use xcloud_app::state::{
    AppSettings, AuthState, FeatureGroup, Handle, LibraryHandle, LibrarySnapshot, SessionSnapshot,
    Shelf, StreamSnapshot,
};
use xcloud_app::theme;
use xcloud_app::ui::library_view::{LibraryTab, LibraryView, Page};
use xcloud_app::ui::widgets::ArtworkSource;
use xcloud_app::user_data::{UserData, UserDataStore};
use xcloud_proto::library_filter::LibraryFilter;
use xcloud_proto::models::{LibraryAttribute, LibraryCard};
use xcloud_proto::streaming_types::StreamLifecycleState;

/// Loads artwork from `file://` paths synchronously.
struct DiskArt {
    ctx: RefCell<Option<egui::Context>>,
    cache: RefCell<HashMap<String, Option<TextureHandle>>>,
}

impl DiskArt {
    fn new() -> Self {
        Self {
            ctx: RefCell::new(None),
            cache: RefCell::new(HashMap::new()),
        }
    }
    fn set_ctx(&self, ctx: &egui::Context) {
        *self.ctx.borrow_mut() = Some(ctx.clone());
    }
    fn load(&self, url: &str) -> Option<TextureHandle> {
        if let Some(hit) = self.cache.borrow().get(url) {
            return hit.clone();
        }
        let path = url.strip_prefix("file://")?;
        let tex = image::ImageReader::open(path)
            .ok()
            .and_then(|r| r.with_guessed_format().ok())
            .and_then(|r| r.decode().ok())
            .and_then(|img| {
                let img = img.to_rgba8();
                let size = [img.width() as usize, img.height() as usize];
                let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
                self.ctx
                    .borrow()
                    .as_ref()
                    .map(|ctx| ctx.load_texture(url, color, TextureOptions::LINEAR))
            });
        self.cache.borrow_mut().insert(url.to_owned(), tex.clone());
        tex
    }
}

impl ArtworkSource for DiskArt {
    fn request(&self, urls: &[String], _kind: ArtworkKind, _max_px: u32) -> ArtworkState {
        for url in urls {
            if let Some(tex) = self.load(url) {
                return ArtworkState::Ready(tex);
            }
        }
        ArtworkState::Failed
    }
}

fn load_cards(json: &Path) -> Vec<LibraryCard> {
    let base = json.parent().map(Path::to_path_buf).unwrap_or_default();
    let text = std::fs::read_to_string(json).expect("read games json");
    let value: serde_json::Value = serde_json::from_str(&text).expect("parse games json");
    let file = |v: &serde_json::Value| -> String {
        v.as_str()
            .map(|p| format!("file://{}", base.join(p).display()))
            .unwrap_or_default()
    };
    let mut cards: Vec<LibraryCard> = value
        .as_object()
        .expect("object")
        .iter()
        .map(|(pid, g)| {
            let s = |k: &str| g.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let attrs: Vec<LibraryAttribute> = g
                .get("attrs")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .map(|n| LibraryAttribute::new(n, n))
                        .collect()
                })
                .unwrap_or_default();
            let max_players = attrs
                .iter()
                .find_map(|a| {
                    a.name
                        .split_whitespace()
                        .find_map(|w| w.parse::<u32>().ok())
                })
                .unwrap_or(1);
            LibraryCard {
                title_id: s("title_id"),
                product_id: pid.clone(),
                name: s("name"),
                short_description: s("desc"),
                art_url: file(g.get("tile").unwrap_or(&serde_json::Value::Null)),
                poster_image_url: String::new(),
                hero_image_url: file(g.get("hero").unwrap_or(&serde_json::Value::Null)),
                titled_hero_url: file(g.get("titled").unwrap_or(&serde_json::Value::Null)),
                description: s("desc"),
                gallery_image_urls: g
                    .get("shots")
                    .and_then(|a| a.as_array())
                    .map(|a| a.iter().map(&file).collect())
                    .unwrap_or_default(),
                publisher_name: s("publisher"),
                developer_name: s("developer"),
                release_date: s("release"),
                is_in_mru: false,
                attributes: attrs,
                supported_input_types: vec![],
                max_players,
                display_art_url: String::new(),
                categories: g
                    .get("genres")
                    .and_then(|a| a.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        })
        .filter(|c| !c.name.is_empty())
        .collect();
    cards.sort_by_key(|c| c.name.to_lowercase());
    // Pretend a handful were played recently.
    for i in [3usize, 11, 17, 24, 30, 41, 7] {
        if let Some(c) = cards.get_mut(i) {
            c.is_in_mru = true;
        }
    }
    cards
}

fn snapshot(cards: &[LibraryCard]) -> LibrarySnapshot {
    let mru: Vec<LibraryCard> = cards.iter().filter(|c| c.is_in_mru).cloned().collect();
    let mut recent = cards.to_vec();
    recent.sort_by(|a, b| b.release_date.cmp(&a.release_date));
    let mut shelves = vec![
        Shelf {
            id: SHELF_MRU.into(),
            title: SHELF_MRU_TITLE.into(),
            items: mru,
        },
        Shelf {
            id: SHELF_RECENTLY_ADDED.into(),
            title: SHELF_RECENTLY_ADDED_TITLE.into(),
            items: recent.into_iter().take(14).collect(),
        },
    ];
    let mut buckets: HashMap<String, Vec<LibraryCard>> = HashMap::new();
    for c in cards {
        for a in &c.attributes {
            buckets.entry(a.name.clone()).or_default().push(c.clone());
        }
    }
    let mut ordered: Vec<_> = buckets.into_iter().filter(|(_, v)| v.len() >= 6).collect();
    ordered.sort_by_key(|a| std::cmp::Reverse(a.1.len()));
    for (title, items) in ordered.into_iter().take(3) {
        shelves.push(Shelf {
            id: format!("{SHELF_FEATURE_PREFIX}{title}"),
            title,
            items,
        });
    }
    let shelf_items: HashMap<String, Vec<LibraryCard>> = shelves
        .iter()
        .map(|s| (s.id.clone(), s.items.clone()))
        .collect();
    LibrarySnapshot {
        revision: 0,
        authentication_required: false,
        shelf_items,
        items: cards.to_vec(),
        all_items: cards.to_vec(),
        browse_shelf: String::new(),
        shelves,
        feature_groups: vec![FeatureGroup {
            id: "multiplayer".into(),
            title: "Multiplayer".into(),
            count: 3,
            features: vec![
                "Online multiplayer".into(),
                "Local co-op".into(),
                "Cross-platform".into(),
            ],
        }],
        query: String::new(),
        filter: LibraryFilter::default(),
        is_loading: false,
        status_text: String::new(),
        last_error: String::new(),
    }
}

fn render(
    name: &str,
    out: &Path,
    size: egui::Vec2,
    frames: usize,
    mut app: impl FnMut(&mut egui::Ui),
) {
    let mut harness = egui_kittest::Harness::builder()
        .with_size(size)
        .with_pixels_per_point(2.0)
        .wgpu()
        .build_ui(|ui| {
            theme::apply(ui.ctx());
            app(ui)
        });
    for _ in 0..frames {
        harness.run_steps(1);
    }
    match harness.render() {
        Ok(img) => {
            let path = out.join(format!("{name}.png"));
            img.save(&path).expect("save png");
            println!("{}", path.display());
        }
        Err(e) => eprintln!("{name}: render failed: {e}"),
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let json = PathBuf::from(args.next().expect("games.json"));
    let out = PathBuf::from(args.next().expect("out dir"));
    let screens: Vec<String> = args.collect();
    let want = |s: &str| screens.is_empty() || screens.iter().any(|x| x == s);
    std::fs::create_dir_all(&out).expect("out dir");

    let cards = load_cards(&json);
    let snap = snapshot(&cards);
    let size = vec2(1440.0, 900.0);
    let session = Handle::detached(SessionSnapshot {
        state: AuthState::Authenticated,
        ..Default::default()
    });

    // A few games played and favourited locally.
    let mut user = UserData::default();
    let now = xcloud_app::user_data::now_secs();
    for (n, i) in [5usize, 13, 21, 2].into_iter().enumerate() {
        if let Some(c) = cards.get(i) {
            user.record_launch(&c.title_id, now - 3_600 * 30 * n as u64);
            user.add_play_time(&c.title_id, 1_800 * (4 - n as u64));
        }
    }
    for i in [5usize, 9, 16, 28, 33] {
        if let Some(c) = cards.get(i) {
            user.toggle_favorite(&c.title_id);
        }
    }
    let user = UserDataStore::in_memory(user).handle();

    let browse = |name: &str, setup: &dyn Fn(&mut LibraryView, &LibraryHandle)| {
        let art = DiskArt::new();
        let library: LibraryHandle = Handle::detached(snap.clone());
        let mut view = LibraryView::new();
        view.set_user_data(user.clone());
        setup(&mut view, &library);
        render(name, &out, size, 6, |ui| {
            art.set_ctx(ui.ctx());
            let _ = view.show(ui, &library, &session, &art);
        });
    };

    if want("home") {
        browse("home", &|_, _| {});
    }
    if want("home-tall") {
        let art = DiskArt::new();
        let library: LibraryHandle = Handle::detached(snap.clone());
        let mut view = LibraryView::new();
        view.set_user_data(user.clone());
        render("home-tall", &out, vec2(1440.0, 2200.0), 6, |ui| {
            art.set_ctx(ui.ctx());
            let _ = view.show(ui, &library, &session, &art);
        });
    }
    if want("home-collapsed") {
        browse("home-collapsed", &|v, _| v.set_sidebar_collapsed(true));
    }
    if want("library") {
        browse("library", &|v, l| v.navigate(Page::Library, l));
    }
    if want("favorites") {
        browse("favorites", &|v, l| {
            v.open_library_tab(LibraryTab::Favorites, l)
        });
    }
    if want("picks") {
        browse("picks", &|v, l| v.open_picks(l));
    }
    if want("library-compact") {
        browse("library-compact", &|v, l| {
            v.navigate(Page::Library, l);
            v.set_compact_grid(true);
        });
    }
    if want("game") {
        let card = cards.get(5).cloned();
        browse("game", &move |v, l| {
            if let Some(card) = &card {
                v.open_game(card, l);
            }
        });
    }
    for (name, category) in [
        (
            "settings",
            xcloud_app::ui::settings_sheet::Category::Streaming,
        ),
        (
            "settings-video",
            xcloud_app::ui::settings_sheet::Category::Video,
        ),
        (
            "settings-controller",
            xcloud_app::ui::settings_sheet::Category::Controller,
        ),
        (
            "settings-language",
            xcloud_app::ui::settings_sheet::Category::Language,
        ),
    ] {
        if !want(name) {
            continue;
        }
        let art = DiskArt::new();
        let library: LibraryHandle = Handle::detached(snap.clone());
        let mut view = LibraryView::new();
        let mut sheet = xcloud_app::ui::settings_sheet::SettingsSheet::new();
        sheet.set_category(category);
        let mut settings = AppSettings {
            locale: "de-DE".into(),
            ..AppSettings::default()
        };
        render(name, &out, size, 6, |ui| {
            art.set_ctx(ui.ctx());
            view.set_settings_open(true);
            let _ = view.show(ui, &library, &session, &art);
            let _ = sheet.show(ui.ctx(), &mut settings, "0.1.0", true);
        });
    }
    if want("signin") {
        let session = Handle::detached(SessionSnapshot {
            state: AuthState::Unauthenticated,
            ..Default::default()
        });
        let mut view = xcloud_app::ui::signin::SignInView::new();
        render("signin", &out, size, 4, |ui| view.show(ui, &session, true));
    }
    if want("devicecode") {
        let session = Handle::detached(SessionSnapshot {
            state: AuthState::Authenticating,
            user_code: "K7MX-QP4R".into(),
            verification_uri: "https://www.microsoft.com/link".into(),
            sign_in_url: "https://www.microsoft.com/link?otc=K7MXQP4R".into(),
            status_text: "Waiting for you to finish signing in".into(),
            ..Default::default()
        });
        let mut view = xcloud_app::ui::device_code::DeviceCodeView::new().without_auto_open();
        render("devicecode", &out, size, 4, |ui| view.show(ui, &session));
    }
    for (name, lifecycle, status, error) in [
        (
            "stream-launch",
            StreamLifecycleState::WaitingForResources,
            "Waiting for a server · about 1 minute",
            "",
        ),
        (
            "stream-error",
            StreamLifecycleState::Failed,
            "Couldn't start the stream",
            "The service has no free consoles in your region right now. Try again in a few minutes.",
        ),
        ("stream", StreamLifecycleState::Connected, "", ""),
    ] {
        if !want(name) {
            continue;
        }
        let card = cards
            .iter()
            .find(|c| !c.hero_image_url.is_empty())
            .cloned()
            .unwrap_or_default();
        let art = DiskArt::new();
        let stats = xcloud_proto::streaming_types::StreamingStatsSnapshot {
            negotiated_width: 1920,
            negotiated_height: 1080,
            frames_per_second: 59.9,
            bitrate_kbps: 18400,
            round_trip_time_ms: 23.0,
            ..Default::default()
        };
        let snapshot = StreamSnapshot {
            lifecycle,
            status_text: status.into(),
            title_id: card.title_id.clone(),
            title_name: card.name.clone(),
            hero_image: card.hero_image_url.clone(),
            error: error.into(),
            active: true,
            hud_enabled: true,
            stats,
            upscaled: true,
            output_height: 2160,
            upscaler_label: "FSR".into(),
            decode_ms_avg: 2.1,
            decode_ms_max: 3.4,
            gpu_ms: 1.8,
            controller_connected: true,
            controller_name: "Xbox Wireless Controller".into(),
            input_focused: true,
            ..Default::default()
        };
        let stream = Handle::detached(snapshot);
        let (_writer, reader) = xcloud_media::FrameMailbox::open();
        let video = Arc::new(parking_lot::Mutex::new(reader));
        let settings = Arc::new(RwLock::new(AppSettings::default()));
        let mut view = xcloud_app::ui::stream_view::StreamView::new(stream, video, settings);
        render(name, &out, size, 4, |ui| {
            art.set_ctx(ui.ctx());
            view.show(ui, &art);
        });
    }
}
