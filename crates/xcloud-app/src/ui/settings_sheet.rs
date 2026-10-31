//! The settings sheet: a modal over the window with a category column on the left
//! and the selected category's controls on the right (Xbox-app style).
//!
//! The sheet edits a draft [`AppSettings`] in place and reports what happened in
//! [`SheetOutput`]; the shell applies, persists and forwards the change.

use egui::{
    Align2, Color32, CornerRadius, Id, Key, Rect, RichText, Sense, Stroke, StrokeKind, Ui,
    UiBuilder, pos2, vec2,
};

use crate::settings::{
    LABEL_DEADZONE, LABEL_FRAME_RATE, LABEL_HARDWARE_DECODE, LABEL_INVERT_Y, LABEL_IPV6,
    LABEL_LICENSED_ONLY, LABEL_PRESENT, LABEL_REGION, LABEL_RESOLUTION, LABEL_SHARPNESS,
    LABEL_SWAP_AB, LABEL_TRIGGER, LABEL_UPSCALE, LABEL_UPSCALER_KIND, LABEL_VIBRATION,
};
use crate::state::AppSettings;
use crate::theme::{self, Weight, font_w};
use crate::ui::icons::{self, Icon};
use crate::ui::kit::{self, Button, Tone};

/// Accessible label of the close button.
pub const CLOSE_LABEL: &str = "Close settings";
pub const LABEL_HUD: &str = "Performance overlay";
pub const LABEL_SIGN_OUT: &str = "Sign out";
pub const LABEL_REFRESH: &str = "Refresh library";

/// The sheet's categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Category {
    #[default]
    Streaming,
    Video,
    Controller,
    Language,
    Account,
    About,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::Streaming,
        Category::Video,
        Category::Controller,
        Category::Language,
        Category::Account,
        Category::About,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Category::Streaming => "Streaming",
            Category::Video => "Video & upscaling",
            Category::Controller => "Controller",
            Category::Language => "Language & region",
            Category::Account => "Account",
            Category::About => "About",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Category::Streaming => Icon::Cloud,
            Category::Video => Icon::Monitor,
            Category::Controller => Icon::Gamepad,
            Category::Language => Icon::Globe,
            Category::Account => Icon::Person,
            Category::About => Icon::Info,
        }
    }
}

/// What the sheet did this frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SheetOutput {
    /// The draft settings changed.
    pub changed: bool,
    /// The user closed the sheet (button, Escape or a click on the scrim).
    pub close: bool,
    pub sign_out: bool,
    pub refresh: bool,
}

/// Resolution profiles in display order: (value, title, subtitle, needs Ultimate).
pub const PROFILES: [(&str, &str, &str, bool); 5] = [
    (
        "auto",
        "Auto",
        "The service picks the best quality for your connection",
        false,
    ),
    ("720p", "720p", "About 3 GB per hour", false),
    ("1080p", "1080p", "About 7 GB per hour", false),
    ("1080pHQ", "1080p High Quality", "About 9 GB per hour", true),
    ("1440p", "1440p Max Quality", "About 14 GB per hour", true),
];

#[derive(Default)]
pub struct SettingsSheet {
    category: Category,
    /// Search text of the language list.
    language_filter: String,
}

