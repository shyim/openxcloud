//! Port of qt/qml/SignInView.qml: the signed-out landing screen.
//!
//! Reads the [`SessionSnapshot`] once per frame; the only command it sends is
//! [`SessionCommand::BeginSignIn`].

use egui::{
    Align, Align2, Color32, CornerRadius, Layout, Rect, RichText, Stroke, StrokeKind, Ui,
    UiBuilder, pos2, vec2,
};

use crate::state::{SessionHandle, SessionSnapshot};
use crate::theme::{self, Weight, font_w};
use crate::ui::icons::{self, Icon};
use crate::ui::kit::Button;
use crate::ui::widgets;

/// Where `KeyringTokenStore` keeps tokens on this platform, for the sign-in footnote.
fn credential_store_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "the macOS Keychain"
    } else if cfg!(windows) {
        "Windows Credential Manager"
    } else {
        "your system keyring"
    }
}

/// `SignInView`'s button text for the current session state.
pub fn sign_in_button_text(session: &SessionSnapshot) -> &'static str {
    if session.is_busy {
        "Starting…"
    } else {
        "Sign in with Microsoft"
    }
}

/// The two-column layout shared by the signed-out screens: brand and value
/// propositions on the left, the action card (`card`, `card_height` tall) on the
/// right. On narrow windows only the card is shown, centred.
pub fn auth_layout(ui: &mut Ui, card_height: f32, card: impl FnOnce(&mut Ui)) {
    let full = ui.max_rect();
    ui.painter().rect_filled(full, 0.0, theme::BACKGROUND);
    widgets::radial_glow(
        ui,
        pos2(full.left() + full.width() * 0.18, full.bottom() + 40.0),
        vec2(full.width() * 0.75, full.height() * 0.9),
        theme::ACCENT,
        0.20,
    );
    widgets::radial_glow(
        ui,
        pos2(full.right() - full.width() * 0.1, full.top() - 80.0),
        vec2(full.width() * 0.5, full.height() * 0.6),
        Color32::from_rgb(0x4a, 0x7c, 0xff),
        0.10,
    );
    let wide = full.width() >= 980.0;
    let card_w = 440.0_f32.min(full.width() - 48.0);
    let card_x = if wide {
        full.right() - card_w - (full.width() * 0.09).max(48.0)
    } else {
        full.center().x - card_w / 2.0
    };
    let card_rect = Rect::from_min_size(
        pos2(
            card_x,
            (full.center().y - card_height / 2.0).max(full.top() + 24.0),
        ),
        vec2(card_w, card_height),
    );

    if wide {
        let left = full.left() + (full.width() * 0.08).max(48.0);
        let col_w = (card_rect.left() - left - 64.0).min(560.0);
        let mut y = full.center().y - 190.0;
        icons::paint(
            ui.painter(),
            Rect::from_min_size(pos2(left, y), vec2(52.0, 52.0)),
            Icon::Logo,
            theme::ACCENT,
        );
        ui.painter().text(
            pos2(left + 68.0, y + 26.0),
            Align2::LEFT_CENTER,
            "OpenXCloud",
            font_w(22.0, Weight::Bold),
            theme::TEXT_PRIMARY,
        );
        y += 92.0;
        let headline = widgets::paint_wrapped(
            ui,
            pos2(left, y),
            "Your Game Pass library, streamed to your Mac.",
            font_w(44.0, Weight::Bold),
            theme::TEXT_PRIMARY,
            col_w,
            3,
        );
        y = headline.bottom() + 30.0;
        for (icon, title, body) in [
            (
                Icon::Monitor,
                "Native and low latency",
                "Hardware decoding straight to the GPU, no browser in between.",
            ),
            (
                Icon::Sparkle,
                "Sharper than the stream",
                "FSR or MetalFX upscaling to your display, up to 4K.",
            ),
            (
                Icon::Gamepad,
                "Play your way",
                "Any controller, or your keyboard as one.",
            ),
        ] {
            let disc = Rect::from_min_size(pos2(left, y), vec2(40.0, 40.0));
            ui.painter().rect_filled(
                disc,
                CornerRadius::same(12),
                theme::SURFACE.gamma_multiply(0.9),
            );
            icons::paint(ui.painter(), disc.shrink(10.0), icon, theme::ACCENT);
            ui.painter().text(
                pos2(left + 56.0, y + 10.0),
                Align2::LEFT_CENTER,
                title,
                font_w(16.0, Weight::Semibold),
                theme::TEXT_PRIMARY,
            );
            widgets::paint_elided(
                ui,
                pos2(left + 56.0, y + 21.0),
                body,
                theme::font(14.0),
                theme::TEXT_SECONDARY,
                col_w - 56.0,
            );
            y += 62.0;
        }
    }

    ui.painter().rect_filled(
        card_rect,
        CornerRadius::same(22),
        theme::SHEET.gamma_multiply(0.94),
    );
    ui.painter().rect_stroke(
        card_rect,
        CornerRadius::same(22),
        Stroke::new(1.0, theme::BORDER),
        StrokeKind::Inside,
    );
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(card_rect.shrink2(vec2(36.0, 34.0)))
            .layout(Layout::top_down(Align::Min)),
        card,
    );
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SignInView;

