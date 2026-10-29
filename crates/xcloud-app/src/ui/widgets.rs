//! Artwork painting (the port of qt/qml/RemoteImage.qml) plus the text, gradient and
//! glow paint helpers the screens share. Interactive components live in
//! [`crate::ui::kit`], icons in [`crate::ui::icons`].
//!
//! Artwork reaches the widgets through [`ArtworkSource`]: the UI asks for the first
//! usable candidate URL once per frame and paints whatever state comes back. The
//! implementation (download, disk cache, decode pool, textures) lives in
//! `crate::artwork`.

use egui::epaint::text::{LayoutJob, TextWrapping};
use egui::{
    Color32, CornerRadius, FontId, Mesh, Pos2, Rect, Sense, Shape, Stroke, Ui, Vec2, pos2, vec2,
};

use crate::theme;

// ----------------------------------------------------------------------------- artwork

pub use crate::artwork::{ArtworkKind, ArtworkState};

/// Image provider as the widgets see it (`image://artwork/...` in QML).
///
/// `urls` are the candidates in priority order (`RemoteImage.urls`); empty strings
/// are skipped. `max_px` is the requested width in device pixels (`sourceSize.width`).
/// `crate::artwork::ArtworkCache` implements it below.
pub trait ArtworkSource {
    fn request(&self, urls: &[String], kind: ArtworkKind, max_px: u32) -> ArtworkState;
}

impl ArtworkSource for crate::artwork::ArtworkCache {
    fn request(&self, urls: &[String], kind: ArtworkKind, max_px: u32) -> ArtworkState {
        crate::artwork::ArtworkCache::request(self, urls, kind, max_px)
    }
}

/// A source without any images (smoke mode, tests): everything is `Failed`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoArtwork;

impl ArtworkSource for NoArtwork {
    fn request(&self, _urls: &[String], _kind: ArtworkKind, _max_px: u32) -> ArtworkState {
        ArtworkState::Failed
    }
}

/// `RemoteImage.sourceSize.width` in *logical* pixels: the layout width rounded up to
/// the next 32 (tile) / 64 (hero) step and capped at 256 / 800. Qt applies the device
/// pixel ratio after this; pass the result through [`artwork_request_width`] (or call
/// that directly) before asking an [`ArtworkSource`] for it.
pub fn artwork_source_width(kind: ArtworkKind, logical_width: f32) -> u32 {
    crate::artwork::source_size_width(kind, logical_width)
}

/// The width an [`ArtworkSource`] is asked for, in device pixels: `sourceSize.width`
/// computed from the layout width in logical pixels, then scaled by the display like
/// `QQuickImageBase::loadPixmap` does (`sourceSize * devicePixelRatio`). A 176-pt tile
/// on a 2x display requests 384 px, a wide hero 1600 px, so decoded resolution and the
/// `<sha1>-384x-1.thumb` cache names match the Qt app.
pub fn artwork_request_width(kind: ArtworkKind, logical_width: f32, pixels_per_point: f32) -> u32 {
    crate::artwork::requested_width(kind, logical_width, pixels_per_point)
}

/// `RemoteImage.candidates`: the non-empty URLs.
pub fn artwork_candidates(urls: &[String]) -> Vec<String> {
    urls.iter().filter(|u| !u.is_empty()).cloned().collect()
}

/// Port of `RemoteImage.qml`: paints the artwork for `urls` into `rect`, aspect-fill
/// cropped and rounded by `radius`, or the gradient placeholder with the diamond
/// glyph while loading / missing. Returns `true` when a real image was drawn.
pub fn remote_image(
    ui: &mut Ui,
    source: &dyn ArtworkSource,
    urls: &[String],
    kind: ArtworkKind,
    rect: Rect,
    radius: f32,
) -> bool {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return false;
    }
    let candidates = artwork_candidates(urls);
    let state = if candidates.is_empty() {
        ArtworkState::Failed
    } else {
        // Round and cap in logical pixels, then apply the display scale (not the
        // other way round): `sourceSize.width` is logical, Qt scales the request.
        let max_px = artwork_request_width(kind, rect.width(), ui.pixels_per_point());
        source.request(&candidates, kind, max_px)
    };
    let corner = CornerRadius::same(radius as u8);
    match state {
        ArtworkState::Ready(texture) => {
            let uv = aspect_fill_uv(texture.size_vec2(), rect.size());
            egui::Image::from_texture(&texture)
                .uv(uv)
                .corner_radius(corner)
                .paint_at(ui, rect);
            true
        }
        ArtworkState::Loading | ArtworkState::Failed => {
            paint_artwork_placeholder(ui, rect, corner);
            false
        }
    }
}