impl SettingsSheet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn category(&self) -> Category {
        self.category
    }

    pub fn set_category(&mut self, category: Category) {
        self.category = category;
    }

    /// Paints the sheet over the whole window.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        settings: &mut AppSettings,
        app_version: &str,
        streaming_available: bool,
    ) -> SheetOutput {
        let mut out = SheetOutput::default();
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            out.close = true;
        }
        let screen = ctx.content_rect();
        egui::Area::new(Id::unique("settings_sheet"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                ui.set_min_size(screen.size());
                // Scrim; a click outside the sheet closes it.
                let scrim = ui.interact(screen, Id::unique("settings_scrim"), Sense::click());
                ui.painter().rect_filled(screen, 0.0, theme::SCRIM);
                let w = (screen.width() - 80.0).min(1180.0);
                let h = (screen.height() - 64.0).min(860.0);
                let sheet = Rect::from_center_size(screen.center(), vec2(w, h));
                if scrim.clicked()
                    && !ui
                        .ctx()
                        .pointer_interact_pos()
                        .is_some_and(|p| sheet.contains(p))
                {
                    out.close = true;
                }
                // Swallow clicks on the sheet so they never reach the scrim.
                ui.interact(sheet, Id::unique("settings_sheet_bg"), Sense::click());
                self.sheet(
                    ui,
                    sheet,
                    settings,
                    app_version,
                    streaming_available,
                    &mut out,
                );
            });
        out
    }

    fn sheet(
        &mut self,
        ui: &mut Ui,
        sheet: Rect,
        settings: &mut AppSettings,
        app_version: &str,
        streaming_available: bool,
        out: &mut SheetOutput,
    ) {
        let radius = CornerRadius::same(theme::RADIUS_LARGE as u8 + 4);
        let shadow = egui::Shadow {
            offset: [0, 18],
            blur: 60,
            spread: 0,
            color: Color32::from_black_alpha(150),
        };
        ui.painter().add(shadow.as_shape(sheet, radius));
        ui.painter().rect_filled(sheet, radius, theme::SHEET);
        let side_w = (sheet.width() * 0.27).clamp(240.0, 320.0);
        let side = Rect::from_min_size(sheet.min, vec2(side_w, sheet.height()));
        ui.painter().rect_filled(
            side,
            CornerRadius {
                nw: radius.nw,
                sw: radius.sw,
                ne: 0,
                se: 0,
            },
            theme::SHEET_SIDE,
        );

        // Left column: title, account, categories.
        ui.scope_builder(
            UiBuilder::new().max_rect(side.shrink2(vec2(22.0, 28.0))),
            |ui| {
                ui.spacing_mut().item_spacing = vec2(0.0, 4.0);
                ui.label(RichText::new("Settings").font(font_w(30.0, Weight::Bold)));
                ui.add_space(18.0);
                let (row, _) =
                    ui.allocate_exact_size(vec2(ui.available_width(), 52.0), Sense::hover());
                let avatar = Rect::from_center_size(
                    pos2(row.left() + 24.0, row.center().y),
                    vec2(44.0, 44.0),
                );
                ui.painter()
                    .circle_filled(avatar.center(), 22.0, theme::SURFACE_HIGHER);
                icons::paint(
                    ui.painter(),
                    avatar.shrink(11.0),
                    Icon::Person,
                    theme::TEXT_PRIMARY,
                );
                ui.painter().circle_filled(
                    avatar.right_bottom() - vec2(6.0, 6.0),
                    6.0,
                    theme::SHEET_SIDE,
                );
                ui.painter().circle_filled(
                    avatar.right_bottom() - vec2(6.0, 6.0),
                    4.0,
                    theme::SUCCESS,
                );
                ui.painter().text(
                    pos2(avatar.right() + 14.0, row.center().y - 10.0),
                    Align2::LEFT_CENTER,
                    "Signed in",
                    font_w(15.0, Weight::Semibold),
                    theme::TEXT_PRIMARY,
                );
                ui.painter().text(
                    pos2(avatar.right() + 14.0, row.center().y + 11.0),
                    Align2::LEFT_CENTER,
                    "Xbox Cloud Gaming",
                    theme::font(13.0),
                    theme::TEXT_SECONDARY,
                );
                ui.add_space(22.0);
                for category in Category::ALL {
                    if kit::nav_item(
                        ui,
                        category.icon(),
                        category.title(),
                        self.category == category,
                        false,
                    )
                    .clicked()
                    {
                        self.category = category;
                    }
                }
            },
        );

        // Close button.
        let close = Rect::from_center_size(
            pos2(sheet.right() - 34.0, sheet.top() + 34.0),
            vec2(40.0, 40.0),
        );
        if kit::place(ui, close, |ui| {
            kit::icon_button(ui, Icon::Close, 40.0, CLOSE_LABEL)
        })
        .clicked()
        {
            out.close = true;
        }

        // Right column.
        let content = Rect::from_min_max(
            pos2(side.right() + 44.0, sheet.top() + 30.0),
            pos2(sheet.right() - 44.0, sheet.bottom() - 10.0),
        );
        ui.scope_builder(UiBuilder::new().max_rect(content), |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            ui.label(RichText::new(self.category.title()).font(font_w(28.0, Weight::Semibold)));
            ui.add_space(20.0);
            egui::ScrollArea::vertical()
                .id_salt(("settings_body", self.category as u8))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing = vec2(0.0, 8.0);
                    ui.set_width(ui.available_width() - 8.0);
                    match self.category {
                        Category::Streaming => {
                            out.changed |= streaming(ui, settings, streaming_available)
                        }
                        Category::Video => out.changed |= video(ui, settings),
                        Category::Controller => out.changed |= controller(ui, settings),
                        Category::Language => {
                            out.changed |= language(ui, settings, &mut self.language_filter)
                        }
                        Category::Account => out.changed |= account(ui, settings, out),
                        Category::About => about(ui, app_version),
                    }
                    ui.add_space(24.0);
                });
        });
    }
}

