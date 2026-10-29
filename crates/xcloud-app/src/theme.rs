//! The OpenXCloud design system: colours, typography, metrics and the egui style.
//!
//! The look is a dark, graphite "console dashboard": a near-black canvas, cards one
//! step lighter, white for selection and emphasis, and the OpenXCloud lime only where an
//! action should pull the eye (Play, primary buttons, focus rings, the logo). Game
//! artwork supplies the colour; the chrome stays neutral.
//!
//! Typography uses the system UI font at real weights: on macOS the variable
//! `SFNS.ttf` is registered four times with different `wght` coordinates
//! ([`Weight`]); elsewhere every weight falls back to egui's bundled font.
//!
//! The constant names of the QML port (`BACKGROUND`, `SURFACE`, `ACCENT`, …) are
//! kept so the stream view and HUD pick up the new palette unchanged.

use std::collections::BTreeMap;
use std::sync::Arc;

use egui::epaint::text::{FontData, FontDefinitions, FontFamily, FontTweak, VariationCoords};
use egui::{Color32, CornerRadius, FontId, Shadow, Stroke, TextStyle, Visuals};

// ----------------------------------------------------------------------------- colours

/// Window canvas.
pub const BACKGROUND: Color32 = Color32::from_rgb(0x0c, 0x0d, 0x0f);
/// Cards, sidebar panels, inputs.
pub const SURFACE: Color32 = Color32::from_rgb(0x17, 0x19, 0x1c);
/// Hovered cards and controls; neutral buttons.
pub const SURFACE_HIGH: Color32 = Color32::from_rgb(0x21, 0x24, 0x28);
/// Selected navigation entries and pressed controls.
pub const SURFACE_HIGHER: Color32 = Color32::from_rgb(0x2b, 0x2f, 0x34);
/// Floating glass (over artwork): round icons, overlay pills.
pub const SURFACE_GLASS: Color32 = Color32::from_rgba_premultiplied(0x10, 0x11, 0x13, 0xb8);
/// Modal sheets (settings).
pub const SHEET: Color32 = Color32::from_rgb(0x1b, 0x1d, 0x21);
/// Settings category column.
pub const SHEET_SIDE: Color32 = Color32::from_rgb(0x15, 0x17, 0x1a);
/// Hairlines and card outlines.
pub const BORDER: Color32 = Color32::from_rgb(0x26, 0x29, 0x2e);
/// The OpenXCloud lime: Play, primary actions, focus.
pub const ACCENT: Color32 = Color32::from_rgb(0xb8, 0xf0, 0x5a);
/// Primary text.
pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(0xf4, 0xf5, 0xf7);
/// Secondary text (publishers, captions).
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(0xa1, 0xa6, 0xad);
/// Tertiary text (hints, disabled).
pub const TEXT_TERTIARY: Color32 = Color32::from_rgb(0x6c, 0x71, 0x78);
pub const WARNING: Color32 = Color32::from_rgb(0xf2, 0xc0, 0x6b);
pub const DANGER: Color32 = Color32::from_rgb(0xff, 0x7a, 0x6e);
/// Online / success dot.
pub const SUCCESS: Color32 = Color32::from_rgb(0x3d, 0xd6, 0x8c);

/// Text on an accent fill.
pub const ON_ACCENT: Color32 = Color32::from_rgb(0x0e, 0x14, 0x05);
/// Text on a white (selected) fill.
pub const ON_WHITE: Color32 = Color32::from_rgb(0x0c, 0x0d, 0x0f);
/// Sidebar cards.
pub const SIDEBAR: Color32 = SURFACE;
/// Selected sidebar entry.
pub const SIDEBAR_SELECTED: Color32 = SURFACE_HIGHER;
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0xc9, 0xf7, 0x7c);
pub const ACCENT_PRESSED: Color32 = Color32::from_rgb(0x9c, 0xd4, 0x40);
/// Fill behind hero artwork while it loads.
pub const HERO_FILL: Color32 = Color32::from_rgb(0x14, 0x1a, 0x14);
/// Body copy over hero artwork.
pub const HERO_BODY_TEXT: Color32 = Color32::from_rgb(0xd6, 0xd9, 0xde);
/// The ambient glow at the top of the browse pages (a dark lime).
pub const GLOW: Color32 = Color32::from_rgb(0x1d, 0x2b, 0x14);
/// Scrim over the window behind modal sheets.
pub const SCRIM: Color32 = Color32::from_rgba_premultiplied(0, 0, 0, 0xb0);

// ----------------------------------------------------------------------------- metrics

