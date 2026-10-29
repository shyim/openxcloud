//! The component kit of the design system: buttons, pills, segmented tabs, toggles,
//! radio cards, navigation rows, section headers and the square game tile.
//!
//! Every widget paints itself with the painter (no egui frames), reports an
//! accessible role and name, shows a pointing-hand cursor when clickable, and draws
//! the lime focus ring when it has keyboard focus.

use egui::{
    Align2, Color32, CornerRadius, FontId, Id, Rect, Response, Role, Sense, Stroke, StrokeKind, Ui,
    Vec2, WidgetInfo, pos2, vec2,
};
use xcloud_proto::models::LibraryCard;

use crate::theme::{self, Weight, font_w};
use crate::ui::icons::{self, Icon};
use crate::ui::widgets::{self, ArtworkKind, ArtworkSource};

// ----------------------------------------------------------------------------- helpers

fn pointer(response: &Response) {
    if response.hovered() && response.enabled() {
        response.ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
    }
}

fn focus_ring(ui: &Ui, response: &Response, rect: Rect, radius: CornerRadius) {
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(2.0),
            radius,
            theme::focus_stroke(2.0),
            StrokeKind::Outside,
        );
    }
}

/// Smoothly animated 0..1 hover amount for `id`.
pub fn hover_t(ui: &Ui, id: Id, hovered: bool) -> f32 {
    ui.ctx()
        .animate_bool_with_time(id.with("hover"), hovered, 0.12)
}

/// Linear blend of two colours.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Color32::from_rgba_premultiplied(
        l(a.r(), b.r()),
        l(a.g(), b.g()),
        l(a.b(), b.b()),
        l(a.a(), b.a()),
    )
}

fn text_width(ui: &Ui, text: &str, font: FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), font, Color32::WHITE)
        .size()
        .x
}

/// Runs `add` in a child `Ui` placed at `rect` without moving the parent's cursor
/// (unlike `Ui::put`, which advances layout and shifts everything after it).
pub fn place<R>(ui: &mut Ui, rect: Rect, add: impl FnOnce(&mut Ui) -> R) -> R {
    place_with(
        ui,
        rect,
        egui::Layout::left_to_right(egui::Align::Center),
        add,
    )
}

/// [`place`] with an explicit layout. Use a top-aligned layout for content that
/// should start at `rect.top()` (a centred layout vertically centres containers such
/// as scroll areas inside `rect`).
pub fn place_with<R>(
    ui: &mut Ui,
    rect: Rect,
    layout: egui::Layout,
    add: impl FnOnce(&mut Ui) -> R,
) -> R {
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout));
    add(&mut child)
}

// ----------------------------------------------------------------------------- buttons

/// Visual weight of a [`Button`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Lime fill: the one action that matters (Play, Sign in).
    Primary,
    /// White fill, dark text: strong secondary emphasis.
    Light,
    /// Neutral surface fill.
    Secondary,
    /// No fill until hovered.
    Ghost,
    /// Translucent glass, for use over artwork.
    Glass,
}

/// A pill button with an optional leading icon.
pub struct Button<'a> {
    text: &'a str,
    icon: Option<Icon>,
    tone: Tone,
    height: f32,
    min_width: f32,
    font_px: f32,
    enabled: bool,
}