// ----------------------------------------------------------------------------- sections

fn heading(ui: &mut Ui, title: &str, body: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title).font(font_w(16.0, Weight::Semibold)));
    if !body.is_empty() {
        ui.add_space(2.0);
        ui.label(
            RichText::new(body)
                .font(theme::font(14.0))
                .color(theme::TEXT_SECONDARY),
        );
    }
    ui.add_space(6.0);
}

fn caption(ui: &mut Ui, text: &str) {
    ui.add_space(6.0);
    ui.label(
        RichText::new(text)
            .font(font_w(13.0, Weight::Medium))
            .color(theme::TEXT_SECONDARY),
    );
}

fn streaming(ui: &mut Ui, s: &mut AppSettings, streaming_available: bool) -> bool {
    let mut changed = false;
    if !streaming_available {
        notice(
            ui,
            "Streaming is unavailable in this build: the native WebRTC stack was not linked.",
        );
    }
    heading(
        ui,
        LABEL_RESOLUTION,
        "The maximum resolution to request. The service may stream lower on a slow connection.",
    );
    let current = crate::settings::normalize_profile(&s.resolution_profile);
    let mut shown_ultimate = false;
    for (value, title, subtitle, ultimate) in PROFILES {
        if ultimate && !shown_ultimate {
            caption(ui, "Included with Game Pass Ultimate");
            shown_ultimate = true;
        }
        if kit::radio_card(ui, current == value, title, subtitle).clicked() && current != value {
            s.resolution_profile = value.to_owned();
            changed = true;
        }
    }
    heading(ui, LABEL_FRAME_RATE, "");
    for (fps, title, subtitle) in [
        (60, "60 fps", "Smoothest motion"),
        (30, "30 fps", "Less bandwidth on weaker connections"),
    ] {
        if kit::radio_card(ui, s.preferred_frame_rate == fps, title, subtitle).clicked()
            && s.preferred_frame_rate != fps
        {
            s.preferred_frame_rate = fps;
            changed = true;
        }
    }
    heading(ui, "Network", "");
    changed |= kit::setting_row(
        ui,
        LABEL_REGION,
        "Leave empty to use the closest region",
        |ui| {
            let edit = egui::TextEdit::singleline(&mut s.preferred_region)
                .hint_text("Automatic")
                .font(theme::font(14.5))
                .desired_width(200.0)
                .margin(egui::Margin::symmetric(12, 8));
            let response = ui.add(edit);
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::TextInput, true, LABEL_REGION)
            });
            response.changed()
        },
    );
    changed |= kit::setting_row(
        ui,
        LABEL_IPV6,
        "Use IPv6 candidates first when connecting",
        |ui| kit::toggle(ui, &mut s.prefer_ipv6, LABEL_IPV6).changed(),
    );
    changed
}