/// Default corner radius for cards and tiles.
pub const RADIUS: f32 = 12.0;
/// Large surfaces (sidebar cards, hero cards, sheets).
pub const RADIUS_LARGE: f32 = 16.0;
/// Pills (search, segmented controls, chips).
pub const RADIUS_PILL: f32 = 999.0;
pub const SPACING: f32 = 20.0;
/// Page content margin.
pub const MARGIN: f32 = 28.0;
/// Square box-art tile edge for shelves.
pub const TILE_WIDTH: f32 = 196.0;
/// Shelf row height (square tile + caption space).
pub const TILE_HEIGHT: f32 = 196.0;
pub const SHELF_TITLE_HEIGHT: f32 = 48.0;
/// Top bar height.
pub const HEADER_HEIGHT: f32 = 60.0;
/// Expanded and collapsed sidebar widths.
pub const SIDEBAR_WIDTH: f32 = 300.0;
pub const SIDEBAR_COLLAPSED_WIDTH: f32 = 76.0;

// ----------------------------------------------------------------------------- fonts

pub const FONT_TINY: f32 = 11.0;
pub const FONT_SMALL: f32 = 13.0;
pub const FONT_BODY: f32 = 15.0;
pub const FONT_TITLE: f32 = 23.0;
pub const FONT_HERO: f32 = 40.0;

/// Named text style for [`FONT_HERO`].
pub const HERO_STYLE: &str = "hero";
/// Named text style for [`FONT_TINY`].
pub const TINY_STYLE: &str = "tiny";

/// Font weights available through [`font_w`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Weight {
    Regular,
    Medium,
    Semibold,
    Bold,
}

impl Weight {
    pub const ALL: [Weight; 4] = [
        Weight::Regular,
        Weight::Medium,
        Weight::Semibold,
        Weight::Bold,
    ];

    /// The `wght` axis value for the system variable font.
    pub fn wght(self) -> f32 {
        match self {
            Weight::Regular => 400.0,
            Weight::Medium => 510.0,
            Weight::Semibold => 600.0,
            Weight::Bold => 700.0,
        }
    }

    /// The egui font family carrying this weight.
    pub fn family(self) -> FontFamily {
        match self {
            Weight::Regular => FontFamily::Proportional,
            Weight::Medium => FontFamily::Name("medium".into()),
            Weight::Semibold => FontFamily::Name("semibold".into()),
            Weight::Bold => FontFamily::Name("bold".into()),
        }
    }
}

/// A regular-weight UI font of `px` logical pixels.
pub fn font(px: f32) -> FontId {
    FontId::proportional(px)
}

/// A UI font of `px` logical pixels at `weight`.
pub fn font_w(px: f32, weight: Weight) -> FontId {
    FontId::new(px, weight.family())
}

/// A monospace font of `px` logical pixels (device code display).
pub fn mono(px: f32) -> FontId {
    FontId::monospace(px)
}

pub fn hero_style() -> TextStyle {
    TextStyle::Name(HERO_STYLE.into())
}

pub fn tiny_style() -> TextStyle {
    TextStyle::Name(TINY_STYLE.into())
}

/// 1 px card outline.
pub fn border_stroke() -> Stroke {
    Stroke::new(1.0, BORDER)
}

/// Keyboard focus ring.
pub fn focus_stroke(width: f32) -> Stroke {
    Stroke::new(width, ACCENT)
}

/// [`RADIUS`] as an egui corner radius.
pub fn corner_radius() -> CornerRadius {
    CornerRadius::same(RADIUS as u8)
}

/// Parses `#RRGGBB` / `#AARRGGBB` into a `Color32`. Returns `None` for anything else.
pub fn qml_color(literal: &str) -> Option<Color32> {
    let hex = literal.strip_prefix('#')?;
    let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    match hex.len() {
        6 => Some(Color32::from_rgb(byte(0)?, byte(2)?, byte(4)?)),
        8 => Some(Color32::from_rgba_unmultiplied(
            byte(2)?,
            byte(4)?,
            byte(6)?,
            byte(0)?,
        )),
        _ => None,
    }
}