/// [`remote_image`] painted at `opacity` (0..1); draws nothing while loading.
/// Returns `true` when a real image was drawn.
pub fn remote_image_opacity(
    ui: &Ui,
    source: &dyn ArtworkSource,
    urls: &[String],
    kind: ArtworkKind,
    rect: Rect,
    radius: f32,
    opacity: f32,
) -> bool {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return false;
    }
    let candidates = artwork_candidates(urls);
    if candidates.is_empty() {
        return false;
    }
    let max_px = artwork_request_width(kind, rect.width(), ui.pixels_per_point());
    match source.request(&candidates, kind, max_px) {
        ArtworkState::Ready(texture) => {
            let uv = aspect_fill_uv(texture.size_vec2(), rect.size());
            egui::Image::from_texture(&texture)
                .uv(uv)
                .corner_radius(CornerRadius::same(radius as u8))
                .tint(Color32::from_white_alpha(
                    (opacity.clamp(0.0, 1.0) * 255.0) as u8,
                ))
                .paint_at(ui, rect);
            true
        }
        _ => false,
    }
}

/// `Image.PreserveAspectCrop`: the centred UV sub-rectangle of a `texture`-sized
/// image that fills `target` without distortion.
pub fn aspect_fill_uv(texture: Vec2, target: Vec2) -> Rect {
    if texture.x <= 0.0 || texture.y <= 0.0 || target.x <= 0.0 || target.y <= 0.0 {
        return Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0));
    }
    let scale = (target.x / texture.x).max(target.y / texture.y);
    let visible = vec2(
        target.x / (texture.x * scale),
        target.y / (texture.y * scale),
    );
    let min = pos2((1.0 - visible.x) * 0.5, (1.0 - visible.y) * 0.5);
    Rect::from_min_size(min, visible)
}

fn paint_artwork_placeholder(ui: &Ui, rect: Rect, corner: CornerRadius) {
    let painter = ui.painter();
    // Rounded clip for the gradient: fill the rounded rect with the bottom colour,
    // then the gradient mesh inside it.
    let top = Color32::from_rgb(0x2b, 0x35, 0x2a);
    let bottom = Color32::from_rgb(0x1b, 0x21, 0x1b);
    painter.rect_filled(rect, corner, bottom);
    let inset = rect.shrink(radius_inset(corner));
    vertical_gradient(ui, inset, top, bottom);
    let glyph_color = Color32::from_rgb(0x53, 0x63, 0x4b);
    let c = rect.center();
    // 36x36 outline rotated 45° = a diamond with 25.46 px half-diagonal.
    let half = 36.0 * std::f32::consts::FRAC_1_SQRT_2;
    let diamond = vec![
        pos2(c.x, c.y - half),
        pos2(c.x + half, c.y),
        pos2(c.x, c.y + half),
        pos2(c.x - half, c.y),
    ];
    painter.add(Shape::closed_line(diamond, Stroke::new(1.0, glyph_color)));
    painter.rect_filled(
        Rect::from_center_size(c, vec2(14.0, 14.0)),
        CornerRadius::same(3),
        glyph_color,
    );
}

fn radius_inset(corner: CornerRadius) -> f32 {
    // Enough that the square gradient mesh stays inside the rounded fill.
    f32::from(corner.nw.max(corner.ne).max(corner.sw).max(corner.se)) * 0.3
}