fn video(ui: &mut Ui, s: &mut AppSettings) -> bool {
    let mut changed = false;
    heading(
        ui,
        "Upscaling",
        "Sharpen lower-resolution streams to your display on the GPU.",
    );
    changed |= kit::setting_row(
        ui,
        LABEL_UPSCALE,
        "Increase resolution locally on the GPU",
        |ui| kit::toggle(ui, &mut s.upscale_enabled, LABEL_UPSCALE).changed(),
    );
    if s.upscale_enabled {
        changed |= kit::setting_row(
            ui,
            "Output resolution",
            "Keeps aspect ratio; never reduces the source resolution",
            |ui| {
                let previous = s.upscale_height;
                let response = egui::ComboBox::from_id_salt("upscale_output_height")
                    .selected_text(match s.upscale_height {
                        1080 => "1080p",
                        1440 => "1440p",
                        _ => "4K (2160p)",
                    })
                    .show_ui(ui, |ui| {
                        for (height, label) in
                            [(1080, "1080p"), (1440, "1440p"), (2160, "4K (2160p)")]
                        {
                            ui.selectable_value(&mut s.upscale_height, height, label);
                        }
                    });
                response.response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "Output resolution")
                });
                previous != s.upscale_height
            },
        );
        caption(ui, LABEL_UPSCALER_KIND);
        let current = crate::settings::normalize_upscaler(&s.upscaler);
        for (value, title, subtitle) in [
            (
                "fsr",
                "FSR 1.0",
                "AMD FidelityFX edge-adaptive upscaling with sharpening",
            ),
            (
                "metalfx",
                "MetalFX",
                "Apple's spatial scaler (falls back to FSR when unavailable)",
            ),
        ] {
            if kit::radio_card(ui, current == value, title, subtitle).clicked() && current != value
            {
                s.upscaler = value.to_owned();
                changed = true;
            }
        }
        if current == "fsr" {
            changed |= kit::setting_row(ui, LABEL_SHARPNESS, "Higher is crisper", |ui| {
                slider(ui, &mut s.sharpness, 0.0..=2.0, LABEL_SHARPNESS, |v| {
                    format!("{v:.2}")
                })
            });
        }
    }
    heading(ui, LABEL_PRESENT, "How frames reach the display.");
    for (value, title, subtitle) in [
        (
            "immediate",
            "Lowest latency",
            "Show each frame as soon as it is ready (may tear)",
        ),
        (
            "fifo",
            "Vsync",
            "Wait for the display refresh (adds up to a frame of delay)",
        ),
    ] {
        let current = crate::settings::normalize_present_mode(&s.present_mode);
        if kit::radio_card(ui, current == value, title, subtitle).clicked() && current != value {
            s.present_mode = value.to_owned();
            changed = true;
        }
    }
    if cfg!(target_os = "linux") {
        heading(ui, "Decoding", "");
        changed |= kit::setting_row(
            ui,
            LABEL_HARDWARE_DECODE,
            "Decode the stream on the GPU with VAAPI. Applies after restarting OpenXCloud",
            |ui| kit::toggle(ui, &mut s.hardware_decode, LABEL_HARDWARE_DECODE).changed(),
        );
    }
    heading(ui, "Overlay", "");
    changed |= kit::setting_row(
        ui,
        LABEL_HUD,
        "Resolution, frame rate, bitrate and latency while streaming",
        |ui| kit::toggle(ui, &mut s.hud_enabled, LABEL_HUD).changed(),
    );
    changed
}

