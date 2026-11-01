//! Port of qt/src/app/ArtworkProvider.{h,cpp} + qt/qml/RemoteImage.qml: download, disk cache,
//! decode pool, egui textures.
//!
//! The Qt side was a `QQuickAsyncImageProvider` (`ArtworkProvider`) whose responses ran on a
//! two-thread `QThreadPool`, plus `RemoteImage.qml`, which tried a list of candidate URLs in
//! order and fell through to the next one on error. Here both become [`ArtworkCache`]:
//!
//! ```text
//!  UI thread                 decode pool (std threads)        tokio runtime
//!  ─────────                 ─────────────────────────        ─────────────
//!  request(urls, kind, px) ─▶ disk cache lookup                [`ArtworkFetcher`] (reqwest)
//!    │ (never blocks)         ├ hit: decode + downscale          bounded by a Semaphore of
//!    ▼                        └ miss ──────────────────────────▶ `pool_size` permits
//!  Loading | Ready(tex)       decode + downscale + write .thumb ◀── bytes
//!    | Failed                 ctx.load_texture(LINEAR) → LRU insert → request_repaint
//! ```
//!
//! The disk cache uses the same key scheme as `ArtworkProvider::cacheKeyFor`
//! (`sha1(url).hex`, files `<key>-<w>x<h>.thumb` for PNG thumbnails and `<key>.img` for an
//! original), so a Qt and a Rust cache directory are interchangeable. Decoded textures live in
//! an in-memory LRU keyed by `(url, max_pixel_size)` with a byte budget (RGBA8 pixels).
//!
//! Sizes: `RemoteImage.qml` computes `sourceSize.width` in *logical* pixels
//! ([`source_size_width`]) and `QQuickImageBase` multiplies the provider request by the
//! device pixel ratio, so a 176-pt tile on a 2x display asks for a 384 px thumbnail. The
//! `max_pixel_size` handed to [`ArtworkCache::request`] is that final device-pixel width;
//! the UI gets it from [`requested_width`].
//!
//! Failures: Qt kept the response per `Image` instance, and delegates are re-created on
//! scroll (`cache: false`, `onUrlsChanged: candidate = 0`), so a URL that failed
//! transiently was re-requested the next time its tile was instantiated. The shared cache
//! here emulates that by forgetting a failed candidate once nobody has asked for it for
//! [`ArtworkConfig::failure_idle`] (the tile scrolled away) or once it is older than
//! [`ArtworkConfig::failure_ttl`] (a tile that never left the screen).

use std::collections::HashMap;
use std::future::Future;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use parking_lot::Mutex;
use thiserror::Error;

const LOG: &str = "Artwork";

/// `m_pool.setMaxThreadCount(2)`: concurrent downloads and decode threads.
pub const DEFAULT_POOL_SIZE: usize = 2;
/// In-memory texture budget (RGBA8 bytes), new in the Rust port.
pub const DEFAULT_TEXTURE_BUDGET_BYTES: usize = 256 * 1024 * 1024;
/// `request.setTransferTimeout(20000)`.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(20);
/// A failed candidate is retried on the next request once it is this old, even when the
/// tile asking for it never left the screen (new in the Rust port; Qt retried whenever the
/// delegate was re-created).
pub const DEFAULT_FAILURE_TTL: Duration = Duration::from_secs(30);
/// A failed candidate nobody requested for this long (its tile scrolled out of view) is
/// forgotten, like a `ListView` delegate being destroyed and re-created.
pub const DEFAULT_FAILURE_IDLE: Duration = Duration::from_secs(2);
/// The Microsoft image service accepts a `w` query parameter and serves a thumbnail.
const THUMBNAIL_HOST: &str = "store-images.s-microsoft.com";
/// `requestedSize.width() > 0 ? requestedSize.width() : 512`.
const DEFAULT_WIDTH: i32 = 512;

// ----------------------------------------------------------------------------- errors

#[derive(Debug, Error)]
pub enum ArtworkError {
    #[error("artwork download failed: {0}")]
    Fetch(String),
    #[error("artwork decode failed: {0}")]
    Decode(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ArtworkError>;

// ----------------------------------------------------------------------------- public types

/// `RemoteImage.kind`: `"hero"` for the featured banner and the stream backdrop, `"tile"` for
/// `GameTile`, `"poster"` for portrait cards. Only the default width (when `max_pixel_size`
/// is 0) and the texture debug name depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtworkKind {
    Hero,
    Poster,
    Tile,
}

impl ArtworkKind {
    /// `sourceSize.width` upper bound in `RemoteImage.qml`: 800 for heroes, 256 otherwise.
    ///
    /// This is a *logical* pixel value: Qt multiplies the provider request by the device
    /// pixel ratio afterwards, so it is not a cap on the device-pixel width passed to
    /// [`ArtworkCache::request`]. Use [`requested_width`] to go from a layout width to the
    /// device-pixel request.
    pub fn default_max_pixel_size(self) -> u32 {
        match self {
            ArtworkKind::Hero => 800,
            ArtworkKind::Poster | ArtworkKind::Tile => 256,
        }
    }

    /// `sourceSize.width` rounding step in `RemoteImage.qml`: 64 for heroes, 32 otherwise.
    fn source_size_step(self) -> f32 {
        match self {
            ArtworkKind::Hero => 64.0,
            ArtworkKind::Poster | ArtworkKind::Tile => 32.0,
        }
    }

    fn name(self) -> &'static str {
        match self {
            ArtworkKind::Hero => "hero",
            ArtworkKind::Poster => "poster",
            ArtworkKind::Tile => "tile",
        }
    }
}

/// What the UI gets back for a tile this frame. `Image.status` in `RemoteImage.qml`.
#[derive(Clone)]
pub enum ArtworkState {
    /// At least one candidate is still being fetched or decoded.
    Loading,
    /// The first candidate that decoded, as an egui texture.
    Ready(egui::TextureHandle),
    /// Every candidate failed (or there were none): draw the placeholder.
    Failed,
}

impl std::fmt::Debug for ArtworkState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArtworkState::Loading => f.write_str("Loading"),
            ArtworkState::Ready(t) => write!(f, "Ready({}x{})", t.size()[0], t.size()[1]),
            ArtworkState::Failed => f.write_str("Failed"),
        }
    }
}

impl ArtworkState {
    pub fn is_ready(&self) -> bool {
        matches!(self, ArtworkState::Ready(_))
    }
    pub fn is_loading(&self) -> bool {
        matches!(self, ArtworkState::Loading)
    }
    pub fn is_failed(&self) -> bool {
        matches!(self, ArtworkState::Failed)
    }
    pub fn texture(&self) -> Option<&egui::TextureHandle> {
        match self {
            ArtworkState::Ready(t) => Some(t),
            _ => None,
        }
    }
}

/// `QSize requestedSize` of `QQuickAsyncImageProvider::requestImageResponse`. Qt passes a
/// non-positive component for "unconstrained"; `RemoteImage.qml` only ever sets the width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestedSize {
    pub width: i32,
    pub height: i32,
}

impl RequestedSize {
    /// `QSize(width, -1)`: what `sourceSize.width` produces.
    pub const fn width_only(width: u32) -> Self {
        Self {
            width: width as i32,
            height: -1,
        }
    }

    /// The suffix of the thumbnail file name: `"-%1x%2.thumb"`.
    fn thumb_suffix(self) -> String {
        format!("-{}x{}.thumb", self.width, self.height)
    }
}