/// Paints a top-to-bottom gradient over `rect`.
pub fn vertical_gradient(ui: &Ui, rect: Rect, top: Color32, bottom: Color32) {
    let mut mesh = Mesh::default();
    mesh.colored_vertex(rect.left_top(), top);
    mesh.colored_vertex(rect.right_top(), top);
    mesh.colored_vertex(rect.right_bottom(), bottom);
    mesh.colored_vertex(rect.left_bottom(), bottom);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    ui.painter().add(Shape::mesh(mesh));
}

/// Paints a left-to-right gradient through `stops` (position 0..1, colour).
pub fn horizontal_gradient(ui: &Ui, rect: Rect, stops: &[(f32, Color32)]) {
    if stops.len() < 2 {
        return;
    }
    let mut mesh = Mesh::default();
    for (pos, color) in stops {
        let x = rect.left() + rect.width() * pos.clamp(0.0, 1.0);
        mesh.colored_vertex(pos2(x, rect.top()), *color);
        mesh.colored_vertex(pos2(x, rect.bottom()), *color);
    }
    for i in 0..(stops.len() as u32 - 1) {
        let a = i * 2;
        mesh.add_triangle(a, a + 1, a + 3);
        mesh.add_triangle(a, a + 3, a + 2);
    }
    ui.painter().add(Shape::mesh(mesh));
}

/// Lays out `text` in `font`, wrapped to `width` and clamped to `max_rows` rows with
/// an ellipsis.
pub fn layout_font(
    ui: &Ui,
    text: &str,
    font: FontId,
    color: Color32,
    width: f32,
    max_rows: usize,
) -> std::sync::Arc<egui::Galley> {
    let mut job = LayoutJob::simple(text.to_owned(), font, color, width);
    job.wrap = TextWrapping {
        max_width: width,
        max_rows,
        break_anywhere: false,
        overflow_character: Some('…'),
    };
    ui.painter().layout_job(job)
}

/// Paints one line of `text` at `pos` (top-left), elided at `max_width`.
pub fn paint_elided(
    ui: &Ui,
    pos: Pos2,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) -> Rect {
    if text.is_empty() || max_width <= 0.0 {
        return Rect::from_min_size(pos, Vec2::ZERO);
    }
    let galley = layout_font(ui, text, font, color, max_width, 1);
    let rect = Rect::from_min_size(pos, galley.size());
    ui.painter().galley(pos, galley, color);
    rect
}

/// Paints wrapped `text` at `pos` (top-left), at most `max_rows` rows. Returns the
/// painted rect.
pub fn paint_wrapped(
    ui: &Ui,
    pos: Pos2,
    text: &str,
    font: FontId,
    color: Color32,
    width: f32,
    max_rows: usize,
) -> Rect {
    if text.is_empty() {
        return Rect::from_min_size(pos, Vec2::ZERO);
    }
    let galley = layout_font(ui, text, font, color, width, max_rows);
    let rect = Rect::from_min_size(pos, galley.size());
    ui.painter().galley(pos, galley, color);
    rect
}

/// A top-to-bottom gradient whose bottom corners are rounded by `radius` (scrims
/// over rounded artwork). Approximates the rounding with a fan per corner.
pub fn vertical_gradient_rounded(ui: &Ui, rect: Rect, top: Color32, bottom: Color32, radius: f32) {
    let r = radius
        .min(rect.height() / 2.0)
        .min(rect.width() / 2.0)
        .max(0.0);
    let body = Rect::from_min_max(rect.min, pos2(rect.right(), rect.bottom() - r));
    let lerp = |y: f32| {
        let t = ((y - rect.top()) / rect.height()).clamp(0.0, 1.0);
        crate::ui::kit::mix(top, bottom, t)
    };
    vertical_gradient(ui, body, top, lerp(body.bottom()));
    if r <= 0.0 {
        return;
    }
    // Bottom strip with rounded corners, coloured by its vertical position.
    let mut mesh = Mesh::default();
    let steps = 8;
    let y0 = rect.bottom() - r;
    let mut ring: Vec<Pos2> = Vec::new();
    ring.push(pos2(rect.left(), y0));
    for i in 0..=steps {
        let a = std::f32::consts::PI - std::f32::consts::FRAC_PI_2 * (i as f32 / steps as f32);
        ring.push(pos2(rect.left() + r + r * a.cos(), y0 + r * a.sin()));
    }
    for i in 0..=steps {
        let a =
            std::f32::consts::FRAC_PI_2 - std::f32::consts::FRAC_PI_2 * (i as f32 / steps as f32);
        ring.push(pos2(rect.right() - r + r * a.cos(), y0 + r * a.sin()));
    }
    ring.push(pos2(rect.right(), y0));
    let center = pos2(rect.center().x, y0);
    mesh.colored_vertex(center, lerp(center.y));
    for p in &ring {
        mesh.colored_vertex(*p, lerp(p.y));
    }
    for i in 1..ring.len() as u32 {
        mesh.add_triangle(0, i, i + 1);
    }
    ui.painter().add(Shape::mesh(mesh));
}