fn controller(ui: &mut Ui, s: &mut AppSettings) -> bool {
    let mut changed = false;
    heading(ui, "Buttons", "");
    changed |= kit::setting_row(ui, LABEL_SWAP_AB, "Swap the A and B buttons", |ui| {
        kit::toggle(ui, &mut s.swap_ab, LABEL_SWAP_AB).changed()
    });
    changed |= kit::setting_row(
        ui,
        LABEL_INVERT_Y,
        "Invert the vertical axis of both sticks",
        |ui| kit::toggle(ui, &mut s.invert_y, LABEL_INVERT_Y).changed(),
    );
    heading(ui, "Sticks and triggers", "");
    let mut deadzone = s.deadzone as f32;
    if kit::setting_row(ui, LABEL_DEADZONE, "Ignore small stick movements", |ui| {
        slider(ui, &mut deadzone, 0.0..=0.9, LABEL_DEADZONE, |v| {
            format!("{:.0}%", v * 100.0)
        })
    }) {
        s.deadzone = f64::from(deadzone);
        changed = true;
    }
    let mut trigger = s.trigger_sensitivity as f32;
    if kit::setting_row(
        ui,
        LABEL_TRIGGER,
        "Lower values reach full pull sooner",
        |ui| {
            slider(ui, &mut trigger, 0.05..=1.0, LABEL_TRIGGER, |v| {
                format!("{:.0}%", v * 100.0)
            })
        },
    ) {
        s.trigger_sensitivity = f64::from(trigger);
        changed = true;
    }
    heading(ui, "Feedback", "");
    let mut vibration = s.vibration_intensity as f32;
    if kit::setting_row(
        ui,
        LABEL_VIBRATION,
        "Rumble strength on controllers that support it",
        |ui| {
            slider(ui, &mut vibration, 0.0..=1.0, LABEL_VIBRATION, |v| {
                format!("{:.0}%", v * 100.0)
            })
        },
    ) {
        s.vibration_intensity = f64::from(vibration);
        changed = true;
    }
    heading(
        ui,
        "Keyboard",
        "Without a controller, the keyboard acts as one while streaming.",
    );
    for (keys, action) in [
        ("W A S D", "Left stick"),
        ("Arrow keys", "D-pad"),
        ("Space / Return", "A"),
        ("Esc / Backspace", "B"),
        ("Q / E", "X / Y"),
        ("Z / C", "Bumpers"),
        ("R / F", "Triggers"),
        ("Tab / M", "Menu / View"),
    ] {
        let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
        let font = font_w(13.0, Weight::Semibold);
        let w = ui
            .painter()
            .layout_no_wrap(keys.to_owned(), font.clone(), Color32::WHITE)
            .size()
            .x
            + 20.0;
        let key = Rect::from_min_size(pos2(row.left(), row.center().y - 13.0), vec2(w, 26.0));
        ui.painter()
            .rect_filled(key, CornerRadius::same(6), theme::SURFACE_HIGH);
        ui.painter().rect_stroke(
            key,
            CornerRadius::same(6),
            Stroke::new(1.0, theme::BORDER),
            StrokeKind::Inside,
        );
        ui.painter().text(
            key.center(),
            Align2::CENTER_CENTER,
            keys,
            font,
            theme::TEXT_PRIMARY,
        );
        ui.painter().text(
            pos2(row.left() + 180.0, row.center().y),
            Align2::LEFT_CENTER,
            action,
            theme::font(14.0),
            theme::TEXT_SECONDARY,
        );
    }
    changed
}

/// Accessible label of the language search field.
pub const LANGUAGE_SEARCH_LABEL: &str = "Search languages";

fn language(ui: &mut Ui, s: &mut AppSettings, filter: &mut String) -> bool {
    use xcloud_proto::locales::{LOCALES, find};
    let mut changed = false;
    ui.label(
        RichText::new(
            "Games on the cloud console run in this language, and game titles and descriptions \
             use it too. It applies from the next game you start.",
        )
        .font(theme::font(14.0))
        .color(theme::TEXT_SECONDARY),
    );
    heading(ui, "Your language", "");
    if let Some(current) = find(&s.locale) {
        locale_row(ui, current, true);
    }
    heading(ui, "All languages", "");
    let edit = egui::TextEdit::singleline(filter)
        .hint_text(LANGUAGE_SEARCH_LABEL)
        .font(theme::font(14.5))
        .desired_width(ui.available_width().min(360.0))
        .margin(egui::Margin::symmetric(12, 9));
    let response = ui.add(edit);
    let value = filter.clone();
    response.widget_info(|| {
        let mut info =
            egui::WidgetInfo::labeled(egui::Role::TextInput, true, LANGUAGE_SEARCH_LABEL);
        info.current_text_value = Some(value.clone());
        info
    });
    ui.add_space(6.0);
    let needle = filter.trim().to_lowercase();
    let mut shown = 0;
    for info in LOCALES {
        if !needle.is_empty()
            && !info.native.to_lowercase().contains(&needle)
            && !info.english.to_lowercase().contains(&needle)
            && !info.code.to_lowercase().contains(&needle)
        {
            continue;
        }
        shown += 1;
        let selected = info.code.eq_ignore_ascii_case(&s.locale);
        if locale_row(ui, info, selected).clicked() && !selected {
            s.locale = info.code.to_owned();
            changed = true;
        }
    }
    if shown == 0 {
        ui.add_space(8.0);
        ui.label(
            RichText::new("No language matches.")
                .font(theme::font(14.0))
                .color(theme::TEXT_SECONDARY),
        );
    }
    changed
}