// ----------------------------------------------------------------------------- pure helpers

/// `RemoteImage.sourceSize.width` in logical pixels: the layout width rounded up to the
/// next 32 (tile/poster) or 64 (hero) step and capped at 256 / 800.
///
/// ```text
/// sourceSize.width: root.kind === "hero" ? Math.min(800, Math.ceil(width / 64) * 64)
///                                        : Math.min(256, Math.ceil(width / 32) * 32)
/// ```
pub fn source_size_width(kind: ArtworkKind, logical_width: f32) -> u32 {
    let w = if logical_width.is_finite() {
        logical_width.max(0.0)
    } else {
        0.0
    };
    let step = kind.source_size_step();
    let cap = kind.default_max_pixel_size() as f32;
    ((w / step).ceil() * step).min(cap) as u32
}

/// The width the image provider is actually asked for, in device pixels:
/// [`source_size_width`] times the display scale, as `QQuickImageBase::loadPixmap` does with
/// `d->sourcesize * d->devicePixelRatio` (`QSize * qreal` rounds). A 176-pt `GameTile` on a
/// 2x display therefore requests 384 px, and a hero up to 1600 px.
///
/// `logical_width` is the widget's layout width in egui points; `pixels_per_point` is
/// `ui.pixels_per_point()`.
pub fn requested_width(kind: ArtworkKind, logical_width: f32, pixels_per_point: f32) -> u32 {
    let scale = if pixels_per_point.is_finite() && pixels_per_point > 0.0 {
        pixels_per_point
    } else {
        1.0
    };
    let logical = source_size_width(kind, logical_width) as f32;
    (logical * scale).round().max(0.0) as u32
}

/// `QSize::scaled(bounds, Qt::KeepAspectRatio)` with Qt's integer arithmetic.
fn qsize_scaled_keep_aspect(src: (u32, u32), bounds: (u32, u32)) -> (u32, u32) {
    let (wd, ht) = (i64::from(src.0), i64::from(src.1));
    let (bw, bh) = (i64::from(bounds.0), i64::from(bounds.1));
    if wd == 0 || ht == 0 {
        return bounds;
    }
    let rw = bh * wd / ht;
    let (w, h) = if rw <= bw {
        (rw, bh)
    } else {
        (bw, bw * ht / wd)
    };
    (clamp_u32(w), clamp_u32(h))
}

fn clamp_u32(v: i64) -> u32 {
    u32::try_from(v.max(0)).unwrap_or(u32::MAX)
}

/// `decodeArtwork`'s size policy: the size the decoded image is scaled to, or `src` when no
/// downscale is needed (the reader only gets a `scaledSize` when the target is smaller).
///
/// A width-only request (`sourceSize.width`) derives the height from the source aspect
/// ratio instead of being treated as `QSize::isEmpty()` and decoding the full poster.
pub fn thumbnail_size(src: (u32, u32), requested: RequestedSize) -> (u32, u32) {
    if src.0 == 0 || src.1 == 0 {
        return src;
    }
    let width = if requested.width > 0 {
        requested.width
    } else {
        DEFAULT_WIDTH
    };
    let height = if requested.height > 0 {
        requested.height
    } else {
        let h = f64::from(src.1) * f64::from(width) / f64::from(src.0);
        (h.round() as i32).max(1)
    };
    let target = qsize_scaled_keep_aspect(src, (clamp_u32(width.into()), clamp_u32(height.into())));
    if target.0 < src.0 || target.1 < src.1 {
        target
    } else {
        src
    }
}

/// `ArtworkProvider::cacheKeyFor`: lowercase hex SHA-1 of the UTF-8 URL.
pub fn cache_key_for(url: &str) -> String {
    let digest = sha1::digest(url.as_bytes());
    let mut out = String::with_capacity(40);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Per-user artwork cache, falling back to `~/.cache/openxcloud/artwork` or
/// the temporary directory when no user cache location is available.
pub fn default_cache_directory() -> PathBuf {
    let base = crate::settings::project_dirs()
        .map(|d| d.cache_dir().to_path_buf())
        .or_else(|| {
            directories::UserDirs::new().map(|u| u.home_dir().join(".cache").join("openxcloud"))
        })
        .unwrap_or_else(|| std::env::temp_dir().join("openxcloud"));
    base.join("artwork")
}

/// The URL actually downloaded: on `store-images.s-microsoft.com` the `w` query item is
/// replaced by the requested width so the service returns a thumbnail; other hosts are
/// fetched as-is. Returns `(url, supports_thumbnail)`.
pub fn download_url_for(url: &str, requested: RequestedSize) -> (String, bool) {
    let Ok(mut parsed) = reqwest::Url::parse(url) else {
        return (url.to_owned(), false);
    };
    if parsed.host_str() != Some(THUMBNAIL_HOST) {
        return (url.to_owned(), false);
    }
    let width = if requested.width > 0 {
        requested.width
    } else {
        DEFAULT_WIDTH
    };
    let kept: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| k != "w")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    {
        let mut q = parsed.query_pairs_mut();
        q.clear();
        q.extend_pairs(kept.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        q.append_pair("w", &width.to_string());
    }
    (parsed.to_string(), true)
}

/// `decodeArtwork`: decode with EXIF auto-transform and downscale to [`thumbnail_size`].
pub fn decode_artwork(bytes: &[u8], requested: RequestedSize) -> Result<DynamicImage> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| ArtworkError::Decode(e.to_string()))?;
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| ArtworkError::Decode(e.to_string()))?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img =
        DynamicImage::from_decoder(decoder).map_err(|e| ArtworkError::Decode(e.to_string()))?;
    img.apply_orientation(orientation);
    let src = (img.width(), img.height());
    let target = thumbnail_size(src, requested);
    if target != src {
        img = img.resize_exact(target.0, target.1, image::imageops::FilterType::Triangle);
    }
    Ok(img)
}

// ----------------------------------------------------------------------------- fetcher

pub type FetchFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'a>>;

/// The HTTP GET behind the cache (the per-worker `QNetworkAccessManager` in the C++).
/// Abstracted so tests can run the whole pipeline without a network.
pub trait ArtworkFetcher: Send + Sync + 'static {
    fn fetch(&self, url: &str) -> FetchFuture<'_>;
}

/// `reqwest`-backed fetcher: follows redirects, 20 s transfer timeout, non-2xx is an error.
pub struct ReqwestFetcher {
    client: reqwest::Client,
}

impl ReqwestFetcher {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(TRANSFER_TIMEOUT)
            .build()
            .map_err(|e| ArtworkError::Fetch(e.to_string()))?;
        Ok(Self { client })
    }

    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl ArtworkFetcher for ReqwestFetcher {
    fn fetch(&self, url: &str) -> FetchFuture<'_> {
        let request = self.client.get(url);
        Box::pin(async move {
            let response = request
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| ArtworkError::Fetch(e.to_string()))?;
            let bytes = response
                .bytes()
                .await
                .map_err(|e| ArtworkError::Fetch(e.to_string()))?;
            Ok(bytes.to_vec())
        })
    }
}

// ----------------------------------------------------------------------------- config

