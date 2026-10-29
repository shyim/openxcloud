//! Port of qt/qml/DeviceCodeView.qml: shows the device code, copies it, opens the
//! Microsoft sign-in page and lets the user cancel.
//!
//! The QML opened the browser once from `Component.onCompleted`. `Main.qml` loads the
//! view through a `Loader` that is recreated every time `session.state` becomes
//! `"authenticating"`, so every sign-in attempt auto-opened the page. This view is
//! long-lived, so it treats "not shown in the previous frame" as that recreation:
//! the first [`DeviceCodeView::show`] after the shell stopped showing it (Cancel,
//! error, sign-out) opens the browser again through the view's opener
//! (`open::that_detached` on a helper thread by default, a recording closure in tests).

use std::sync::Arc;

use egui::{Rect, RichText, Ui, pos2, vec2};

use crate::state::{SessionCommand, SessionHandle, SessionSnapshot};
use crate::theme::{self, Weight, font_w};
use crate::ui::icons::Icon;
use crate::ui::kit::{Button, Tone};

/// Opens a URL in the system browser (`Qt.openUrlExternally`).
pub type UrlOpener = Arc<dyn Fn(&str) + Send + Sync>;

/// `Qt.openUrlExternally` is asynchronous; `open::that` would spawn the platform
/// launcher and wait for it inside the egui frame. Hand the URL off to a short-lived
/// thread and use the detached variant so the UI thread never waits on it.
fn system_opener() -> UrlOpener {
    Arc::new(|url: &str| {
        let url = url.to_owned();
        let spawned = std::thread::Builder::new()
            .name("open-sign-in-page".into())
            .spawn(move || {
                if let Err(err) = open::that_detached(&url) {
                    tracing::warn!(target: "Auth", "could not open sign-in page {url}: {err}");
                }
            });
        if let Err(err) = spawned {
            tracing::warn!(target: "Auth", "could not start the browser opener: {err}");
        }
    })
}

/// The URL the view opens: `DeviceCodeInfo::signInUrl()`, falling back to the
/// plain verification URI the QML used.
pub fn sign_in_url(session: &SessionSnapshot) -> &str {
    if session.sign_in_url.is_empty() {
        &session.verification_uri
    } else {
        &session.sign_in_url
    }
}

/// Status line under the spinner.
pub fn waiting_text(session: &SessionSnapshot) -> &str {
    if session.status_text.is_empty() {
        "Waiting for you to finish signing in"
    } else {
        &session.status_text
    }
}

pub struct DeviceCodeView {
    /// `didAutoOpen`: cleared again whenever the view is "recreated" (see
    /// [`Self::show`]) or [`Self::reset`] is called.
    did_auto_open: bool,
    auto_open: bool,
    /// `egui::Context::cumulative_frame_nr` of the last frame that showed the view.
    /// A gap of more than one frame means the shell routed elsewhere in between,
    /// which is where the QML `Loader` would have destroyed and recreated the view.
    last_shown_frame: Option<u64>,
    opener: UrlOpener,
}

impl Default for DeviceCodeView {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceCodeView {
    pub fn new() -> Self {
        Self {
            did_auto_open: false,
            auto_open: true,
            last_shown_frame: None,
            opener: system_opener(),
        }
    }

    /// Replaces the browser opener (tests, smoke mode).
    pub fn with_opener(mut self, opener: UrlOpener) -> Self {
        self.opener = opener;
        self
    }

    /// Disables the one-shot automatic browser open.
    pub fn without_auto_open(mut self) -> Self {
        self.auto_open = false;
        self
    }

    /// Forget the auto-open so a new sign-in attempt opens the browser again.
    /// [`Self::show`] does this by itself when the view was not shown in the previous
    /// frame; the shell may call it explicitly as well.
    pub fn reset(&mut self) {
        self.did_auto_open = false;
        self.last_shown_frame = None;
    }

    /// `Component.onCompleted` for a long-lived view: re-arm the auto-open when the
    /// view was not shown in the previous frame, i.e. the shell routed to another
    /// screen in between (the QML `Loader` would have created a fresh instance).
    fn note_shown(&mut self, frame: u64) {
        if let Some(previous) = self.last_shown_frame
            && frame > previous + 1
        {
            self.did_auto_open = false;
        }
        self.last_shown_frame = Some(frame);
    }

    /// `openSignInPage()`.
    pub fn open_sign_in_page(&self, session: &SessionSnapshot) {
        let url = sign_in_url(session);
        if !url.is_empty() {
            (self.opener)(url);
        }
    }