/// The dark [`Visuals`] of the design system.
pub fn visuals() -> Visuals {
    let mut v = Visuals::dark();
    let radius = corner_radius();

    v.override_text_color = Some(TEXT_PRIMARY);
    v.weak_text_color = Some(TEXT_SECONDARY);
    v.panel_fill = BACKGROUND;
    v.window_fill = SHEET;
    v.window_stroke = border_stroke();
    v.window_corner_radius = CornerRadius::same(RADIUS_LARGE as u8);
    v.window_shadow = Shadow {
        offset: [0, 12],
        blur: 40,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    v.popup_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(120),
    };
    v.menu_corner_radius = radius;
    v.faint_bg_color = SURFACE;
    v.extreme_bg_color = SURFACE;
    v.text_edit_bg_color = Some(SURFACE_HIGH);
    v.code_bg_color = SURFACE_HIGH;
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = WARNING;
    v.error_fg_color = DANGER;
    v.selection.bg_fill = ACCENT.gamma_multiply(0.30);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.button_frame = true;
    v.striped = false;
    v.slider_trailing_fill = true;

    let w = &mut v.widgets;
    let button_radius = CornerRadius::same(10);
    w.noninteractive.bg_fill = SURFACE;
    w.noninteractive.weak_bg_fill = SURFACE;
    w.noninteractive.bg_stroke = border_stroke();
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_SECONDARY);
    w.noninteractive.corner_radius = button_radius;
    w.inactive.bg_fill = SURFACE_HIGH;
    w.inactive.weak_bg_fill = SURFACE_HIGH;
    w.inactive.bg_stroke = Stroke::NONE;
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    w.inactive.corner_radius = button_radius;
    w.hovered.bg_fill = SURFACE_HIGHER;
    w.hovered.weak_bg_fill = SURFACE_HIGHER;
    w.hovered.bg_stroke = Stroke::NONE;
    w.hovered.fg_stroke = Stroke::new(1.5, TEXT_PRIMARY);
    w.hovered.corner_radius = button_radius;
    w.hovered.expansion = 0.0;
    w.active.bg_fill = SURFACE_HIGHER;
    w.active.weak_bg_fill = SURFACE_HIGHER;
    w.active.bg_stroke = focus_stroke(1.0);
    w.active.fg_stroke = Stroke::new(2.0, TEXT_PRIMARY);
    w.active.corner_radius = button_radius;
    w.active.expansion = 0.0;
    w.open.bg_fill = SURFACE_HIGHER;
    w.open.weak_bg_fill = SURFACE_HIGHER;
    w.open.bg_stroke = Stroke::NONE;
    w.open.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);
    w.open.corner_radius = button_radius;
    v
}

/// Text styles: `Small`, `Button`, `Body`, `Heading` (semibold), `hero` (bold),
/// `Monospace`.
pub fn text_styles() -> BTreeMap<TextStyle, FontId> {
    BTreeMap::from([
        (TextStyle::Small, font(FONT_TINY)),
        (TextStyle::Button, font_w(FONT_SMALL, Weight::Medium)),
        (TextStyle::Body, font(FONT_BODY)),
        (TextStyle::Heading, font_w(FONT_TITLE, Weight::Semibold)),
        (TextStyle::Monospace, mono(FONT_BODY)),
        (hero_style(), font_w(FONT_HERO, Weight::Bold)),
        (tiny_style(), font(FONT_TINY)),
    ])
}

/// The system UI font, when the platform ships one as a variable font.
#[cfg(target_os = "macos")]
const SYSTEM_UI_FONT: Option<&str> = Some("/System/Library/Fonts/SFNS.ttf");
#[cfg(not(target_os = "macos"))]
const SYSTEM_UI_FONT: Option<&str> = None;

/// System fonts tried, in order, as glyph fallbacks for symbols the UI font lacks.
#[cfg(target_os = "macos")]
const SYMBOL_FALLBACK_FONTS: &[(&str, &str)] = &[
    ("Apple Symbols", "/System/Library/Fonts/Apple Symbols.ttf"),
    (
        "Arial Unicode",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    ),
];
#[cfg(not(target_os = "macos"))]
const SYMBOL_FALLBACK_FONTS: &[(&str, &str)] = &[];

/// Builds the font set: the system UI font at every [`Weight`] in front of egui's
/// bundled fonts, then the symbol fallbacks. Returns the definitions and whether the
/// system font was found.
pub fn font_definitions() -> (FontDefinitions, bool) {
    let mut defs = FontDefinitions::default();
    let bundled: Vec<String> = defs
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();

    let system = SYSTEM_UI_FONT.and_then(|path| std::fs::read(path).ok());
    let found = system.is_some();
    let blob: Option<egui::epaint::text::Blob> = system.map(|bytes| Arc::new(bytes) as _);

    for weight in Weight::ALL {
        let mut family = Vec::new();
        if let Some(blob) = &blob {
            let name = format!("system-ui-{}", weight.wght() as u32);
            let mut data = FontData::from_blob(blob.clone(), 0);
            data.tweak = FontTweak {
                coords: VariationCoords::new([("wght", weight.wght())]),
                ..FontTweak::default()
            };
            defs.font_data.insert(name.clone(), Arc::new(data));
            family.push(name);
        }
        family.extend(bundled.iter().cloned());
        defs.families.insert(weight.family(), family);
    }

    for (name, path) in SYMBOL_FALLBACK_FONTS {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        defs.font_data
            .insert((*name).to_owned(), Arc::new(FontData::from_owned(bytes)));
        for family in defs.families.values_mut() {
            family.push((*name).to_owned());
        }
    }
    (defs, found)
}