#[derive(Debug, Clone)]
pub struct ArtworkConfig {
    /// `ArtworkProvider::cacheDirectory()`.
    pub cache_dir: PathBuf,
    /// In-memory budget for decoded textures, RGBA8 bytes.
    pub texture_budget_bytes: usize,
    /// Decode threads and concurrent downloads (`QThreadPool::maxThreadCount`).
    pub pool_size: usize,
    /// A failed candidate older than this is retried on the next request
    /// ([`DEFAULT_FAILURE_TTL`]).
    pub failure_ttl: Duration,
    /// A failed candidate nobody requested for this long is retried on the next request
    /// ([`DEFAULT_FAILURE_IDLE`]).
    pub failure_idle: Duration,
}

impl Default for ArtworkConfig {
    fn default() -> Self {
        Self {
            cache_dir: default_cache_directory(),
            texture_budget_bytes: DEFAULT_TEXTURE_BUDGET_BYTES,
            pool_size: DEFAULT_POOL_SIZE,
            failure_ttl: DEFAULT_FAILURE_TTL,
            failure_idle: DEFAULT_FAILURE_IDLE,
        }
    }
}

// ----------------------------------------------------------------------------- cache

/// `(url, max_pixel_size)`: one decoded texture per source and size.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    url: String,
    max_pixel_size: u32,
}

struct ReadyEntry {
    texture: egui::TextureHandle,
    bytes: usize,
    last_used: u64,
}

/// `Image.status === Image.Error` for one candidate, remembered only for a while (see the
/// module docs): Qt dropped the error with the delegate.
struct FailedEntry {
    failed_at: Instant,
    last_requested: Instant,
}

impl FailedEntry {
    fn expired(&self, now: Instant, config: &ArtworkConfig) -> bool {
        now.saturating_duration_since(self.failed_at) >= config.failure_ttl
            || now.saturating_duration_since(self.last_requested) >= config.failure_idle
    }
}

enum Slot {
    Loading,
    Ready(ReadyEntry),
    Failed(FailedEntry),
}

#[derive(Default)]
struct Slots {
    map: HashMap<Key, Slot>,
    ready_bytes: usize,
}

impl Slots {
    /// The state of `key` for a tile that wants to show it. An expired failure is removed
    /// and reported as `None`, so the caller re-requests it like a brand-new candidate.
    fn touch(
        &mut self,
        key: &Key,
        tick: u64,
        now: Instant,
        config: &ArtworkConfig,
    ) -> Option<ArtworkState> {
        match self.map.get_mut(key) {
            Some(Slot::Ready(entry)) => {
                entry.last_used = tick;
                Some(ArtworkState::Ready(entry.texture.clone()))
            }
            Some(Slot::Loading) => Some(ArtworkState::Loading),
            Some(Slot::Failed(entry)) => {
                if entry.expired(now, config) {
                    self.map.remove(key);
                    None
                } else {
                    entry.last_requested = now;
                    Some(ArtworkState::Failed)
                }
            }
            None => None,
        }
    }

    /// A decoded texture for `key` if one is resident, without starting anything.
    fn peek_ready(&mut self, key: &Key, tick: u64) -> Option<egui::TextureHandle> {
        match self.map.get_mut(key) {
            Some(Slot::Ready(entry)) => {
                entry.last_used = tick;
                Some(entry.texture.clone())
            }
            _ => None,
        }
    }

    fn insert_ready(&mut self, key: Key, texture: egui::TextureHandle, tick: u64, budget: usize) {
        let [w, h] = texture.size();
        let bytes = w * h * 4;
        self.remove(&key);
        self.ready_bytes += bytes;
        self.map.insert(
            key,
            Slot::Ready(ReadyEntry {
                texture,
                bytes,
                last_used: tick,
            }),
        );
        self.evict_to(budget);
    }

    fn remove(&mut self, key: &Key) {
        if let Some(Slot::Ready(entry)) = self.map.remove(key) {
            self.ready_bytes -= entry.bytes;
        }
    }

    /// Drops least recently used textures until the budget is met. The newest entry is kept
    /// even when it alone exceeds the budget, so a huge hero still shows.
    fn evict_to(&mut self, budget: usize) {
        while self.ready_bytes > budget && self.ready_count() > 1 {
            let victim = self
                .map
                .iter()
                .filter_map(|(k, s)| match s {
                    Slot::Ready(e) => Some((e.last_used, k.clone())),
                    _ => None,
                })
                .min_by_key(|(used, _)| *used)
                .map(|(_, k)| k);
            match victim {
                Some(k) => self.remove(&k),
                None => break,
            }
        }
    }

    fn ready_count(&self) -> usize {
        self.map
            .values()
            .filter(|s| matches!(s, Slot::Ready(_)))
            .count()
    }
}

type Task = Box<dyn FnOnce() + Send + 'static>;

struct Inner {
    ctx: egui::Context,
    runtime: tokio::runtime::Handle,
    fetcher: Arc<dyn ArtworkFetcher>,
    config: ArtworkConfig,
    slots: Mutex<Slots>,
    tick: AtomicU64,
    decode_tx: Sender<Task>,
    download_permits: Arc<tokio::sync::Semaphore>,
}

/// `ArtworkProvider` + `RemoteImage.qml` as one object the library views poll each frame.
#[derive(Clone)]
pub struct ArtworkCache {
    inner: Arc<Inner>,
}