impl<'a> Button<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            text,
            icon: None,
            tone: Tone::Secondary,
            height: 40.0,
            min_width: 0.0,
            font_px: 14.0,
            enabled: true,
        }
    }
    pub fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }
    pub fn primary(self) -> Self {
        self.tone(Tone::Primary)
    }
    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }
    pub fn height(mut self, height: f32) -> Self {
        self.height = height;
        self.font_px = (height * 0.36).clamp(12.0, 18.0);
        self
    }
    pub fn min_width(mut self, width: f32) -> Self {
        self.min_width = width;
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let font = font_w(self.font_px, Weight::Semibold);
        let icon_size = self.font_px * 1.2;
        let gap = if self.icon.is_some() && !self.text.is_empty() {
            8.0
        } else {
            0.0
        };
        let label_w = if self.text.is_empty() {
            0.0
        } else {
            text_width(ui, self.text, font.clone())
        };
        let content_w = label_w + gap + if self.icon.is_some() { icon_size } else { 0.0 };
        let pad = if self.text.is_empty() {
            (self.height - icon_size) / 2.0
        } else {
            self.height * 0.5
        };
        let width = (content_w + pad * 2.0).max(self.min_width).max(self.height);
        let sense = if self.enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        let (rect, response) = ui.allocate_exact_size(vec2(width, self.height), sense);
        let name = self.text;
        response.widget_info(|| WidgetInfo::labeled(Role::Button, self.enabled, name));
        pointer(&response);
        if !ui.is_rect_visible(rect) {
            return response;
        }
        let t = hover_t(ui, response.id, response.hovered());
        let pressed = response.is_pointer_button_down_on();
        let (base, hover, fg) = match self.tone {
            Tone::Primary => (theme::ACCENT, theme::ACCENT_HOVER, theme::ON_ACCENT),
            Tone::Light => (theme::TEXT_PRIMARY, Color32::WHITE, theme::ON_WHITE),
            Tone::Secondary => (
                theme::SURFACE_HIGH,
                theme::SURFACE_HIGHER,
                theme::TEXT_PRIMARY,
            ),
            Tone::Ghost => (
                Color32::TRANSPARENT,
                theme::SURFACE_HIGH,
                theme::TEXT_PRIMARY,
            ),
            Tone::Glass => (
                theme::SURFACE_GLASS,
                Color32::from_rgba_premultiplied(0x30, 0x33, 0x38, 0xd0),
                theme::TEXT_PRIMARY,
            ),
        };
        let mut fill = mix(base, hover, t);
        if pressed {
            fill = match self.tone {
                Tone::Primary => theme::ACCENT_PRESSED,
                _ => mix(fill, Color32::BLACK, 0.12),
            };
        }
        let (fill, fg) = if self.enabled {
            (fill, fg)
        } else {
            (theme::SURFACE_HIGH, theme::TEXT_TERTIARY)
        };
        let radius = CornerRadius::same((self.height / 2.0) as u8);
        ui.painter().rect_filled(rect, radius, fill);
        focus_ring(ui, &response, rect, radius);
        let mut x = rect.center().x - content_w / 2.0;
        if let Some(icon) = self.icon {
            icons::paint(
                ui.painter(),
                icons::centered(pos2(x + icon_size / 2.0, rect.center().y), icon_size),
                icon,
                fg,
            );
            x += icon_size + gap;
        }
        if !self.text.is_empty() {
            ui.painter().text(
                pos2(x, rect.center().y),
                Align2::LEFT_CENTER,
                self.text,
                font,
                fg,
            );
        }
        response
    }
}

/// A round icon-only button. `label` is the tooltip and accessible name.
pub fn icon_button(ui: &mut Ui, icon: Icon, size: f32, label: &str) -> Response {
    icon_button_toned(ui, icon, size, label, Tone::Ghost, false)
}

/// [`icon_button`] with an explicit tone; `active` draws the selected state.
pub fn icon_button_toned(
    ui: &mut Ui,
    icon: Icon,
    size: f32,
    label: &str,
    tone: Tone,
    active: bool,
) -> Response {
    icon_button_tinted(ui, icon, size, label, tone, active, None)
}

/// The favourite heart colour.
pub const FAVORITE: Color32 = Color32::from_rgb(0xff, 0x5a, 0x6e);

/// [`icon_button_toned`] with the icon drawn in `tint` instead of the text colours.
pub fn icon_button_tinted(
    ui: &mut Ui,
    icon: Icon,
    size: f32,
    label: &str,
    tone: Tone,
    active: bool,
    tint: Option<Color32>,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, label));
    pointer(&response);
    if ui.is_rect_visible(rect) {
        let t = hover_t(ui, response.id, response.hovered());
        let base = match tone {
            Tone::Glass => theme::SURFACE_GLASS,
            Tone::Secondary => theme::SURFACE_HIGH,
            _ => Color32::TRANSPARENT,
        };
        let mut fill = mix(base, theme::SURFACE_HIGHER, t);
        if active {
            fill = theme::SURFACE_HIGHER;
        }
        if response.is_pointer_button_down_on() {
            fill = mix(fill, Color32::BLACK, 0.15);
        }
        let radius = CornerRadius::same((size / 2.0) as u8);
        ui.painter().rect_filled(rect, radius, fill);
        focus_ring(ui, &response, rect, radius);
        let color = mix(
            theme::TEXT_SECONDARY,
            theme::TEXT_PRIMARY,
            t.max(active as u8 as f32),
        );
        let color = tint.unwrap_or(color);
        icons::paint(ui.painter(), rect.shrink(size * 0.24), icon, color);
    }
    if label.is_empty() {
        response
    } else {
        response.on_hover_text(label)
    }
}