impl SignInView {
    pub fn new() -> Self {
        Self
    }

    pub fn show(&mut self, ui: &mut Ui, session: &SessionHandle, streaming_available: bool) {
        let snapshot = session.read();
        let mut height = 290.0;
        if !snapshot.last_error.is_empty() {
            height += 70.0;
        }
        if !streaming_available {
            height += 70.0;
        }
        auth_layout(ui, height, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            ui.label(RichText::new("Welcome").font(font_w(30.0, Weight::Bold)));
            ui.add_space(10.0);
            ui.label(
                RichText::new(
                    "Sign in with the Microsoft account that has Xbox Game Pass Ultimate.",
                )
                .font(theme::font(15.0))
                .color(theme::TEXT_SECONDARY),
            );
            ui.add_space(28.0);
            let button = Button::new(sign_in_button_text(&snapshot))
                .primary()
                .height(52.0)
                .min_width(ui.available_width())
                .enabled(!snapshot.is_busy)
                .show(ui);
            if button.clicked() {
                session.send(crate::state::SessionCommand::BeginSignIn);
            }
            ui.add_space(18.0);
            ui.label(
                RichText::new(format!(
                    "You'll get a short code to enter on microsoft.com/link. Your tokens are kept in {}.",
                    credential_store_name()
                ))
                    .font(theme::font(13.0))
                    .color(theme::TEXT_TERTIARY),
            );
            if !snapshot.last_error.is_empty() {
                ui.add_space(18.0);
                message(ui, snapshot.last_error.as_str(), theme::DANGER);
            }
            if !streaming_available {
                ui.add_space(14.0);
                message(
                    ui,
                    "Streaming is unavailable in this build. You can still browse your library.",
                    theme::WARNING,
                );
            }
        });
    }
}

/// A tinted inline message (errors, warnings).
pub fn message(ui: &mut Ui, text: &str, color: Color32) {
    let width = ui.available_width();
    let galley = widgets::layout_font(
        ui,
        text,
        theme::font(13.5),
        theme::TEXT_PRIMARY,
        width - 48.0,
        4,
    );
    let (rect, response) =
        ui.allocate_exact_size(vec2(width, galley.size().y + 22.0), egui::Sense::hover());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, text));
    ui.painter()
        .rect_filled(rect, CornerRadius::same(10), color.gamma_multiply(0.14));
    icons::paint(
        ui.painter(),
        icons::centered(pos2(rect.left() + 18.0, rect.top() + 20.0), 16.0),
        Icon::Info,
        color,
    );
    ui.painter().galley(
        pos2(rect.left() + 36.0, rect.top() + 11.0),
        galley,
        theme::TEXT_PRIMARY,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AuthState, Handle, SessionCommand};
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    fn harness<'a>(
        view: &'a mut SignInView,
        session: &'a SessionHandle,
        streaming: bool,
    ) -> Harness<'a> {
        Harness::builder()
            .with_size(vec2(900.0, 600.0))
            .with_context_setup(theme::apply)
            .allow_missing_glyphs()
            .build_ui(move |ui| view.show(ui, session, streaming))
    }

    #[test]
    fn sign_in_click_sends_begin_sign_in() {
        let (session, _snapshot, rx) = Handle::new(SessionSnapshot {
            state: AuthState::Unauthenticated,
            ..Default::default()
        });
        let mut view = SignInView::new();
        let mut harness = harness(&mut view, &session, true);
        harness.run_steps(2);
        harness.get_by_label("Sign in with Microsoft").click();
        harness.run_steps(2);
        assert_eq!(rx.try_recv(), Ok(SessionCommand::BeginSignIn));
        assert!(rx.try_recv().is_err(), "exactly one command");
    }

    #[test]
    fn busy_state_disables_the_button() {
        let (session, _snapshot, rx) = Handle::new(SessionSnapshot {
            state: AuthState::Authenticating,
            is_busy: true,
            ..Default::default()
        });
        let mut view = SignInView::new();
        let mut harness = harness(&mut view, &session, true);
        harness.run_steps(2);
        let button = harness.get_by_label("Starting…");
        button.click();
        harness.run_steps(2);
        assert!(rx.try_recv().is_err(), "disabled button sends nothing");
    }

    #[test]
    fn error_and_unavailable_notes_are_shown() {
        let (session, _snapshot, _rx) = Handle::new(SessionSnapshot {
            last_error: "Token refresh failed".into(),
            ..Default::default()
        });
        let mut view = SignInView::new();
        let mut harness = harness(&mut view, &session, false);
        harness.run_steps(2);
        harness.get_by_label("Token refresh failed");
        harness.get_by_label_contains("Streaming is unavailable in this build");
    }
}