/// Appends the platform symbol fonts to the proportional and monospace families of
/// an already configured context. Returns the number of fonts installed.
pub fn install_symbol_fallbacks(ctx: &egui::Context) -> usize {
    use egui::epaint::text::{FontInsert, FontPriority, InsertFontFamily};
    let mut installed = 0;
    for (name, path) in SYMBOL_FALLBACK_FONTS {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        ctx.add_font(FontInsert::new(
            name,
            FontData::from_owned(bytes),
            vec![
                InsertFontFamily {
                    family: FontFamily::Proportional,
                    priority: FontPriority::Lowest,
                },
                InsertFontFamily {
                    family: FontFamily::Monospace,
                    priority: FontPriority::Lowest,
                },
            ],
        ));
        installed += 1;
    }
    installed
}

/// Installs the design system on `ctx`: fonts, dark visuals, text styles and spacing.
pub fn apply(ctx: &egui::Context) {
    let (fonts, _) = font_definitions();
    ctx.set_fonts(fonts);
    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.visuals = visuals();
        style.text_styles = text_styles();
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(16.0, 10.0);
        style.spacing.interact_size = egui::vec2(40.0, 28.0);
        style.spacing.menu_margin = egui::Margin::same(8);
        style.spacing.window_margin = egui::Margin::same(0);
        style.spacing.scroll = egui::style::ScrollStyle::floating();
        style.interaction.selectable_labels = false;
        style.interaction.tooltip_delay = 0.5;
        style.url_in_tooltip = false;
        style.animation_time = 0.14;
    });
    ctx.set_theme(egui::Theme::Dark);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colours_parse() {
        assert_eq!(qml_color("#0c0d0f"), Some(BACKGROUND));
        assert_eq!(qml_color("#b8f05a"), Some(ACCENT));
        assert_eq!(
            qml_color("#55000000"),
            Some(Color32::from_rgba_unmultiplied(0, 0, 0, 0x55))
        );
        assert_eq!(qml_color("#12"), None);
        assert_eq!(qml_color("red"), None);
    }

    #[test]
    fn visuals_match_theme() {
        let v = visuals();
        assert!(v.dark_mode);
        assert_eq!(v.panel_fill, BACKGROUND);
        assert_eq!(v.window_corner_radius, CornerRadius::same(16));
        assert_eq!(v.hyperlink_color, ACCENT);
        assert_eq!(v.widgets.inactive.bg_fill, SURFACE_HIGH);
    }

    #[test]
    fn text_styles_cover_theme_sizes() {
        let styles = text_styles();
        assert_eq!(styles[&TextStyle::Small].size, 11.0);
        assert_eq!(styles[&TextStyle::Button].size, 13.0);
        assert_eq!(styles[&TextStyle::Body].size, 15.0);
        assert_eq!(styles[&TextStyle::Heading].size, 23.0);
        assert_eq!(styles[&hero_style()].size, FONT_HERO);
        assert_eq!(styles[&hero_style()].family, Weight::Bold.family());
    }

    #[test]
    fn every_weight_has_a_family() {
        let (defs, _) = font_definitions();
        for weight in Weight::ALL {
            let family = defs.families.get(&weight.family());
            assert!(family.is_some_and(|f| !f.is_empty()), "{weight:?}");
        }
    }

    #[test]
    fn apply_installs_dark_theme() {
        let ctx = egui::Context::default();
        apply(&ctx);
        assert_eq!(ctx.theme(), egui::Theme::Dark);
        let fill = ctx.global_style().visuals.panel_fill;
        assert_eq!(fill, BACKGROUND);
        assert_eq!(
            ctx.global_style().text_styles[&TextStyle::Heading].size,
            FONT_TITLE
        );
        // Laying out text in every weight must not panic (families exist).
        let mut output = ctx.run_ui(Default::default(), |ui| {
            for weight in Weight::ALL {
                ui.label(egui::RichText::new("Aa").font(font_w(14.0, weight)));
            }
        });
        output.textures_delta.clear();
    }
}