// ----------------------------------------------------------------------------- selection

/// A segmented pill row (Xbox-style tabs). Returns the newly selected index.
pub fn segmented(ui: &mut Ui, id: Id, items: &[&str], selected: usize) -> Option<usize> {
    let font = font_w(14.0, Weight::Semibold);
    let height = 44.0;
    let pad = 18.0;
    let widths: Vec<f32> = items
        .iter()
        .map(|t| text_width(ui, t, font.clone()) + pad * 2.0)
        .collect();
    let total = widths.iter().sum::<f32>() + 8.0;
    let (outer, _) = ui.allocate_exact_size(vec2(total, height), Sense::hover());
    ui.painter()
        .rect_filled(outer, CornerRadius::same(22), theme::SURFACE);
    let mut x = outer.left() + 4.0;
    let mut changed = None;
    for (i, (item, w)) in items.iter().zip(&widths).enumerate() {
        let rect = Rect::from_min_size(pos2(x, outer.top() + 4.0), vec2(*w, height - 8.0));
        let response = ui.interact(rect, id.with(i), Sense::click());
        response.widget_info(|| WidgetInfo::selected(Role::Tab, true, i == selected, *item));
        pointer(&response);
        let t = hover_t(ui, response.id, response.hovered());
        let radius = CornerRadius::same(18);
        if i == selected {
            ui.painter()
                .rect_filled(rect, radius, theme::SURFACE_HIGHER);
        } else if t > 0.0 {
            ui.painter().rect_filled(
                rect,
                radius,
                mix(Color32::TRANSPARENT, theme::SURFACE_HIGH, t),
            );
        }
        focus_ring(ui, &response, rect, radius);
        let color = if i == selected {
            theme::TEXT_PRIMARY
        } else {
            mix(theme::TEXT_SECONDARY, theme::TEXT_PRIMARY, t)
        };
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            *item,
            font.clone(),
            color,
        );
        if response.clicked() && i != selected {
            changed = Some(i);
        }
        x += w;
    }
    changed
}

/// A compact pill (filter/sort/chip). `active` tints it.
pub fn pill(
    ui: &mut Ui,
    icon: Option<Icon>,
    text: &str,
    trailing: Option<Icon>,
    active: bool,
) -> Response {
    let font = font_w(14.0, Weight::Medium);
    let height = 40.0;
    let icon_size = 18.0;
    let tw = if text.is_empty() {
        0.0
    } else {
        text_width(ui, text, font.clone())
    };
    let mut width = 28.0 + tw;
    if icon.is_some() {
        width += icon_size + if text.is_empty() { 0.0 } else { 8.0 };
    }
    if trailing.is_some() {
        width += 14.0 + 6.0;
    }
    let width = width.max(height);
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, text));
    pointer(&response);
    if ui.is_rect_visible(rect) {
        let t = hover_t(ui, response.id, response.hovered());
        let base = if active {
            theme::SURFACE_HIGHER
        } else {
            theme::SURFACE
        };
        let fill = mix(base, theme::SURFACE_HIGHER, t);
        let radius = CornerRadius::same(20);
        ui.painter().rect_filled(rect, radius, fill);
        if active {
            ui.painter().rect_stroke(
                rect,
                radius,
                Stroke::new(1.0, theme::TEXT_SECONDARY.gamma_multiply(0.5)),
                StrokeKind::Inside,
            );
        }
        focus_ring(ui, &response, rect, radius);
        let fg = theme::TEXT_PRIMARY;
        let mut x = if text.is_empty() && trailing.is_none() {
            rect.center().x - icon_size / 2.0
        } else {
            rect.left() + 14.0
        };
        if let Some(icon) = icon {
            icons::paint(
                ui.painter(),
                icons::square(pos2(x, rect.center().y - icon_size / 2.0), icon_size),
                icon,
                fg,
            );
            x += icon_size + 8.0;
        }
        if !text.is_empty() {
            ui.painter().text(
                pos2(x, rect.center().y),
                Align2::LEFT_CENTER,
                text,
                font,
                fg,
            );
        }
        if let Some(icon) = trailing {
            icons::paint(
                ui.painter(),
                icons::centered(pos2(rect.right() - 20.0, rect.center().y), 14.0),
                icon,
                theme::TEXT_SECONDARY,
            );
        }
    }
    response
}