    pub fn show(&mut self, ui: &mut Ui, session: &SessionHandle) {
        let snapshot = session.read();
        self.note_shown(ui.ctx().cumulative_frame_nr());
        if self.auto_open && !self.did_auto_open && !sign_in_url(&snapshot).is_empty() {
            self.did_auto_open = true;
            self.open_sign_in_page(&snapshot);
        }

        crate::ui::signin::auth_layout(ui, 418.0, |ui| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            ui.label(RichText::new("Sign in on the web").font(font_w(28.0, Weight::Bold)));
            ui.add_space(10.0);
            ui.label(
                RichText::new(
                    "Go to microsoft.com/link and enter this code. It may already be filled in.",
                )
                .font(theme::font(15.0))
                .color(theme::TEXT_SECONDARY),
            );
            ui.add_space(24.0);
            code_box(ui, &snapshot.user_code);
            ui.add_space(22.0);
            let open_page = Button::new("Open Microsoft sign-in")
                .icon(Icon::External)
                .primary()
                .height(48.0)
                .min_width(ui.available_width())
                .enabled(!sign_in_url(&snapshot).is_empty())
                .show(ui);
            if open_page.clicked() {
                self.open_sign_in_page(&snapshot);
            }
            ui.add_space(10.0);
            let half = (ui.available_width() - 10.0) / 2.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = vec2(10.0, 0.0);
                if Button::new("Copy code")
                    .icon(Icon::Copy)
                    .height(44.0)
                    .min_width(half)
                    .show(ui)
                    .clicked()
                {
                    ui.ctx().copy_text(snapshot.user_code.clone());
                }
                if Button::new("Cancel")
                    .tone(Tone::Ghost)
                    .height(44.0)
                    .min_width(half)
                    .show(ui)
                    .clicked()
                {
                    session.send(SessionCommand::CancelSignIn);
                }
            });
            ui.add_space(22.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = vec2(10.0, 0.0);
                ui.add(egui::Spinner::new().size(16.0).color(theme::ACCENT));
                ui.label(
                    RichText::new(waiting_text(&snapshot))
                        .font(theme::font(14.0))
                        .color(theme::TEXT_SECONDARY),
                );
            });
        });
    }
}