impl ArtworkCache {
    /// `ArtworkProvider::ArtworkProvider`: sizes the pool and creates the cache directory.
    pub fn new(
        ctx: egui::Context,
        runtime: tokio::runtime::Handle,
        fetcher: Arc<dyn ArtworkFetcher>,
        config: ArtworkConfig,
    ) -> Self {
        let pool_size = config.pool_size.max(1);
        if let Err(e) = std::fs::create_dir_all(&config.cache_dir) {
            tracing::warn!(target: LOG, "cannot create artwork cache dir {:?}: {e}", config.cache_dir);
        }
        let (decode_tx, decode_rx) = crossbeam_channel::unbounded::<Task>();
        for i in 0..pool_size {
            let rx: Receiver<Task> = decode_rx.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("artwork-decode-{i}"))
                .spawn(move || {
                    while let Ok(task) = rx.recv() {
                        task();
                    }
                });
            if let Err(e) = spawned {
                tracing::error!(target: LOG, "cannot spawn artwork decode thread: {e}");
            }
        }
        Self {
            inner: Arc::new(Inner {
                ctx,
                runtime,
                fetcher,
                download_permits: Arc::new(tokio::sync::Semaphore::new(pool_size)),
                config,
                slots: Mutex::new(Slots::default()),
                tick: AtomicU64::new(0),
                decode_tx,
            }),
        }
    }

    /// `RemoteImage`: tries `urls` in order (empty ones are skipped) and returns the state
    /// of the first candidate that has not failed. Never blocks and never does I/O; a
    /// candidate seen for the first time is queued on the decode pool.
    ///
    /// `max_pixel_size` is the requested width in device pixels (see [`requested_width`];
    /// 0 = the kind's `sourceSize.width` cap times the display scale).
    ///
    /// A failed candidate is skipped while its failure is remembered and retried once it
    /// expires (module docs). While an earlier candidate is being (re)fetched, a later one
    /// that already decoded keeps showing instead of the placeholder, but later candidates
    /// are never *started* before the earlier one failed, as in `RemoteImage.qml`.
    pub fn request(&self, urls: &[String], kind: ArtworkKind, max_pixel_size: u32) -> ArtworkState {
        let max_pixel_size = self.effective_size(kind, max_pixel_size);
        let tick = self.inner.tick.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let mut pending = false;
        for url in urls.iter().filter(|u| !u.is_empty()) {
            let key = Key {
                url: url.clone(),
                max_pixel_size,
            };
            if pending {
                let ready = self.inner.slots.lock().peek_ready(&key, tick);
                if let Some(texture) = ready {
                    return ArtworkState::Ready(texture);
                }
                continue;
            }
            let (state, fresh) = {
                let mut slots = self.inner.slots.lock();
                match slots.touch(&key, tick, now, &self.inner.config) {
                    Some(state) => (state, false),
                    None => {
                        slots.map.insert(key.clone(), Slot::Loading);
                        (ArtworkState::Loading, true)
                    }
                }
            };
            if fresh {
                // Freshly inserted (or an expired failure): start the job outside the lock.
                self.spawn_lookup(key, kind);
            }
            match state {
                ArtworkState::Failed => continue,
                ArtworkState::Loading => pending = true,
                ready @ ArtworkState::Ready(_) => return ready,
            }
        }
        if pending {
            ArtworkState::Loading
        } else {
            ArtworkState::Failed
        }
    }

    /// Forgets a failed candidate right away so the next `request` retries it
    /// (`RemoteImage` reset `candidate` whenever `urls` changed). Failures also expire on
    /// their own, see [`ArtworkConfig::failure_ttl`] and [`ArtworkConfig::failure_idle`].
    pub fn retry(&self, url: &str, kind: ArtworkKind, max_pixel_size: u32) {
        let key = Key {
            url: url.to_owned(),
            max_pixel_size: self.effective_size(kind, max_pixel_size),
        };
        let mut slots = self.inner.slots.lock();
        if matches!(slots.map.get(&key), Some(Slot::Failed(_))) {
            slots.map.remove(&key);
        }
    }

    /// Forgets every remembered failure, for a library reload (Qt re-created all delegates
    /// on a model reset, so every candidate started from `candidate = 0`).
    pub fn forget_failures(&self) {
        self.inner
            .slots
            .lock()
            .map
            .retain(|_, slot| !matches!(slot, Slot::Failed(_)));
    }

    /// Number of decoded textures currently held.
    pub fn texture_count(&self) -> usize {
        self.inner.slots.lock().ready_count()
    }

    /// RGBA8 bytes of all decoded textures currently held.
    pub fn texture_bytes(&self) -> usize {
        self.inner.slots.lock().ready_bytes
    }

    /// Whether a decoded texture for this exact candidate is resident (does not touch LRU).
    pub fn is_resident(&self, url: &str, kind: ArtworkKind, max_pixel_size: u32) -> bool {
        let key = Key {
            url: url.to_owned(),
            max_pixel_size: self.effective_size(kind, max_pixel_size),
        };
        matches!(self.inner.slots.lock().map.get(&key), Some(Slot::Ready(_)))
    }

    pub fn cache_dir(&self) -> &Path {
        &self.inner.config.cache_dir
    }

    fn effective_size(&self, kind: ArtworkKind, max_pixel_size: u32) -> u32 {
        let wanted = if max_pixel_size == 0 {
            // Unconstrained: the kind's `sourceSize.width` cap, scaled by the display
            // like Qt scales every provider request.
            let cap = kind.default_max_pixel_size() as f32;
            requested_width(kind, cap, self.inner.ctx.pixels_per_point())
        } else {
            max_pixel_size
        };
        // `load_texture` asserts against the backend limit; stay under it.
        let max_side = self.inner.ctx.input(|i| i.max_texture_side) as u32;
        wanted.min(max_side.max(1))
    }

    fn spawn_lookup(&self, key: Key, kind: ArtworkKind) {
        let weak = Arc::downgrade(&self.inner);
        let job_key = key.clone();
        let task: Task = Box::new(move || {
            if let Some(inner) = weak.upgrade() {
                Self::lookup(inner, job_key, kind);
            }
        });
        if let Err(e) = self.inner.decode_tx.send(task) {
            tracing::error!(target: LOG, "artwork decode pool is gone: {e}");
            self.finish(&key, None, kind);
        }
    }

    // --- worker side -------------------------------------------------------------------

    /// First half of `ArtworkResponse`'s worker lambda: disk cache lookups.
    fn lookup(inner: Arc<Inner>, key: Key, kind: ArtworkKind) {
        let requested = RequestedSize::width_only(key.max_pixel_size);
        let dir = &inner.config.cache_dir;
        let key_hex = cache_key_for(&key.url);
        let thumb_path = dir.join(format!("{key_hex}{}", requested.thumb_suffix()));

        let mut image: Option<DynamicImage> = None;
        if let Ok(bytes) = std::fs::read(&thumb_path) {
            image = decode_artwork(&bytes, requested)
                .inspect_err(|e| tracing::debug!(target: LOG, "stale thumb {thumb_path:?}: {e}"))
                .ok();
        }
        let thumbnail_hit = image.is_some();
        let (download_url, supports_thumbnail) = download_url_for(&key.url, requested);

        // Ask the Microsoft image service for a thumbnail instead of decoding its
        // full-resolution original, even when an old original is cached.
        if !thumbnail_hit && !supports_thumbnail {
            let original = dir.join(format!("{key_hex}.img"));
            if let Ok(bytes) = std::fs::read(&original) {
                image = decode_artwork(&bytes, requested)
                    .inspect_err(|e| tracing::debug!(target: LOG, "bad original {original:?}: {e}"))
                    .ok();
            }
        }

        if let Some(img) = image {
            if !thumbnail_hit {
                Self::save_thumbnail(&thumb_path, &img);
            }
            Self::upload(&inner, &key, kind, img);
            return;
        }

        // Miss: download on tokio, bounded like the QThreadPool, then decode back here.
        let weak = Arc::downgrade(&inner);
        let permits = Arc::clone(&inner.download_permits);
        let fetcher = Arc::clone(&inner.fetcher);
        let decode_tx = inner.decode_tx.clone();
        let runtime = inner.runtime.clone();
        drop(inner);
        runtime.spawn(async move {
            let _permit = match permits.acquire_owned().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let fetched = fetcher.fetch(&download_url).await;
            drop(_permit);
            let task: Task = Box::new(move || {
                let Some(inner) = weak.upgrade() else { return };
                match fetched {
                    Ok(bytes) => match decode_artwork(&bytes, requested) {
                        Ok(img) => {
                            Self::save_thumbnail(&thumb_path, &img);
                            Self::upload(&inner, &key, kind, img);
                        }
                        Err(e) => {
                            tracing::warn!(target: LOG, "{}: {e}", key.url);
                            Self::finish_inner(&inner, &key, None, kind);
                        }
                    },
                    Err(e) => {
                        tracing::warn!(target: LOG, "{}: {e}", download_url);
                        Self::finish_inner(&inner, &key, None, kind);
                    }
                }
            });
            if decode_tx.send(task).is_err() {
                tracing::error!(target: LOG, "artwork decode pool is gone");
            }
        });
    }

    /// `QSaveFile` + `QImage::save(PNG)`: write next to the target, then rename.
    fn save_thumbnail(path: &Path, img: &DynamicImage) {
        let mut png = Cursor::new(Vec::new());
        if let Err(e) = img.write_to(&mut png, ImageFormat::Png) {
            tracing::warn!(target: LOG, "cannot encode thumbnail {path:?}: {e}");
            return;
        }
        let tmp = path.with_extension(format!("thumb.{}.tmp", std::process::id()));
        let result =
            std::fs::write(&tmp, png.into_inner()).and_then(|()| std::fs::rename(&tmp, path));
        if let Err(e) = result {
            tracing::warn!(target: LOG, "cannot write thumbnail {path:?}: {e}");
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// `QQuickTextureFactory::textureFactoryForImage`: upload to egui off the UI thread.
    fn upload(inner: &Arc<Inner>, key: &Key, kind: ArtworkKind, img: DynamicImage) {
        let rgba = img.into_rgba8();
        let size = [rgba.width() as usize, rgba.height() as usize];
        let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
        let texture = inner.ctx.load_texture(
            format!("artwork/{}/{}", kind.name(), cache_key_for(&key.url)),
            color,
            egui::TextureOptions::LINEAR,
        );
        Self::finish_inner(inner, key, Some(texture), kind);
    }

    fn finish(&self, key: &Key, texture: Option<egui::TextureHandle>, kind: ArtworkKind) {
        Self::finish_inner(&self.inner, key, texture, kind);
    }

    /// `emit finished()`: publish the slot and ask eframe to redraw.
    fn finish_inner(
        inner: &Arc<Inner>,
        key: &Key,
        texture: Option<egui::TextureHandle>,
        kind: ArtworkKind,
    ) {
        let tick = inner.tick.fetch_add(1, Ordering::Relaxed);
        {
            let mut slots = inner.slots.lock();
            match texture {
                Some(t) => {
                    slots.insert_ready(key.clone(), t, tick, inner.config.texture_budget_bytes)
                }
                None => {
                    let now = Instant::now();
                    slots.map.insert(
                        key.clone(),
                        Slot::Failed(FailedEntry {
                            failed_at: now,
                            last_requested: now,
                        }),
                    );
                }
            }
        }
        tracing::trace!(target: LOG, "{} {} finished", kind.name(), key.url);
        inner.ctx.request_repaint();
    }
}

// ----------------------------------------------------------------------------- sha1

/// `QCryptographicHash::Sha1`, inline so `xcloud-app` needs no extra dependency for one
/// 20-byte digest (FIPS 180-1).
mod sha1 {
    pub fn digest(data: &[u8]) -> [u8; 20] {
        let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
        let bit_len = (data.len() as u64).wrapping_mul(8);
        let mut msg = data.to_vec();
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bit_len.to_be_bytes());

        let mut w = [0u32; 80];
        let (blocks, _) = msg.as_chunks::<64>();
        for chunk in blocks {
            let (words, _) = chunk.as_chunks::<4>();
            for (i, word) in words.iter().enumerate() {
                w[i] = u32::from_be_bytes(*word);
            }
            for i in 16..80 {
                w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
            }
            let [mut a, mut b, mut c, mut d, mut e] = h;
            for (i, &wi) in w.iter().enumerate() {
                let (f, k) = match i {
                    0..=19 => ((b & c) | (!b & d), 0x5A827999),
                    20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                    40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                    _ => (b ^ c ^ d, 0xCA62C1D6),
                };
                let temp = a
                    .rotate_left(5)
                    .wrapping_add(f)
                    .wrapping_add(e)
                    .wrapping_add(k)
                    .wrapping_add(wi);
                e = d;
                d = c;
                c = b.rotate_left(30);
                b = a;
                a = temp;
            }
            h[0] = h[0].wrapping_add(a);
            h[1] = h[1].wrapping_add(b);
            h[2] = h[2].wrapping_add(c);
            h[3] = h[3].wrapping_add(d);
            h[4] = h[4].wrapping_add(e);
        }
        let mut out = [0u8; 20];
        for (i, v) in h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
        }
        out
    }
}