/// An iOS-style switch. Returns the response; `value` is toggled on click.
pub fn toggle(ui: &mut Ui, value: &mut bool, label: &str) -> Response {
    let size = vec2(46.0, 26.0);
    let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
    response.widget_info(|| WidgetInfo::selected(Role::Switch, true, *value, label));
    pointer(&response);
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    let t = ui
        .ctx()
        .animate_bool_with_time(response.id.with("on"), *value, 0.14);
    let radius = CornerRadius::same(13);
    let fill = mix(theme::SURFACE_HIGHER, theme::ACCENT, t);
    ui.painter().rect_filled(rect, radius, fill);
    focus_ring(ui, &response, rect, radius);
    let knob_x = rect.left() + 13.0 + (rect.width() - 26.0) * t;
    let knob = mix(theme::TEXT_PRIMARY, theme::ON_ACCENT, t);
    ui.painter()
        .circle_filled(pos2(knob_x, rect.center().y), 9.5, knob);
    response
}

/// A full-width selectable card with a title, an optional subtitle and a radio dot
/// on the right (the Xbox settings resolution list).
pub fn radio_card(ui: &mut Ui, selected: bool, title: &str, subtitle: &str) -> Response {
    let height = if subtitle.is_empty() { 52.0 } else { 66.0 };
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    response.widget_info(|| WidgetInfo::selected(Role::RadioButton, true, selected, title));
    pointer(&response);
    if ui.is_rect_visible(rect) {
        let t = hover_t(ui, response.id, response.hovered());
        let radius = CornerRadius::same(12);
        let fill = mix(
            theme::SURFACE,
            theme::SURFACE_HIGH,
            t.max(if selected { 0.6 } else { 0.0 }),
        );
        ui.painter().rect_filled(rect, radius, fill);
        if selected {
            ui.painter().rect_stroke(
                rect,
                radius,
                Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.55)),
                StrokeKind::Inside,
            );
        }
        focus_ring(ui, &response, rect, radius);
        let left = rect.left() + 20.0;
        if subtitle.is_empty() {
            ui.painter().text(
                pos2(left, rect.center().y),
                Align2::LEFT_CENTER,
                title,
                font_w(15.0, Weight::Medium),
                theme::TEXT_PRIMARY,
            );
        } else {
            ui.painter().text(
                pos2(left, rect.center().y - 10.0),
                Align2::LEFT_CENTER,
                title,
                font_w(15.0, Weight::Medium),
                theme::TEXT_PRIMARY,
            );
            ui.painter().text(
                pos2(left, rect.center().y + 11.0),
                Align2::LEFT_CENTER,
                subtitle,
                theme::font(13.0),
                theme::TEXT_SECONDARY,
            );
        }
        let c = pos2(rect.right() - 28.0, rect.center().y);
        if selected {
            ui.painter().circle_filled(c, 10.0, theme::ACCENT);
            ui.painter().circle_filled(c, 4.0, theme::ON_ACCENT);
        } else {
            ui.painter()
                .circle_stroke(c, 9.0, Stroke::new(1.5, theme::TEXT_SECONDARY));
        }
    }
    response
}

/// A labelled settings row with a control on the right. `control` draws the control
/// right-aligned and returns whatever the caller needs.
pub fn setting_row<R>(
    ui: &mut Ui,
    title: &str,
    subtitle: &str,
    control: impl FnOnce(&mut Ui) -> R,
) -> R {
    let height = if subtitle.is_empty() { 52.0 } else { 64.0 };
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let text_y = if subtitle.is_empty() {
        rect.center().y
    } else {
        rect.center().y - 10.0
    };
    ui.painter().text(
        pos2(rect.left(), text_y),
        Align2::LEFT_CENTER,
        title,
        font_w(15.0, Weight::Medium),
        theme::TEXT_PRIMARY,
    );
    if !subtitle.is_empty() {
        ui.painter().text(
            pos2(rect.left(), rect.center().y + 11.0),
            Align2::LEFT_CENTER,
            subtitle,
            theme::font(13.0),
            theme::TEXT_SECONDARY,
        );
    }
    let control_rect = Rect::from_min_max(
        pos2(rect.left() + rect.width() * 0.45, rect.top()),
        rect.right_bottom(),
    );
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(control_rect)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
        control,
    )
    .inner
}