/// A smooth radial glow: an elliptical gradient mesh from `color` at `center`
/// (at `strength` alpha) fading to transparent at `radius`, with an ease-out falloff
/// so there is no visible banding.
pub fn ambient_glow(ui: &Ui, center: Pos2, radius: Vec2, color: Color32) {
    radial_glow(ui, center, radius, color, 0.22);
}

/// [`ambient_glow`] with an explicit peak opacity.
pub fn radial_glow(ui: &Ui, center: Pos2, radius: Vec2, color: Color32, strength: f32) {
    let segments = 72;
    let rings = 10;
    let mut mesh = Mesh::default();
    let alpha_at = |t: f32| {
        // Smooth falloff: 1 at the centre, 0 at the edge.
        let f = (1.0 - t).clamp(0.0, 1.0);
        (f * f * (3.0 - 2.0 * f)) * strength
    };
    mesh.colored_vertex(center, color.gamma_multiply(alpha_at(0.0)));
    for ring in 1..=rings {
        let t = ring as f32 / rings as f32;
        let c = color.gamma_multiply(alpha_at(t));
        for i in 0..segments {
            let a = i as f32 / segments as f32 * std::f32::consts::TAU;
            mesh.colored_vertex(center + vec2(a.cos() * radius.x, a.sin() * radius.y) * t, c);
        }
    }
    let seg = segments as u32;
    for i in 0..seg {
        mesh.add_triangle(0, 1 + i, 1 + (i + 1) % seg);
    }
    for ring in 1..rings as u32 {
        let a0 = 1 + (ring - 1) * seg;
        let b0 = 1 + ring * seg;
        for i in 0..seg {
            let j = (i + 1) % seg;
            mesh.add_triangle(a0 + i, b0 + i, b0 + j);
            mesh.add_triangle(a0 + i, b0 + j, a0 + j);
        }
    }
    ui.painter().add(Shape::mesh(mesh));
}