/// One language: native name, English name below, a check when selected.
fn locale_row(
    ui: &mut Ui,
    info: &xcloud_proto::locales::LocaleInfo,
    selected: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 58.0), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::Role::RadioButton, true, selected, info.native)
    });
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let t = kit::hover_t(ui, response.id, response.hovered());
    if t > 0.0 {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(10),
            kit::mix(Color32::TRANSPARENT, theme::SURFACE_HIGH, t),
        );
    }
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect,
            CornerRadius::same(10),
            theme::focus_stroke(2.0),
            StrokeKind::Inside,
        );
    }
    ui.painter().text(
        pos2(rect.left() + 16.0, rect.center().y - 10.0),
        Align2::LEFT_CENTER,
        info.native,
        font_w(
            15.5,
            if selected {
                Weight::Semibold
            } else {
                Weight::Regular
            },
        ),
        theme::TEXT_PRIMARY,
    );
    ui.painter().text(
        pos2(rect.left() + 16.0, rect.center().y + 11.0),
        Align2::LEFT_CENTER,
        info.english,
        theme::font(13.0),
        theme::TEXT_SECONDARY,
    );
    if selected {
        icons::paint(
            ui.painter(),
            icons::centered(pos2(rect.right() - 26.0, rect.center().y), 20.0),
            Icon::Check,
            theme::ACCENT,
        );
    }
    response
}

fn account(ui: &mut Ui, s: &mut AppSettings, out: &mut SheetOutput) -> bool {
    heading(
        ui,
        "Your library",
        "Reload the Game Pass catalog and your recently played games.",
    );
    let changed = kit::setting_row(
        ui,
        LABEL_LICENSED_ONLY,
        "Hide games that neither your subscription nor your purchases let you play",
        |ui| kit::toggle(ui, &mut s.licensed_only, LABEL_LICENSED_ONLY).changed(),
    );
    if Button::new(LABEL_REFRESH)
        .icon(Icon::Refresh)
        .show(ui)
        .clicked()
    {
        out.refresh = true;
    }
    heading(
        ui,
        "Session",
        "Forget the tokens stored in your Keychain. You will need a new device code to sign in again.",
    );
    if Button::new(LABEL_SIGN_OUT)
        .icon(Icon::SignOut)
        .tone(Tone::Secondary)
        .show(ui)
        .clicked()
    {
        out.sign_out = true;
    }
    changed
}

fn about(ui: &mut Ui, version: &str) {
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 72.0), Sense::hover());
    icons::paint(
        ui.painter(),
        Rect::from_min_size(row.min, vec2(56.0, 56.0)),
        Icon::Logo,
        theme::ACCENT,
    );
    ui.painter().text(
        pos2(row.left() + 74.0, row.top() + 16.0),
        Align2::LEFT_CENTER,
        "OpenXCloud",
        font_w(20.0, Weight::Bold),
        theme::TEXT_PRIMARY,
    );
    ui.painter().text(
        pos2(row.left() + 74.0, row.top() + 40.0),
        Align2::LEFT_CENTER,
        format!("Version {version}"),
        theme::font(14.0),
        theme::TEXT_SECONDARY,
    );
    heading(
        ui,
        "Xbox Cloud Gaming for your Mac",
        "Hardware-decoded video, GPU upscaling and a native WebRTC stack built for low latency.",
    );
    heading(
        ui,
        "Licence",
        "GNU General Public License v3.0. OpenXCloud is not affiliated with or endorsed by Microsoft.",
    );
}