// ----------------------------------------------------------------------------- navigation

/// A sidebar navigation row: icon + label, white-ish selected fill. When `compact`
/// only the icon is drawn (collapsed sidebar).
pub fn nav_item(ui: &mut Ui, icon: Icon, label: &str, selected: bool, compact: bool) -> Response {
    let height = 52.0;
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    response.widget_info(|| WidgetInfo::selected(Role::Tab, true, selected, label));
    pointer(&response);
    if ui.is_rect_visible(rect) {
        let t = hover_t(ui, response.id, response.hovered());
        let radius = CornerRadius::same(12);
        let fill = if selected {
            theme::SURFACE_HIGHER
        } else {
            mix(Color32::TRANSPARENT, theme::SURFACE_HIGH, t)
        };
        ui.painter().rect_filled(rect, radius, fill);
        focus_ring(ui, &response, rect, radius);
        let fg = if selected {
            theme::TEXT_PRIMARY
        } else {
            mix(theme::TEXT_SECONDARY, theme::TEXT_PRIMARY, t)
        };
        let icon_center = if compact {
            rect.center()
        } else {
            pos2(rect.left() + 26.0, rect.center().y)
        };
        icons::paint(ui.painter(), icons::centered(icon_center, 22.0), icon, fg);
        if !compact {
            ui.painter().text(
                pos2(rect.left() + 52.0, rect.center().y),
                Align2::LEFT_CENTER,
                label,
                font_w(
                    16.0,
                    if selected {
                        Weight::Semibold
                    } else {
                        Weight::Medium
                    },
                ),
                fg,
            );
        }
    }
    if compact {
        response.on_hover_text(label)
    } else {
        response
    }
}

/// A rounded card background spanning `rect`.
pub fn card(ui: &Ui, rect: Rect) {
    // Slightly translucent so the window backdrop glows through.
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(theme::RADIUS_LARGE as u8),
        theme::SURFACE.gamma_multiply(0.88),
    );
}

/// A section title ("Jump back in") with an optional trailing link. Returns `true`
/// when the link was clicked.
pub fn section_header(ui: &mut Ui, title: &str, link: Option<&str>) -> bool {
    let height = 44.0;
    let (rect, heading) =
        ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    heading.widget_info(|| WidgetInfo::labeled(Role::Heading, true, title));
    ui.painter().text(
        pos2(rect.left(), rect.center().y),
        Align2::LEFT_CENTER,
        title,
        font_w(24.0, Weight::Semibold),
        theme::TEXT_PRIMARY,
    );
    let Some(link) = link else {
        return false;
    };
    let font = font_w(14.0, Weight::Medium);
    let w = text_width(ui, link, font.clone()) + 22.0;
    let link_rect = Rect::from_min_max(
        pos2(rect.right() - w, rect.center().y - 16.0),
        pos2(rect.right(), rect.center().y + 16.0),
    );
    let response = ui.interact(
        link_rect,
        ui.scope_id().with(("section_link", title)),
        Sense::click(),
    );
    response.widget_info(|| WidgetInfo::labeled(Role::Link, true, link));
    pointer(&response);
    let t = hover_t(ui, response.id, response.hovered());
    let color = mix(theme::TEXT_SECONDARY, theme::TEXT_PRIMARY, t);
    ui.painter().text(
        pos2(link_rect.left(), link_rect.center().y),
        Align2::LEFT_CENTER,
        link,
        font,
        color,
    );
    icons::paint(
        ui.painter(),
        icons::centered(pos2(link_rect.right() - 7.0, link_rect.center().y), 14.0),
        Icon::ChevronRight,
        color,
    );
    response.clicked()
}