/// `Rectangle { height: 1; color: Theme.border }` spanning the available width.
pub fn hairline(ui: &mut Ui) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, theme::BORDER);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_width_matches_remote_image() {
        assert_eq!(artwork_source_width(ArtworkKind::Tile, 176.0), 192);
        assert_eq!(artwork_source_width(ArtworkKind::Tile, 352.0), 256);
        assert_eq!(artwork_source_width(ArtworkKind::Tile, 1000.0), 256);
        assert_eq!(artwork_source_width(ArtworkKind::Hero, 700.0), 704);
        assert_eq!(artwork_source_width(ArtworkKind::Hero, 1800.0), 800);
        assert_eq!(artwork_source_width(ArtworkKind::Hero, 0.0), 0);
    }

    #[test]
    fn request_width_rounds_in_logical_pixels_then_scales() {
        // 176-pt GameTile: ceil(176/32)*32 = 192 logical → 384 device px at 2x.
        assert_eq!(artwork_request_width(ArtworkKind::Tile, 176.0, 2.0), 384);
        assert_eq!(artwork_request_width(ArtworkKind::Tile, 176.0, 1.0), 192);
        // The 256 / 800 caps are logical: a 2x display may ask for 512 / 1600.
        assert_eq!(artwork_request_width(ArtworkKind::Tile, 1000.0, 2.0), 512);
        assert_eq!(artwork_request_width(ArtworkKind::Hero, 1200.0, 2.0), 1600);
        assert_eq!(artwork_request_width(ArtworkKind::Hero, 1200.0, 1.0), 800);
        // Not what the old device-pixel rounding produced (256 for a 2x tile).
        assert_ne!(
            artwork_request_width(ArtworkKind::Tile, 176.0, 2.0),
            artwork_source_width(ArtworkKind::Tile, 176.0 * 2.0)
        );
    }

    /// Records every `max_px` the widgets ask for.
    #[derive(Default)]
    struct RecordingSource(std::sync::Mutex<Vec<(ArtworkKind, u32)>>);

    impl ArtworkSource for RecordingSource {
        fn request(&self, _urls: &[String], kind: ArtworkKind, max_px: u32) -> ArtworkState {
            if let Ok(mut log) = self.0.lock() {
                log.push((kind, max_px));
            }
            ArtworkState::Loading
        }
    }

    #[test]
    fn remote_image_requests_the_qt_width_on_a_2x_display() {
        use egui_kittest::Harness;

        let source = RecordingSource::default();
        let urls = vec!["https://img/poster.jpg".to_owned()];
        let mut harness = Harness::builder()
            .with_size(vec2(400.0, 400.0))
            .with_pixels_per_point(2.0)
            .allow_missing_glyphs()
            .build_ui(|ui| {
                let tile = Rect::from_min_size(pos2(10.0, 10.0), vec2(176.0, 176.0));
                remote_image(ui, &source, &urls, ArtworkKind::Tile, tile, theme::RADIUS);
                let hero = Rect::from_min_size(pos2(10.0, 260.0), vec2(1200.0, 100.0));
                remote_image(ui, &source, &urls, ArtworkKind::Hero, hero, 16.0);
            });
        harness.step();
        drop(harness);
        // `remote_image` asks the source once per frame (the cache dedupes); the harness
        // runs several frames/passes, so compare the distinct requests in order.
        let mut requests = source.0.into_inner().unwrap();
        requests.dedup();
        let mut distinct: Vec<(ArtworkKind, u32)> = Vec::new();
        for request in requests {
            if !distinct.contains(&request) {
                distinct.push(request);
            }
        }
        assert_eq!(
            distinct,
            vec![(ArtworkKind::Tile, 384), (ArtworkKind::Hero, 1600)],
            "sourceSize.width is rounded/capped in logical pixels, then scaled by 2x"
        );
    }

    #[test]
    fn candidates_skip_empty() {
        let urls = vec![String::new(), "a".into(), String::new(), "b".into()];
        assert_eq!(
            artwork_candidates(&urls),
            vec!["a".to_owned(), "b".to_owned()]
        );
    }

    #[test]
    fn aspect_fill_crops_the_long_axis() {
        // Wide texture into a tall target: crop left/right.
        let uv = aspect_fill_uv(vec2(400.0, 200.0), vec2(100.0, 200.0));
        assert!((uv.min.y - 0.0).abs() < 1e-5 && (uv.max.y - 1.0).abs() < 1e-5);
        assert!((uv.min.x - 0.375).abs() < 1e-5 && (uv.max.x - 0.625).abs() < 1e-5);
        // Same aspect: whole texture.
        let uv = aspect_fill_uv(vec2(200.0, 100.0), vec2(400.0, 200.0));
        assert_eq!(uv, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)));
        // Degenerate sizes do not panic.
        let uv = aspect_fill_uv(Vec2::ZERO, vec2(1.0, 1.0));
        assert_eq!(uv, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)));
    }

    #[test]
    fn no_artwork_is_failed() {
        assert!(
            NoArtwork
                .request(&["x".into()], ArtworkKind::Tile, 256)
                .is_failed()
        );
    }
}