fn notice(ui: &mut Ui, text: &str) {
    let width = ui.available_width();
    let galley = crate::ui::widgets::layout_font(
        ui,
        text,
        theme::font(14.0),
        theme::TEXT_PRIMARY,
        width - 64.0,
        4,
    );
    let (rect, _) = ui.allocate_exact_size(vec2(width, galley.size().y + 28.0), Sense::hover());
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(12),
        theme::WARNING.gamma_multiply(0.14),
    );
    icons::paint(
        ui.painter(),
        icons::centered(pos2(rect.left() + 24.0, rect.center().y), 18.0),
        Icon::Info,
        theme::WARNING,
    );
    ui.painter().galley(
        pos2(rect.left() + 46.0, rect.top() + 14.0),
        galley,
        theme::TEXT_PRIMARY,
    );
    ui.add_space(8.0);
}

/// A slim slider with a value readout. Returns `true` when the value changed.
pub fn slider(
    ui: &mut Ui,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    label: &str,
    format: impl Fn(f32) -> String,
) -> bool {
    let track_w = 200.0;
    let readout_w = 56.0;
    let (rect, response) =
        ui.allocate_exact_size(vec2(track_w + readout_w, 30.0), Sense::click_and_drag());
    response.widget_info(|| egui::WidgetInfo::slider(true, f64::from(*value), label));
    if response.hovered() || response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let track = Rect::from_min_max(
        pos2(rect.left() + readout_w + 8.0, rect.center().y - 2.0),
        pos2(rect.right() - 10.0, rect.center().y + 2.0),
    );
    let (lo, hi) = (*range.start(), *range.end());
    let mut changed = false;
    if let Some(p) = response.interact_pointer_pos()
        && (response.dragged() || response.clicked())
    {
        let t = ((p.x - track.left()) / track.width()).clamp(0.0, 1.0);
        let v = lo + (hi - lo) * t;
        let v = (v * 100.0).round() / 100.0;
        if (v - *value).abs() > f32::EPSILON {
            *value = v;
            changed = true;
        }
    }
    if response.has_focus() {
        let step = (hi - lo) / 20.0;
        let delta = ui.input(|i| {
            if i.key_pressed(Key::ArrowRight) {
                step
            } else if i.key_pressed(Key::ArrowLeft) {
                -step
            } else {
                0.0
            }
        });
        if delta != 0.0 {
            *value = (*value + delta).clamp(lo, hi);
            changed = true;
        }
    }
    let t = ((*value - lo) / (hi - lo)).clamp(0.0, 1.0);
    ui.painter()
        .rect_filled(track, CornerRadius::same(2), theme::SURFACE_HIGHER);
    let filled = Rect::from_min_max(
        track.min,
        pos2(track.left() + track.width() * t, track.bottom()),
    );
    ui.painter()
        .rect_filled(filled, CornerRadius::same(2), theme::ACCENT);
    let knob = pos2(track.left() + track.width() * t, track.center().y);
    ui.painter().circle_filled(knob, 9.0, theme::TEXT_PRIMARY);
    if response.has_focus() {
        ui.painter()
            .circle_stroke(knob, 12.0, theme::focus_stroke(2.0));
    }
    ui.painter().text(
        pos2(rect.left(), rect.center().y),
        Align2::LEFT_CENTER,
        format(*value),
        font_w(14.0, Weight::Medium),
        theme::TEXT_SECONDARY,
    );
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    fn harness<'a>(
        sheet: &'a mut SettingsSheet,
        settings: &'a mut AppSettings,
        outputs: &'a mut Vec<SheetOutput>,
    ) -> Harness<'a> {
        Harness::builder()
            .with_size(egui::vec2(1280.0, 860.0))
            .with_context_setup(theme::apply)
            .build_ui(move |ui| {
                let out = sheet.show(ui.ctx(), settings, "1.0", true);
                outputs.push(out);
            })
    }

    #[test]
    fn output_resolution_selection_updates_settings() {
        let mut settings = AppSettings::default();
        let mut changed = false;
        {
            let mut h = Harness::builder()
                .with_size(egui::vec2(900.0, 1000.0))
                .with_context_setup(theme::apply)
                .build_ui(|ui| {
                    changed |= video(ui, &mut settings);
                });
            h.run_steps(3);
            h.get_by_label("Output resolution").click();
            h.run_steps(3);
            h.get_by_label("1440p").click();
            h.run_steps(3);
        }
        assert_eq!(settings.upscale_height, 1440);
        assert!(changed);
    }

    #[test]
    fn every_category_shows_its_controls() {
        let expected: [(Category, &[&str]); 3] = [
            (
                Category::Streaming,
                &[LABEL_REGION, LABEL_IPV6, "1440p Max Quality", "60 fps"],
            ),
            (
                Category::Video,
                &[
                    LABEL_UPSCALE,
                    "Output resolution",
                    LABEL_SHARPNESS,
                    LABEL_HUD,
                    "Lowest latency",
                ],
            ),
            (
                Category::Controller,
                &[
                    LABEL_SWAP_AB,
                    LABEL_INVERT_Y,
                    LABEL_DEADZONE,
                    LABEL_TRIGGER,
                    LABEL_VIBRATION,
                ],
            ),
        ];
        for (category, labels) in expected {
            let mut sheet = SettingsSheet::new();
            sheet.set_category(category);
            let mut settings = AppSettings::default();
            let mut outputs = Vec::new();
            let mut h = harness(&mut sheet, &mut settings, &mut outputs);
            h.run_steps(2);
            for label in labels {
                assert!(
                    h.query_all_by_label(label).next().is_some(),
                    "{category:?} shows {label}"
                );
            }
        }
    }

    #[test]
    fn choosing_a_profile_changes_the_draft() {
        let mut sheet = SettingsSheet::new();
        let mut settings = AppSettings::default();
        let mut outputs = Vec::new();
        {
            let mut h = harness(&mut sheet, &mut settings, &mut outputs);
            h.run_steps(2);
            h.get_by_label("720p").click();
            h.run_steps(2);
        }
        assert_eq!(settings.resolution_profile, "720p");
        assert!(outputs.iter().any(|o| o.changed));
    }

    #[test]
    fn toggles_and_close_report_back() {
        let mut sheet = SettingsSheet::new();
        sheet.set_category(Category::Controller);
        let mut settings = AppSettings::default();
        let mut outputs = Vec::new();
        {
            let mut h = harness(&mut sheet, &mut settings, &mut outputs);
            h.run_steps(2);
            h.get_by_label(LABEL_INVERT_Y).click();
            h.run_steps(2);
            h.get_by_label(CLOSE_LABEL).click();
            h.run_steps(2);
        }
        assert!(settings.invert_y);
        assert!(outputs.iter().any(|o| o.close));
    }

    #[test]
    fn picking_a_language_changes_the_locale() {
        let mut sheet = SettingsSheet::new();
        sheet.set_category(Category::Language);
        let mut settings = AppSettings::default();
        let mut outputs = Vec::new();
        {
            let mut h = harness(&mut sheet, &mut settings, &mut outputs);
            h.run_steps(2);
            h.get_by_label(LANGUAGE_SEARCH_LABEL).click();
            h.run_steps(1);
            h.get_by_label(LANGUAGE_SEARCH_LABEL).type_text("deutsch");
            h.run_steps(2);
            assert!(
                h.query_by_label("Français - France").is_none(),
                "filtered out"
            );
            h.get_by_label("Deutsch - Deutschland").click();
            h.run_steps(2);
        }
        assert_eq!(settings.locale, "de-DE");
        assert!(outputs.iter().any(|o| o.changed));
    }

    #[test]
    fn account_actions_are_reported() {
        let mut sheet = SettingsSheet::new();
        sheet.set_category(Category::Account);
        let mut settings = AppSettings::default();
        let mut outputs = Vec::new();
        {
            let mut h = harness(&mut sheet, &mut settings, &mut outputs);
            h.run_steps(2);
            h.get_by_label(LABEL_SIGN_OUT).click();
            h.run_steps(2);
        }
        assert!(outputs.iter().any(|o| o.sign_out));
    }
}