/// Carousel page dots. Returns a clicked index.
pub fn page_dots(ui: &mut Ui, id: Id, count: usize, current: usize) -> Option<usize> {
    if count < 2 {
        return None;
    }
    let dot = 8.0;
    let active_w = 20.0;
    let gap = 8.0;
    let total = (count as f32 - 1.0) * (dot + gap) + active_w;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::hover());
    let mut x = rect.center().x - total / 2.0;
    let mut clicked = None;
    for i in 0..count {
        let w = if i == current { active_w } else { dot };
        let r = Rect::from_min_size(pos2(x, rect.center().y - dot / 2.0), vec2(w, dot));
        let response = ui.interact(r.expand(4.0), id.with(i), Sense::click());
        pointer(&response);
        let color = if i == current {
            theme::TEXT_PRIMARY
        } else if response.hovered() {
            theme::TEXT_SECONDARY
        } else {
            theme::TEXT_TERTIARY
        };
        ui.painter().rect_filled(r, CornerRadius::same(4), color);
        if response.clicked() {
            clicked = Some(i);
        }
        x += w + gap;
    }
    clicked
}

/// A small rounded badge ("RECENT", "4 players") drawn at `pos` (top-left).
pub fn badge(ui: &Ui, pos: egui::Pos2, text: &str, fill: Color32, fg: Color32) -> Rect {
    let font = font_w(10.5, Weight::Bold);
    let w = text_width(ui, text, font.clone()) + 12.0;
    let rect = Rect::from_min_size(pos, vec2(w, 20.0));
    ui.painter().rect_filled(rect, CornerRadius::same(5), fill);
    ui.painter()
        .text(rect.center(), Align2::CENTER_CENTER, text, font, fg);
    rect
}

// ----------------------------------------------------------------------------- game tiles

/// The URLs for square box art, best first.
pub fn box_art_urls(card: &LibraryCard) -> [String; 3] {
    [
        card.art_url.clone(),
        card.poster_image_url.clone(),
        card.hero_image_url.clone(),
    ]
}

/// The URLs for wide hero art, best first.
pub fn hero_urls(card: &LibraryCard) -> [String; 3] {
    [
        card.hero_image_url.clone(),
        card.gallery_image_urls.first().cloned().unwrap_or_default(),
        card.art_url.clone(),
    ]
}

/// A square box-art tile with a hover lift, play affordance and an optional
/// caption below. `size` is the square edge; the response covers the whole tile.
/// What a click on a [`game_tile`] asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TileClick {
    /// The tile was clicked: open the game's page.
    pub open: bool,
    /// The play disc was clicked: start the game.
    pub play: bool,
    /// The heart was clicked: toggle the favourite.
    pub favorite: bool,
}

/// Accessible label of a tile's play disc.
pub fn play_label(name: &str) -> String {
    format!("Play {name}")
}

/// Accessible label of a favourite toggle.
pub fn favorite_label(name: &str, is_favorite: bool) -> String {
    if is_favorite {
        format!("Remove {name} from favourites")
    } else {
        format!("Add {name} to favourites")
    }
}