/// The user code as large letter tiles, split at the dash.
fn code_box(ui: &mut Ui, user_code: &str) {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(vec2(width, 72.0), egui::Sense::hover());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, user_code));
    let chars: Vec<char> = user_code.chars().collect();
    let letters = chars.iter().filter(|c| **c != '-').count().max(1) as f32;
    let dashes = chars.iter().filter(|c| **c == '-').count() as f32;
    let gap = 6.0;
    let dash_w = 18.0;
    let tile_w = ((width - dash_w * dashes - gap * (letters - 1.0)) / letters).min(44.0);
    let total = tile_w * letters + gap * (letters - 1.0) + dash_w * dashes;
    let mut x = rect.center().x - total / 2.0;
    let font = theme::font_w(30.0, theme::Weight::Semibold);
    for c in chars {
        if c == '-' {
            ui.painter().rect_filled(
                Rect::from_center_size(
                    pos2(x + dash_w / 2.0 - gap / 2.0, rect.center().y),
                    vec2(8.0, 2.5),
                ),
                egui::CornerRadius::same(1),
                theme::TEXT_TERTIARY,
            );
            x += dash_w;
            continue;
        }
        let tile = Rect::from_min_size(pos2(x, rect.top()), vec2(tile_w, rect.height()));
        ui.painter()
            .rect_filled(tile, egui::CornerRadius::same(10), theme::SURFACE_HIGH);
        ui.painter().text(
            tile.center(),
            egui::Align2::CENTER_CENTER,
            c,
            font.clone(),
            theme::TEXT_PRIMARY,
        );
        x += tile_w + gap;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AuthState, Handle};
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use parking_lot::Mutex;

    fn snapshot() -> SessionSnapshot {
        SessionSnapshot {
            state: AuthState::Authenticating,
            user_code: "ABCD-EFGH".into(),
            verification_uri: "https://www.microsoft.com/link".into(),
            sign_in_url: "https://www.microsoft.com/link?otc=ABCDEFGH".into(),
            ..Default::default()
        }
    }

    fn recording_opener() -> (UrlOpener, Arc<Mutex<Vec<String>>>) {
        let opened = Arc::new(Mutex::new(Vec::new()));
        let sink = opened.clone();
        let opener: UrlOpener = Arc::new(move |url: &str| sink.lock().push(url.to_owned()));
        (opener, opened)
    }

    fn harness<'a>(view: &'a mut DeviceCodeView, session: &'a SessionHandle) -> Harness<'a> {
        Harness::builder()
            .with_size(vec2(900.0, 600.0))
            .with_context_setup(theme::apply)
            .allow_missing_glyphs()
            .build_ui(move |ui| view.show(ui, session))
    }

    #[test]
    fn auto_opens_sign_in_page_once() {
        let (session, _s, _rx) = Handle::new(snapshot());
        let (opener, opened) = recording_opener();
        let mut view = DeviceCodeView::new().with_opener(opener);
        let mut harness = harness(&mut view, &session);
        harness.run_steps(3);
        assert_eq!(
            *opened.lock(),
            vec!["https://www.microsoft.com/link?otc=ABCDEFGH".to_owned()]
        );
    }

    /// `Main.qml`'s `Loader` recreates the view on every "authenticating" entry, so a
    /// second sign-in attempt after Cancel auto-opens the browser again. The shell
    /// simply stops calling `show` while it routes elsewhere; nothing calls `reset`.
    #[test]
    fn auto_opens_again_after_the_shell_hid_the_view() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let (session, slot, _rx) = Handle::new(snapshot());
        let (opener, opened) = recording_opener();
        let mut view = DeviceCodeView::new().with_opener(opener);
        let visible = Arc::new(AtomicBool::new(true));
        let routed = visible.clone();
        let mut harness = Harness::builder()
            .with_size(vec2(900.0, 600.0))
            .with_context_setup(theme::apply)
            .allow_missing_glyphs()
            .build_ui(move |ui| {
                if routed.load(Ordering::SeqCst) {
                    view.show(ui, &session);
                } else {
                    ui.label("Sign in with Microsoft");
                }
            });
        harness.run_steps(3);
        assert_eq!(opened.lock().len(), 1, "first attempt opens once");

        // Cancel: the controller clears the code and the shell shows SignInView.
        {
            let mut s = slot.write();
            s.state = AuthState::Unauthenticated;
            s.user_code.clear();
            s.sign_in_url.clear();
            s.verification_uri.clear();
        }
        visible.store(false, Ordering::SeqCst);
        harness.run_steps(2);
        assert_eq!(opened.lock().len(), 1, "nothing opens while hidden");

        // Second "Sign in with Microsoft": a fresh code arrives and the view is back.
        {
            let mut s = slot.write();
            s.state = AuthState::Authenticating;
            s.user_code = "WXYZ-1234".into();
            s.sign_in_url = "https://www.microsoft.com/link?otc=WXYZ1234".into();
        }
        visible.store(true, Ordering::SeqCst);
        harness.run_steps(3);
        assert_eq!(
            *opened.lock(),
            vec![
                "https://www.microsoft.com/link?otc=ABCDEFGH".to_owned(),
                "https://www.microsoft.com/link?otc=WXYZ1234".to_owned(),
            ],
            "the second attempt auto-opens exactly once more"
        );
    }

    /// The code arrives a few frames after the view is first shown (the controller is
    /// still requesting it): the auto-open waits for it, then fires once.
    #[test]
    fn auto_open_waits_for_the_code_without_rearming() {
        let (session, slot, _rx) = Handle::new(SessionSnapshot {
            state: AuthState::Authenticating,
            ..Default::default()
        });
        let (opener, opened) = recording_opener();
        let mut view = DeviceCodeView::new().with_opener(opener);
        let mut harness = harness(&mut view, &session);
        harness.run_steps(3);
        assert!(opened.lock().is_empty(), "no URL yet, nothing to open");
        *slot.write() = snapshot();
        harness.run_steps(4);
        assert_eq!(
            *opened.lock(),
            vec!["https://www.microsoft.com/link?otc=ABCDEFGH".to_owned()]
        );
    }

    #[test]
    fn open_button_uses_sign_in_url() {
        let (session, _s, _rx) = Handle::new(snapshot());
        let (opener, opened) = recording_opener();
        let mut view = DeviceCodeView::new()
            .with_opener(opener)
            .without_auto_open();
        let mut harness = harness(&mut view, &session);
        harness.run_steps(2);
        assert!(opened.lock().is_empty());
        harness.get_by_label("Open Microsoft sign-in").click();
        harness.run_steps(2);
        assert_eq!(
            *opened.lock(),
            vec!["https://www.microsoft.com/link?otc=ABCDEFGH".to_owned()]
        );
    }

    #[test]
    fn copy_code_copies_user_code() {
        let (session, _s, _rx) = Handle::new(snapshot());
        let (opener, _opened) = recording_opener();
        let mut view = DeviceCodeView::new()
            .with_opener(opener)
            .without_auto_open();
        let mut harness = harness(&mut view, &session);
        harness.run_steps(2);
        harness.get_by_label("Copy code").click();
        harness.step();
        let copied = harness
            .output()
            .platform_output
            .commands
            .iter()
            .any(|c| matches!(c, egui::OutputCommand::CopyText(t) if t == "ABCD-EFGH"));
        assert!(copied, "expected a CopyText output command");
    }

    #[test]
    fn cancel_sends_cancel_sign_in() {
        let (session, _s, rx) = Handle::new(snapshot());
        let (opener, _opened) = recording_opener();
        let mut view = DeviceCodeView::new()
            .with_opener(opener)
            .without_auto_open();
        let mut harness = harness(&mut view, &session);
        harness.run_steps(2);
        harness.get_by_label("Cancel").click();
        harness.run_steps(2);
        assert_eq!(rx.try_recv(), Ok(SessionCommand::CancelSignIn));
    }

    #[test]
    fn status_text_overrides_waiting_copy() {
        let mut s = snapshot();
        assert_eq!(waiting_text(&s), "Waiting for you to finish signing in");
        s.status_text = "Checking…".into();
        assert_eq!(waiting_text(&s), "Checking…");
        s.sign_in_url.clear();
        assert_eq!(sign_in_url(&s), "https://www.microsoft.com/link");
    }
}