// ----------------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    /// Port of `tst_artworkprovider.cpp` plus tests for the Rust-only pieces (LRU, fetcher
    /// abstraction, disk round trip, SHA-1).
    struct FakeFetcher {
        images: Mutex<HashMap<String, Vec<u8>>>,
        hits: AtomicUsize,
    }

    impl FakeFetcher {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                images: Mutex::new(HashMap::new()),
                hits: AtomicUsize::new(0),
            })
        }
        fn add(&self, url: &str, bytes: Vec<u8>) {
            self.images.lock().insert(url.to_owned(), bytes);
        }
        fn hits(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    impl ArtworkFetcher for FakeFetcher {
        fn fetch(&self, url: &str) -> FetchFuture<'_> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            let found = self.images.lock().get(url).cloned();
            Box::pin(async move { found.ok_or_else(|| ArtworkError::Fetch("404".into())) })
        }
    }

    fn png(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(width, height, image::Rgb(rgb));
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(img)
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "xcloud-artwork-test-{}-{n}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn unique_url(host: &str, query: &str) -> String {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("https://{host}/{}-{n}{query}", std::process::id())
    }

    struct Harness {
        _rt: tokio::runtime::Runtime,
        cache: ArtworkCache,
        fetcher: Arc<FakeFetcher>,
        dir: PathBuf,
        keep_dir: bool,
    }

    impl Harness {
        fn new(name: &str, budget: usize) -> Self {
            let dir = temp_dir(name);
            Self::with_dir(dir, budget, FakeFetcher::new())
        }

        fn with_dir(dir: PathBuf, budget: usize, fetcher: Arc<FakeFetcher>) -> Self {
            Self::with_failure_policy(
                dir,
                budget,
                fetcher,
                DEFAULT_FAILURE_TTL,
                DEFAULT_FAILURE_IDLE,
            )
        }

        fn with_failure_policy(
            dir: PathBuf,
            budget: usize,
            fetcher: Arc<FakeFetcher>,
            failure_ttl: Duration,
            failure_idle: Duration,
        ) -> Self {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let config = ArtworkConfig {
                cache_dir: dir.clone(),
                texture_budget_bytes: budget,
                pool_size: 2,
                failure_ttl,
                failure_idle,
            };
            let cache = ArtworkCache::new(
                egui::Context::default(),
                rt.handle().clone(),
                fetcher.clone() as Arc<dyn ArtworkFetcher>,
                config,
            );
            Self {
                _rt: rt,
                cache,
                fetcher,
                dir,
                keep_dir: false,
            }
        }

        /// `QSignalSpy::wait(5000)`.
        fn wait(&self, urls: &[String], kind: ArtworkKind, size: u32) -> ArtworkState {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let state = self.cache.request(urls, kind, size);
                if !state.is_loading() {
                    return state;
                }
                assert!(
                    Instant::now() < deadline,
                    "artwork did not finish within 5 s"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        /// Waits until the fake fetcher has been hit `n` times in total.
        fn wait_hits(&self, n: usize) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.fetcher.hits() < n {
                assert!(
                    Instant::now() < deadline,
                    "expected {n} fetches, saw {}",
                    self.fetcher.hits()
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            if !self.keep_dir {
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }
    }

    // --- tst_artworkprovider.cpp ---------------------------------------------------------

    #[test]
    fn width_only_request_produces_thumbnail() {
        let h = Harness::new("width-only", DEFAULT_TEXTURE_BUDGET_BYTES);
        // A query inside the URL must survive; cache-only data avoids the network.
        let url = unique_url("example.invalid", "?format=jpg");
        let path = h.dir.join(format!("{}.img", cache_key_for(&url)));
        std::fs::write(&path, png(2400, 3600, [255, 0, 0])).unwrap();

        let state = h.wait(std::slice::from_ref(&url), ArtworkKind::Tile, 256);
        let texture = state.texture().expect("Artwork unavailable");
        assert_eq!(texture.size(), [256, 384]);

        let thumb = h.dir.join(format!("{}-256x-1.thumb", cache_key_for(&url)));
        let cached = ImageReader::open(&thumb)
            .unwrap()
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap();
        assert_eq!((cached.width(), cached.height()), (256, 384));
        assert_eq!(
            h.fetcher.hits(),
            0,
            "cache-only data must not hit the network"
        );
    }

    // --- thumbnail_size (decodeArtwork semantics) -----------------------------------------

    #[test]
    fn thumbnail_size_width_only_keeps_aspect() {
        assert_eq!(
            thumbnail_size((2400, 3600), RequestedSize::width_only(256)),
            (256, 384)
        );
        assert_eq!(
            thumbnail_size((1920, 1080), RequestedSize::width_only(800)),
            (800, 450)
        );
        // Odd aspect: height is round(h * w / src_w) = 33, then QSize::scaled's integer
        // arithmetic refits the width to that height (33 * 1000 / 333 = 99), as Qt does.
        assert_eq!(
            thumbnail_size((1000, 333), RequestedSize::width_only(100)),
            (99, 33)
        );
    }

    #[test]
    fn thumbnail_size_never_upscales() {
        assert_eq!(
            thumbnail_size((200, 300), RequestedSize::width_only(256)),
            (200, 300)
        );
        assert_eq!(
            thumbnail_size((256, 384), RequestedSize::width_only(256)),
            (256, 384)
        );
    }

    #[test]
    fn thumbnail_size_defaults_to_512_wide() {
        assert_eq!(
            thumbnail_size(
                (2048, 1024),
                RequestedSize {
                    width: -1,
                    height: -1
                }
            ),
            (512, 256)
        );
        assert_eq!(
            thumbnail_size(
                (2048, 1024),
                RequestedSize {
                    width: 0,
                    height: 0
                }
            ),
            (512, 256)
        );
    }

    #[test]
    fn thumbnail_size_both_dimensions_fit_inside() {
        // Explicit bounds: QSize::scaled(KeepAspectRatio) fits inside the box.
        assert_eq!(
            thumbnail_size(
                (2400, 3600),
                RequestedSize {
                    width: 256,
                    height: 256
                }
            ),
            (170, 256)
        );
        assert_eq!(
            thumbnail_size(
                (3600, 2400),
                RequestedSize {
                    width: 256,
                    height: 256
                }
            ),
            (256, 170)
        );
        assert_eq!(
            thumbnail_size((0, 0), RequestedSize::width_only(256)),
            (0, 0)
        );
    }

    // --- cache key / URL helpers -----------------------------------------------------------

    #[test]
    fn sha1_matches_known_vectors() {
        assert_eq!(
            cache_key_for(""),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            cache_key_for("abc"),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            cache_key_for("The quick brown fox jumps over the lazy dog"),
            "2fd4e1c67a2d28fced849ee1bb76e7391b93eb12"
        );
        // Multi-block (> 64 bytes) input.
        assert_eq!(
            cache_key_for("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        let long = "a".repeat(1000);
        assert_eq!(
            cache_key_for(&long),
            "291e9a6c66994949b57ba5e650361e98fc36b1ba"
        );
    }

    #[test]
    fn download_url_rewrites_width_only_for_microsoft_store() {
        let (url, thumb) = download_url_for(
            "https://store-images.s-microsoft.com/image/apps.1234?w=1200&format=jpg",
            RequestedSize::width_only(256),
        );
        assert!(thumb);
        assert_eq!(
            url,
            "https://store-images.s-microsoft.com/image/apps.1234?format=jpg&w=256"
        );

        let (url, thumb) = download_url_for(
            "https://store-images.s-microsoft.com/image/apps.1234",
            RequestedSize {
                width: -1,
                height: -1,
            },
        );
        assert!(thumb);
        assert_eq!(
            url,
            "https://store-images.s-microsoft.com/image/apps.1234?w=512"
        );

        let other = "https://example.invalid/a.png?w=9000";
        assert_eq!(
            download_url_for(other, RequestedSize::width_only(256)),
            (other.to_owned(), false)
        );
        assert_eq!(
            download_url_for("not a url", RequestedSize::width_only(256)),
            ("not a url".to_owned(), false)
        );
    }

    // --- RemoteImage.sourceSize.width and the device pixel ratio ---------------------------

    #[test]
    fn source_size_width_rounds_up_to_step_and_caps_in_logical_pixels() {
        // GameTile is 176 pt wide: ceil(176 / 32) * 32 = 192.
        assert_eq!(source_size_width(ArtworkKind::Tile, 176.0), 192);
        assert_eq!(source_size_width(ArtworkKind::Poster, 176.0), 192);
        assert_eq!(source_size_width(ArtworkKind::Tile, 192.0), 192);
        assert_eq!(source_size_width(ArtworkKind::Tile, 193.0), 224);
        assert_eq!(source_size_width(ArtworkKind::Tile, 1000.0), 256);
        // Heroes step by 64 and cap at 800.
        assert_eq!(source_size_width(ArtworkKind::Hero, 700.0), 704);
        assert_eq!(source_size_width(ArtworkKind::Hero, 1200.0), 800);
        assert_eq!(source_size_width(ArtworkKind::Hero, 0.0), 0);
        assert_eq!(source_size_width(ArtworkKind::Tile, -5.0), 0);
        assert_eq!(source_size_width(ArtworkKind::Tile, f32::NAN), 0);
    }

    #[test]
    fn requested_width_applies_display_scale_after_the_logical_cap() {
        // The cap is applied in logical pixels, then Qt scales by the DPR: a 176-pt tile on
        // a 2x display requests 384 device pixels, not min(256, 352) = 256.
        assert_eq!(requested_width(ArtworkKind::Tile, 176.0, 2.0), 384);
        assert_eq!(requested_width(ArtworkKind::Tile, 176.0, 1.0), 192);
        // A hero wider than the cap reaches 1600 device pixels on 2x.
        assert_eq!(requested_width(ArtworkKind::Hero, 1200.0, 2.0), 1600);
        assert_eq!(requested_width(ArtworkKind::Hero, 1200.0, 1.0), 800);
        // Fractional scale factors round like `QSize * qreal`.
        assert_eq!(requested_width(ArtworkKind::Tile, 176.0, 1.5), 288);
        assert_eq!(requested_width(ArtworkKind::Tile, 100.0, 1.25), 160);
        // Nonsense scale falls back to 1x.
        assert_eq!(requested_width(ArtworkKind::Tile, 176.0, 0.0), 192);
        assert_eq!(requested_width(ArtworkKind::Tile, 176.0, f32::NAN), 192);
    }

    #[test]
    fn unconstrained_request_scales_the_kind_cap_by_pixels_per_point() {
        let h = Harness::new("dpr-default", DEFAULT_TEXTURE_BUDGET_BYTES);
        // `egui::Context::default()` reports 1 pixel per point: the raw caps.
        assert_eq!(h.cache.effective_size(ArtworkKind::Tile, 0), 256);
        assert_eq!(h.cache.effective_size(ArtworkKind::Hero, 0), 800);
        // An explicit device-pixel width is taken as-is (only the GPU limit applies).
        assert_eq!(h.cache.effective_size(ArtworkKind::Tile, 384), 384);
        assert_eq!(h.cache.effective_size(ArtworkKind::Hero, 1600), 1600);
    }

    #[test]
    fn default_cache_directory_ends_with_artwork() {
        assert_eq!(
            default_cache_directory()
                .file_name()
                .and_then(|n| n.to_str()),
            Some("artwork")
        );
    }

    // --- pipeline with a fake fetcher -----------------------------------------------------

    #[test]
    fn download_decodes_and_writes_thumbnail_then_disk_round_trip() {
        let dir = temp_dir("round-trip");
        let fetcher = FakeFetcher::new();
        let url = unique_url("cdn.example.invalid", "/poster.png?x=1");
        fetcher.add(&url, png(600, 900, [0, 255, 0]));

        {
            let mut h =
                Harness::with_dir(dir.clone(), DEFAULT_TEXTURE_BUDGET_BYTES, fetcher.clone());
            h.keep_dir = true; // the second instance below reads the same directory
            let first = h
                .cache
                .request(std::slice::from_ref(&url), ArtworkKind::Poster, 256);
            assert!(first.is_loading());
            let state = h.wait(std::slice::from_ref(&url), ArtworkKind::Poster, 256);
            assert_eq!(state.texture().expect("ready").size(), [256, 384]);
            assert_eq!(fetcher.hits(), 1);
            // Re-requesting the same key does not refetch.
            assert!(
                h.cache
                    .request(std::slice::from_ref(&url), ArtworkKind::Poster, 256)
                    .is_ready()
            );
            assert_eq!(fetcher.hits(), 1);
            let thumb = dir.join(format!("{}-256x-1.thumb", cache_key_for(&url)));
            assert!(thumb.exists(), "thumbnail written to the disk cache");
        }

        // A fresh cache (new process) is served from disk: zero network hits.
        let h = Harness::with_dir(dir, DEFAULT_TEXTURE_BUDGET_BYTES, fetcher.clone());
        let state = h.wait(std::slice::from_ref(&url), ArtworkKind::Poster, 256);
        assert_eq!(state.texture().expect("ready from disk").size(), [256, 384]);
        assert_eq!(
            fetcher.hits(),
            1,
            "second instance must be served from the disk cache"
        );
    }

    #[test]
    fn microsoft_store_url_is_fetched_with_width_param() {
        let h = Harness::new("ms-store", DEFAULT_TEXTURE_BUDGET_BYTES);
        let url = unique_url("store-images.s-microsoft.com", "?w=1200&format=png");
        let (download, _) = download_url_for(&url, RequestedSize::width_only(256));
        h.fetcher.add(&download, png(256, 384, [1, 2, 3]));
        let state = h.wait(std::slice::from_ref(&url), ArtworkKind::Tile, 256);
        assert_eq!(state.texture().expect("ready").size(), [256, 384]);
        assert_eq!(h.fetcher.hits(), 1);
    }

    #[test]
    fn first_working_url_wins() {
        let h = Harness::new("fallback", DEFAULT_TEXTURE_BUDGET_BYTES);
        let missing = unique_url("cdn.example.invalid", "/missing.png");
        let good = unique_url("cdn.example.invalid", "/good.png");
        h.fetcher.add(&good, png(64, 64, [9, 9, 9]));
        let urls = vec![String::new(), missing.clone(), good.clone()];

        let state = h.wait(&urls, ArtworkKind::Tile, 64);
        assert_eq!(
            state.texture().expect("fallback candidate").size(),
            [64, 64]
        );
        assert!(!h.cache.is_resident(&missing, ArtworkKind::Tile, 64));
        assert!(h.cache.is_resident(&good, ArtworkKind::Tile, 64));
        // While the failure is remembered, later frames do not retry the first candidate.
        let hits = h.fetcher.hits();
        assert!(h.cache.request(&urls, ArtworkKind::Tile, 64).is_ready());
        assert_eq!(h.fetcher.hits(), hits);
    }

    // --- failure memory (RemoteImage retried whenever its delegate was re-created) ---------

    #[test]
    fn failed_candidate_is_retried_after_ttl_even_when_kept_on_screen() {
        // The failure is stamped on the worker before `wait` returns, so `start` below
        // trails it by scheduler latency. Keep the on-screen window far below the TTL so
        // a slow CI runner can't age the failure out mid-check.
        let ttl = Duration::from_millis(500);
        let on_screen = Duration::from_millis(50);
        let h = Harness::with_failure_policy(
            temp_dir("fail-ttl"),
            DEFAULT_TEXTURE_BUDGET_BYTES,
            FakeFetcher::new(),
            ttl,
            Duration::from_secs(3600),
        );
        let url = unique_url("cdn.example.invalid", "/flaky.png");
        assert!(
            h.wait(std::slice::from_ref(&url), ArtworkKind::Tile, 64)
                .is_failed()
        );
        assert_eq!(h.fetcher.hits(), 1);

        // Kept on screen: requested every few ms, still failed, no refetch before the TTL.
        let start = Instant::now();
        while start.elapsed() < on_screen {
            assert!(
                h.cache
                    .request(std::slice::from_ref(&url), ArtworkKind::Tile, 64)
                    .is_failed()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(h.fetcher.hits(), 1);

        // The CDN recovers; once the failure is older than the TTL the tile re-requests it.
        h.fetcher.add(&url, png(8, 8, [0, 0, 0]));
        while start.elapsed() < ttl {
            let _ = h
                .cache
                .request(std::slice::from_ref(&url), ArtworkKind::Tile, 64);
            std::thread::sleep(Duration::from_millis(5));
        }
        let state = h.wait(std::slice::from_ref(&url), ArtworkKind::Tile, 64);
        assert!(state.is_ready(), "transient failure recovered: {state:?}");
        assert_eq!(h.fetcher.hits(), 2);
    }

    #[test]
    fn failed_candidate_is_retried_after_its_tile_scrolled_away() {
        let idle = Duration::from_millis(60);
        let h = Harness::with_failure_policy(
            temp_dir("fail-idle"),
            DEFAULT_TEXTURE_BUDGET_BYTES,
            FakeFetcher::new(),
            Duration::from_secs(3600),
            idle,
        );
        let url = unique_url("cdn.example.invalid", "/offline.png");
        assert!(
            h.wait(std::slice::from_ref(&url), ArtworkKind::Tile, 64)
                .is_failed()
        );
        assert_eq!(h.fetcher.hits(), 1);

        // Visible the whole time: every request refreshes the failure, no refetch.
        let start = Instant::now();
        while start.elapsed() < idle * 2 {
            assert!(
                h.cache
                    .request(std::slice::from_ref(&url), ArtworkKind::Tile, 64)
                    .is_failed()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(h.fetcher.hits(), 1);

        // Scrolled out of view (nobody asks) for longer than `failure_idle`, then back: the
        // candidate is requested again, like a freshly created delegate.
        std::thread::sleep(idle + Duration::from_millis(20));
        assert!(
            h.cache
                .request(std::slice::from_ref(&url), ArtworkKind::Tile, 64)
                .is_loading()
        );
        h.wait_hits(2);
        // Still 404: fails again and is remembered again.
        assert!(
            h.wait(std::slice::from_ref(&url), ArtworkKind::Tile, 64)
                .is_failed()
        );
        assert_eq!(h.fetcher.hits(), 2);
    }

    #[test]
    fn ready_fallback_keeps_showing_while_first_candidate_retries() {
        let ttl = Duration::from_millis(60);
        let h = Harness::with_failure_policy(
            temp_dir("fail-fallback"),
            DEFAULT_TEXTURE_BUDGET_BYTES,
            FakeFetcher::new(),
            ttl,
            Duration::from_secs(3600),
        );
        let missing = unique_url("cdn.example.invalid", "/missing.png");
        let good = unique_url("cdn.example.invalid", "/good.png");
        h.fetcher.add(&good, png(16, 16, [7, 7, 7]));
        let urls = vec![missing.clone(), good.clone()];
        assert!(h.wait(&urls, ArtworkKind::Tile, 16).is_ready());
        assert_eq!(h.fetcher.hits(), 2);

        // After the TTL the first candidate is retried, but the tile never drops back to
        // the placeholder: the decoded second candidate is shown meanwhile.
        std::thread::sleep(ttl + Duration::from_millis(20));
        let state = h.cache.request(&urls, ArtworkKind::Tile, 16);
        assert!(state.is_ready(), "no placeholder flicker: {state:?}");
        h.wait_hits(3);
        // The retry was only for the first candidate; the second was not refetched.
        let state = h.wait(&urls, ArtworkKind::Tile, 16);
        assert!(state.is_ready());
        assert_eq!(h.fetcher.hits(), 3);
        assert!(!h.cache.is_resident(&missing, ArtworkKind::Tile, 16));
    }

    #[test]
    fn later_candidates_are_not_started_while_an_earlier_one_loads() {
        let h = Harness::new("no-prefetch", DEFAULT_TEXTURE_BUDGET_BYTES);
        let first = unique_url("cdn.example.invalid", "/first.png");
        let second = unique_url("cdn.example.invalid", "/second.png");
        h.fetcher.add(&first, png(8, 8, [1, 1, 1]));
        h.fetcher.add(&second, png(8, 8, [2, 2, 2]));
        let urls = vec![first.clone(), second.clone()];
        assert!(h.cache.request(&urls, ArtworkKind::Tile, 8).is_loading());
        assert!(h.wait(&urls, ArtworkKind::Tile, 8).is_ready());
        assert_eq!(
            h.fetcher.hits(),
            1,
            "RemoteImage requests one candidate at a time"
        );
        assert!(!h.cache.is_resident(&second, ArtworkKind::Tile, 8));
    }

    #[test]
    fn forget_failures_clears_every_remembered_failure() {
        let h = Harness::new("forget", DEFAULT_TEXTURE_BUDGET_BYTES);
        let a = unique_url("cdn.example.invalid", "/a.png");
        let b = unique_url("cdn.example.invalid", "/b.png");
        assert!(
            h.wait(std::slice::from_ref(&a), ArtworkKind::Tile, 8)
                .is_failed()
        );
        assert!(
            h.wait(std::slice::from_ref(&b), ArtworkKind::Tile, 8)
                .is_failed()
        );
        assert_eq!(h.fetcher.hits(), 2);
        h.fetcher.add(&a, png(8, 8, [1, 1, 1]));
        h.fetcher.add(&b, png(8, 8, [2, 2, 2]));
        h.cache.forget_failures();
        assert!(
            h.wait(std::slice::from_ref(&a), ArtworkKind::Tile, 8)
                .is_ready()
        );
        assert!(
            h.wait(std::slice::from_ref(&b), ArtworkKind::Tile, 8)
                .is_ready()
        );
        assert_eq!(h.fetcher.hits(), 4);
    }

    #[test]
    fn all_candidates_failing_reports_failed() {
        let h = Harness::new("all-fail", DEFAULT_TEXTURE_BUDGET_BYTES);
        let a = unique_url("cdn.example.invalid", "/a.png");
        let b = unique_url("cdn.example.invalid", "/b.png");
        h.fetcher.add(&b, b"not an image".to_vec());
        let state = h.wait(&[a.clone(), b.clone()], ArtworkKind::Tile, 64);
        assert!(state.is_failed());
        assert!(h.cache.request(&[], ArtworkKind::Tile, 64).is_failed());
        assert_eq!(h.cache.texture_count(), 0);

        // `retry` forgets the failure and the candidate is fetched again.
        h.fetcher.add(&a, png(8, 8, [0, 0, 0]));
        h.cache.retry(&a, ArtworkKind::Tile, 64);
        let state = h.wait(std::slice::from_ref(&a), ArtworkKind::Tile, 64);
        assert!(state.is_ready());
    }

    #[test]
    fn lru_evicts_least_recently_used_by_pixel_budget() {
        // Budget for exactly two 64x64 RGBA textures.
        let h = Harness::new("lru", 2 * 64 * 64 * 4);
        let urls: Vec<String> = (0..3)
            .map(|i| unique_url("cdn.example.invalid", &format!("/{i}.png")))
            .collect();
        for (i, url) in urls.iter().enumerate() {
            h.fetcher.add(url, png(64, 64, [i as u8, 0, 0]));
        }

        assert!(h.wait(&urls[0..1], ArtworkKind::Tile, 64).is_ready());
        assert!(h.wait(&urls[1..2], ArtworkKind::Tile, 64).is_ready());
        assert_eq!(h.cache.texture_count(), 2);
        assert_eq!(h.cache.texture_bytes(), 2 * 64 * 64 * 4);

        // Touch #0 so #1 becomes the least recently used, then load #2.
        assert!(
            h.cache
                .request(&urls[0..1], ArtworkKind::Tile, 64)
                .is_ready()
        );
        assert!(h.wait(&urls[2..3], ArtworkKind::Tile, 64).is_ready());

        assert_eq!(h.cache.texture_count(), 2);
        assert!(h.cache.texture_bytes() <= 2 * 64 * 64 * 4);
        assert!(h.cache.is_resident(&urls[0], ArtworkKind::Tile, 64));
        assert!(
            !h.cache.is_resident(&urls[1], ArtworkKind::Tile, 64),
            "LRU victim"
        );
        assert!(h.cache.is_resident(&urls[2], ArtworkKind::Tile, 64));

        // An evicted key is reloaded (from disk, no network) on the next request.
        let hits = h.fetcher.hits();
        assert!(
            h.cache
                .request(&urls[1..2], ArtworkKind::Tile, 64)
                .is_loading()
        );
        assert!(h.wait(&urls[1..2], ArtworkKind::Tile, 64).is_ready());
        assert_eq!(h.fetcher.hits(), hits);
    }

    #[test]
    fn oversized_single_texture_is_kept() {
        let h = Harness::new("oversized", 16);
        let url = unique_url("cdn.example.invalid", "/big.png");
        h.fetcher.add(&url, png(32, 32, [0, 0, 1]));
        assert!(h.wait(&[url], ArtworkKind::Tile, 32).is_ready());
        assert_eq!(h.cache.texture_count(), 1);
    }

    #[test]
    fn kind_default_size_and_key_distinct_per_size() {
        let h = Harness::new("sizes", DEFAULT_TEXTURE_BUDGET_BYTES);
        let url = unique_url("cdn.example.invalid", "/hero.png");
        h.fetcher.add(&url, png(1600, 900, [0, 0, 1]));
        let hero = h.wait(std::slice::from_ref(&url), ArtworkKind::Hero, 0);
        assert_eq!(hero.texture().expect("hero").size(), [800, 450]);
        let tile = h.wait(std::slice::from_ref(&url), ArtworkKind::Tile, 0);
        assert_eq!(tile.texture().expect("tile").size(), [256, 144]);
        assert_eq!(h.cache.texture_count(), 2);
        assert!(
            h.dir
                .join(format!("{}-800x-1.thumb", cache_key_for(&url)))
                .exists()
        );
        assert!(
            h.dir
                .join(format!("{}-256x-1.thumb", cache_key_for(&url)))
                .exists()
        );
    }
}