/// `favorite` is the game's favourite state; `None` hides the heart.
pub fn game_tile(
    ui: &mut Ui,
    card: &LibraryCard,
    size: f32,
    caption: bool,
    favorite: Option<bool>,
    artwork: &dyn ArtworkSource,
) -> TileClick {
    let caption_h = if caption { 50.0 } else { 0.0 };
    let (rect, response) = ui.allocate_exact_size(vec2(size, size + caption_h), Sense::click());
    // The play disc is its own hit target on top of the tile.
    let disc = 50.0;
    let disc_rect = Rect::from_center_size(
        Rect::from_min_size(rect.min, Vec2::splat(size)).center(),
        Vec2::splat(disc),
    );
    let play = ui.interact(disc_rect, response.id.with("play"), Sense::click());
    let tile_name = if card.name.is_empty() {
        "Game"
    } else {
        card.name.as_str()
    };
    play.widget_info(|| WidgetInfo::labeled(Role::Button, true, play_label(tile_name)));
    pointer(&play);
    // The heart sits in the top-right corner, also its own hit target.
    let heart_rect = Rect::from_min_size(rect.min + vec2(size - 44.0, 8.0), Vec2::splat(36.0));
    let heart = favorite.map(|is_favorite| {
        let r = ui.interact(heart_rect, response.id.with("favorite"), Sense::click());
        r.widget_info(|| {
            WidgetInfo::labeled(Role::Button, true, favorite_label(tile_name, is_favorite))
        });
        pointer(&r);
        (r, is_favorite)
    });
    let click = TileClick {
        open: response.clicked(),
        play: play.clicked(),
        favorite: heart.as_ref().is_some_and(|(r, _)| r.clicked()),
    };
    let name = if card.name.is_empty() {
        "Game"
    } else {
        card.name.as_str()
    };
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, name));
    pointer(&response);
    if !ui.is_rect_visible(rect) {
        return click;
    }
    let t = hover_t(
        ui,
        response.id,
        response.hovered() || response.has_focus() || play.hovered(),
    );
    let art = Rect::from_min_size(rect.min, Vec2::splat(size));
    let lifted = art.expand(t * 4.0).translate(vec2(0.0, -t * 2.0));
    let radius = CornerRadius::same(10);
    ui.painter()
        .rect_filled(lifted, radius, theme::SURFACE_HIGH);
    widgets::remote_image(
        ui,
        artwork,
        &box_art_urls(card),
        ArtworkKind::Tile,
        lifted,
        10.0,
    );
    if t > 0.0 {
        // Darken slightly and show a play disc.
        ui.painter()
            .rect_filled(lifted, radius, Color32::from_black_alpha((70.0 * t) as u8));
        let c = lifted.center();
        let grow = if play.hovered() { 3.0 } else { 0.0 };
        let fill = if play.hovered() {
            theme::ACCENT_HOVER
        } else {
            theme::ACCENT
        };
        ui.painter()
            .circle_filled(c, disc / 2.0 - 2.0 + grow, fill.gamma_multiply(t));
        icons::paint(
            ui.painter(),
            icons::centered(c + vec2(1.5, 0.0), 20.0),
            Icon::Play,
            theme::ON_ACCENT.gamma_multiply(t),
        );
        ui.painter().rect_stroke(
            lifted,
            radius,
            Stroke::new(2.0, Color32::WHITE.gamma_multiply(0.85 * t)),
            StrokeKind::Outside,
        );
    }
    if let Some((heart_response, is_favorite)) = &heart {
        let show = t.max(if *is_favorite { 1.0 } else { 0.0 });
        if show > 0.01 {
            let r = heart_rect.translate(vec2(t * 4.0, -t * 6.0));
            let hovered = heart_response.hovered();
            let fill = if hovered {
                Color32::from_black_alpha(200)
            } else {
                Color32::from_black_alpha(140)
            };
            ui.painter()
                .circle_filled(r.center(), 16.0, fill.gamma_multiply(show));
            let (icon, color) = if *is_favorite {
                (Icon::HeartFilled, FAVORITE)
            } else {
                (Icon::Heart, theme::TEXT_PRIMARY)
            };
            icons::paint(
                ui.painter(),
                icons::centered(r.center() + vec2(0.0, 1.0), 18.0),
                icon,
                color.gamma_multiply(show),
            );
        }
    }
    if !caption && t > 0.01 {
        // Grids without captions reveal the name on hover.
        let scrim = Rect::from_min_max(
            pos2(lifted.left(), lifted.bottom() - lifted.height() * 0.42),
            lifted.max,
        );
        widgets::vertical_gradient_rounded(
            ui,
            scrim,
            Color32::TRANSPARENT,
            Color32::from_black_alpha((220.0 * t) as u8),
            10.0,
        );
        widgets::paint_elided(
            ui,
            pos2(lifted.left() + 12.0, lifted.bottom() - 30.0),
            name,
            font_w(14.0, Weight::Semibold),
            theme::TEXT_PRIMARY.gamma_multiply(t),
            lifted.width() - 24.0,
        );
    }
    if caption {
        let y = art.bottom() + 12.0;
        widgets::paint_elided(
            ui,
            pos2(art.left() + 2.0, y),
            name,
            font_w(14.0, Weight::Semibold),
            theme::TEXT_PRIMARY,
            size - 4.0,
        );
        widgets::paint_elided(
            ui,
            pos2(art.left() + 2.0, y + 19.0),
            &card.publisher_name,
            theme::font(12.5),
            theme::TEXT_SECONDARY,
            size - 4.0,
        );
    }
    click
}

/// Featured-card art: the titled hero (logo baked in) first.
pub fn featured_urls(card: &LibraryCard) -> [String; 3] {
    [
        card.titled_hero_url.clone(),
        card.hero_image_url.clone(),
        card.art_url.clone(),
    ]
}

/// A wide hero card with a bottom scrim, title and subtitle (the Xbox "featured"
/// carousel cards).
pub fn hero_card(
    ui: &mut Ui,
    rect: Rect,
    card: &LibraryCard,
    subtitle: &str,
    artwork: &dyn ArtworkSource,
) -> Response {
    let response = ui.interact(
        rect,
        ui.scope_id().with(("hero", &card.title_id)),
        Sense::click(),
    );
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, card.name.as_str()));
    pointer(&response);
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let t = hover_t(ui, response.id, response.hovered() || response.has_focus());
    let radius = CornerRadius::same(theme::RADIUS_LARGE as u8);
    ui.painter().rect_filled(rect, radius, theme::HERO_FILL);
    widgets::remote_image(
        ui,
        artwork,
        &hero_urls(card),
        ArtworkKind::Hero,
        rect,
        theme::RADIUS_LARGE,
    );
    let scrim = Rect::from_min_max(
        pos2(rect.left(), rect.bottom() - rect.height() * 0.55),
        rect.max,
    );
    widgets::vertical_gradient_rounded(
        ui,
        scrim,
        Color32::TRANSPARENT,
        Color32::from_black_alpha(230),
        theme::RADIUS_LARGE,
    );
    if t > 0.0 {
        ui.painter().rect_stroke(
            rect,
            radius,
            Stroke::new(2.0, Color32::WHITE.gamma_multiply(0.85 * t)),
            StrokeKind::Outside,
        );
    }
    let left = rect.left() + 26.0;
    let title_font = font_w((rect.height() * 0.085).clamp(20.0, 30.0), Weight::Bold);
    let sub_font = theme::font((rect.height() * 0.055).clamp(13.0, 17.0));
    let sub_y = rect.bottom() - 26.0;
    if !subtitle.is_empty() {
        widgets::paint_elided(
            ui,
            pos2(left, sub_y - sub_font.size),
            subtitle,
            sub_font.clone(),
            theme::HERO_BODY_TEXT,
            rect.width() - 52.0,
        );
    }
    let title_bottom = if subtitle.is_empty() {
        sub_y
    } else {
        sub_y - sub_font.size - 8.0
    };
    widgets::paint_elided(
        ui,
        pos2(left, title_bottom - title_font.size - 2.0),
        &card.name,
        title_font,
        theme::TEXT_PRIMARY,
        rect.width() - 52.0,
    );
    response
}

/// A compact list row: thumbnail, title and publisher (sidebar "Recent games").
pub fn game_row(ui: &mut Ui, card: &LibraryCard, artwork: &dyn ArtworkSource) -> Response {
    let height = 64.0;
    let (rect, response) =
        ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, card.name.as_str()));
    pointer(&response);
    if ui.is_rect_visible(rect) {
        let t = hover_t(ui, response.id, response.hovered());
        if t > 0.0 {
            ui.painter().rect_filled(
                rect.expand2(vec2(8.0, 0.0)),
                CornerRadius::same(10),
                mix(Color32::TRANSPARENT, theme::SURFACE_HIGH, t),
            );
        }
        let thumb = Rect::from_min_size(pos2(rect.left(), rect.top() + 6.0), vec2(52.0, 52.0));
        ui.painter()
            .rect_filled(thumb, CornerRadius::same(8), theme::SURFACE_HIGH);
        widgets::remote_image(
            ui,
            artwork,
            &box_art_urls(card),
            ArtworkKind::Tile,
            thumb,
            8.0,
        );
        let x = thumb.right() + 14.0;
        let w = rect.right() - x - 4.0;
        widgets::paint_elided(
            ui,
            pos2(x, rect.center().y - 19.0),
            &card.name,
            font_w(14.5, Weight::Medium),
            theme::TEXT_PRIMARY,
            w,
        );
        widgets::paint_elided(
            ui,
            pos2(x, rect.center().y + 2.0),
            &card.publisher_name,
            theme::font(13.0),
            theme::TEXT_SECONDARY,
            w,
        );
    }
    response
}
