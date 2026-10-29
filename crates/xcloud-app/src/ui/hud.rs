//! Port of qt/qml/StreamHUD.qml: the compact performance pill at the bottom of the stream
//! view (resolution and upscale label, frame rate, bitrate, colour-coded round trip, packet
//! loss) extended with the per-stage latency numbers PORTING_PLAN §4 asks for (decode,
//! GPU, frame age, jitter wait, present). The formatting lives in [`hud_items`] so it is
//! testable without a
//! window; [`StreamHud`] paints those items.

use std::cell::RefCell;
use std::collections::VecDeque;

use egui::{
    Color32, CornerRadius, Pos2, Rect, Role, Sense, Stroke, StrokeKind, Vec2, WidgetInfo, pos2,
    vec2,
};

use crate::state::{StreamHandle, StreamSnapshot};
use crate::theme;

/// `implicitHeight: 34`.
pub const HUD_HEIGHT: f32 = 38.0;
/// `RowLayout.spacing: 14`.
const ITEM_SPACING: f32 = 22.0;
/// `implicitWidth: layout.implicitWidth + 28`.
const HORIZONTAL_PADDING: f32 = 36.0;
/// `anchors.bottomMargin: 18` in StreamView.qml.
pub const HUD_BOTTOM_MARGIN: f32 = 18.0;

/// One entry of the pill: its text and colour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HudItem {
    pub text: String,
    pub color: Color32,
}

impl HudItem {
    fn new(text: impl Into<String>, color: Color32) -> Self {
        Self {
            text: text.into(),
            color,
        }
    }
}

/// `StreamHUD.latencyColor()`: accent below 60 ms, warning below 120 ms, danger above.
pub fn latency_color(round_trip_ms: f64) -> Color32 {
    if round_trip_ms < 60.0 {
        theme::ACCENT
    } else if round_trip_ms < 120.0 {
        theme::WARNING
    } else {
        theme::DANGER
    }
}

/// The text of the marker shown while the stream view draws through the CPU image path.
pub const CPU_VIDEO_LABEL: &str = "CPU video";

/// The entries StreamHUD.qml shows for `s`, in its order, followed by the new latency
/// fields. Entries whose value is unknown (zero) are left out, like the QML `visible`
/// bindings. The upscale label reads `1440p → 2160p FSR` or `1440p → 2160p FX` for
/// MetalFX, from `StreamSnapshot::upscaler_label` (PORTING_PLAN §5.4 keeps the two
/// distinct; the Qt app only ever said `FX`).
pub fn hud_items(s: &StreamSnapshot) -> Vec<HudItem> {
    hud_items_for_path(s, false)
}

/// [`hud_items`] plus, when `cpu_video` is set, a warning-coloured [`CPU_VIDEO_LABEL`]:
/// the stream view reverted from the GPU path to CPU conversion (what MetalVideoItem's
/// `nativeVideoEnabled == false` meant, which the Qt HUD never showed).
pub fn hud_items_for_path(s: &StreamSnapshot, cpu_video: bool) -> Vec<HudItem> {
    let mut items = Vec::with_capacity(11);
    let height = s.resolution_height();
    if height > 0 {
        let mut text = format!("{height}p");
        if s.upscaled && s.output_height > 0 {
            text.push_str(&format!(" → {}p", s.output_height));
            if !s.upscaler_label.is_empty() {
                text.push(' ');
                text.push_str(&s.upscaler_label);
            }
        }
        items.push(HudItem::new(text, theme::TEXT_PRIMARY));
    }
    let fps = s.stats.frames_per_second;
    if fps > 0.0 {
        items.push(HudItem::new(
            format!("{} fps", fps.round() as i64),
            theme::TEXT_PRIMARY,
        ));
    }
    let mbps = s.bitrate_mbps();
    if mbps > 0.0 {
        items.push(HudItem::new(format!("{mbps:.1} Mbps"), theme::TEXT_PRIMARY));
    }
    let rtt = s.stats.round_trip_time_ms;
    if rtt > 0.0 {
        items.push(HudItem::new(
            format!("{} ms", rtt.round() as i64),
            latency_color(rtt),
        ));
    }
    if s.stats.packets_lost > 0 {
        items.push(HudItem::new(
            format!("{} lost", s.stats.packets_lost),
            theme::WARNING,
        ));
    }
    // New in the Rust port: per-stage latency (PORTING_PLAN §4).
    if s.decode_ms_avg > 0.0 {
        let text = if s.decode_ms_max > 0.0 {
            format!("dec {:.1}/{:.1} ms", s.decode_ms_avg, s.decode_ms_max)
        } else {
            format!("dec {:.1} ms", s.decode_ms_avg)
        };
        items.push(HudItem::new(text, theme::TEXT_SECONDARY));
    }
    if s.gpu_ms > 0.0 {
        items.push(HudItem::new(
            format!("gpu {:.2} ms", s.gpu_ms),
            theme::TEXT_SECONDARY,
        ));
    }
    if s.frame_age_ms > 0.0 {
        items.push(HudItem::new(
            format!("age {:.0} ms", s.frame_age_ms),
            theme::TEXT_SECONDARY,
        ));
    }
    if s.jitter_ms_avg > 0.0 {
        let text = if s.jitter_ms_max > 0.0 {
            format!("jit {:.1}/{:.1} ms", s.jitter_ms_avg, s.jitter_ms_max)
        } else {
            format!("jit {:.1} ms", s.jitter_ms_avg)
        };
        items.push(HudItem::new(text, theme::TEXT_SECONDARY));
    }
    // An upper bound quantised to the UI frame period (see `StreamSnapshot::present_ms`).
    if s.present_ms > 0.0 {
        items.push(HudItem::new(
            format!("present {:.0} ms", s.present_ms),
            theme::TEXT_SECONDARY,
        ));
    }
    if cpu_video {
        items.push(HudItem::new(CPU_VIDEO_LABEL, theme::WARNING));
    }
    items
}

/// The text of the whole pill, for accessibility and tests.
pub fn hud_text(items: &[HudItem]) -> String {
    items
        .iter()
        .map(|i| i.text.as_str())
        .collect::<Vec<_>>()
        .join("  ")
}

/// `StreamHUD` as a widget: reads the stream snapshot and paints the pill.
pub struct StreamHud {
    stream: StreamHandle,
    history: RefCell<History>,
}

impl StreamHud {
    pub fn new(stream: StreamHandle) -> Self {
        Self {
            stream,
            history: RefCell::new(History::default()),
        }
    }

    pub fn reset(&self) {
        *self.history.borrow_mut() = History::default();
    }

    /// `visible: stream.hudEnabled && stream.connected`.
    pub fn visible(s: &StreamSnapshot) -> bool {
        s.hud_enabled && s.connected()
    }

    /// Paints the pill horizontally centred at the bottom of `area` (18 pt margin) when
    /// visible. Returns the rectangle it covered.
    pub fn show(&self, ui: &mut egui::Ui, area: Rect) -> Option<Rect> {
        let snapshot = self.stream.read();
        self.show_snapshot(ui, area, &snapshot)
    }

    /// Like [`StreamHud::show`] with an already read snapshot.
    pub fn show_snapshot(&self, ui: &mut egui::Ui, area: Rect, s: &StreamSnapshot) -> Option<Rect> {
        self.show_snapshot_for_path(ui, area, s, false)
    }

    /// Like [`StreamHud::show_snapshot`], with the CPU video marker when `cpu_video` is set
    /// (see [`hud_items_for_path`]).
    pub fn show_snapshot_for_path(
        &self,
        ui: &mut egui::Ui,
        area: Rect,
        s: &StreamSnapshot,
        cpu_video: bool,
    ) -> Option<Rect> {
        let now = ui.input(|i| i.time);
        self.history.borrow_mut().sample(now, s);
        if !Self::visible(s) {
            return None;
        }
        self.show_details(ui, area, s);
        let items = hud_items_for_path(s, cpu_video);
        if items.is_empty() {
            return None;
        }
        let bottom_centre = pos2(area.center().x, area.max.y - HUD_BOTTOM_MARGIN);
        // Wrap the readouts instead of letting a long pill extend off-screen.
        let mut rows: Vec<Vec<HudItem>> = vec![Vec::new()];
        for item in items {
            let row = rows.last_mut().unwrap();
            row.push(item);
            if row.len() > 1 && pill_size(ui, row).x > area.width() - 24.0 {
                let last = row.pop().unwrap();
                rows.push(vec![last]);
            }
        }
        let mut covered = Rect::NOTHING;
        for (index, row) in rows.iter().rev().enumerate() {
            let rect = ui
                .push_id(index, |ui| {
                    paint_pill(
                        ui,
                        bottom_centre - vec2(0.0, index as f32 * (HUD_HEIGHT + 4.0)),
                        row,
                    )
                })
                .inner;
            covered = covered.union(rect);
        }
        Some(covered)
    }
}

#[derive(Default)]
struct History {
    points: VecDeque<(f64, [Option<f64>; 3])>,
    title: String,
}

impl History {
    fn sample(&mut self, now: f64, s: &StreamSnapshot) {
        if !s.connected() || self.title != s.title_id {
            self.points.clear();
            self.title = s.title_id.clone();
        }
        if !s.connected() {
            return;
        }
        while self
            .points
            .front()
            .is_some_and(|(time, _)| now - time > 60.0)
        {
            self.points.pop_front();
        }
        if self.points.back().is_some_and(|(time, _)| now - time < 1.0) {
            return;
        }
        let valid = |v: f64| (v.is_finite() && v > 0.0).then_some(v);
        let rate = |v: f64| {
            (v.is_finite() && v >= 0.0 && (s.pipeline_stats_ready || v > 0.0)).then_some(v)
        };
        self.points.push_back((
            now,
            [
                rate(s.stats.frames_per_second),
                rate(s.bitrate_mbps()),
                valid(s.stats.round_trip_time_ms),
            ],
        ));
    }
}

fn detail_rows(s: &StreamSnapshot) -> Vec<(&'static str, String)> {
    if !s.pipeline_stats_ready {
        return vec![("Pipeline", "Waiting for measurements…".into())];
    }
    let d = &s.decode_stats;
    let v = &s.video_stats;
    vec![
        ("Incomplete video frames dropped", v.dropped.to_string()),
        ("Decode queue drops", d.dropped_full.to_string()),
        ("Frames skipped during recovery", d.gated.to_string()),
        ("Frames without a decoder", d.unavailable.to_string()),
        ("Decoder errors", d.decode_errors.to_string()),
        ("Frames decoded", d.frames_decoded.to_string()),
        ("Audio packets lost", s.stats.audio_packets_lost.to_string()),
        ("Retransmission requests (NACK)", v.nacks.to_string()),
        ("Video resyncs", v.resyncs.to_string()),
        (
            "Decode queue + submit · avg / max",
            if d.queue.avg_ms > 0.0 {
                format!("{:.1} / {:.1} ms", d.queue.avg_ms, d.queue.max_ms)
            } else {
                "—".into()
            },
        ),
    ]
}

impl StreamHud {
    fn show_details(&self, ui: &mut egui::Ui, area: Rect, s: &StreamSnapshot) {
        egui::Area::new(ui.scope_id().with("stream-details"))
            .order(egui::Order::Foreground)
            .fixed_pos(area.min + vec2(12.0, 64.0))
            .constrain_to(area)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).fill(Color32::from_black_alpha(225)).show(ui, |ui| {
                    egui::CollapsingHeader::new("Detailed stats").show(ui, |ui| {
                        ui.set_width((area.width() - 72.0).clamp(180.0, 430.0));
                        // Area remembers its collapsed height. Explicitly grow the content
                        // rectangle before ScrollArea measures it, or it stays trapped in
                        // that old height and shows only one counter.
                        let content_height = (area.height() - 190.0).clamp(100.0, 590.0);
                        ui.set_height(content_height);
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .max_height(content_height)
                            .show(ui, |ui| {
                            ui.label("Session totals · reset for each stream");
                            egui::Grid::new("performance-counters").min_row_height(18.0).spacing(vec2(14.0, 4.0)).striped(true).show(ui, |ui| {
                                for (label, value) in detail_rows(s) {
                                    ui.label(label); ui.monospace(value); ui.end_row();
                                }
                            });
                            ui.add_space(8.0);
                            ui.label("Last 60 seconds · sampled once per second");
                            let history = self.history.borrow();
                            for (index, label) in ["FPS", "Bitrate · Mbps", "RTT · ms"].iter().enumerate() {
                                graph(ui, label, &history, index);
                            }
                            ui.small("RTT is network round-trip time, not input-to-screen latency. Gaps mean no measurement.");
                        });
                    });
                });
            });
    }
}

fn graph(ui: &mut egui::Ui, label: &str, history: &History, index: usize) {
    let current = history.points.back().and_then(|(_, values)| values[index]);
    let maximum = history
        .points
        .iter()
        .filter_map(|(_, values)| values[index])
        .fold(1.0_f64, f64::max);
    ui.label(format!(
        "{label}   {}   (scale 0–{maximum:.1})",
        current
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "—".into())
    ));
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, 4.0, Color32::from_white_alpha(12));
    let now = ui.input(|i| i.time);
    let mut previous = None;
    for (time, values) in &history.points {
        if let Some(value) = values[index] {
            let point = pos2(
                rect.right() - ((now - time) / 60.0) as f32 * rect.width(),
                rect.bottom() - (value / maximum) as f32 * rect.height(),
            );
            if let Some((last_time, last_point)) = previous
                && time - last_time < 2.0
            {
                ui.painter()
                    .line_segment([last_point, point], Stroke::new(1.5, theme::ACCENT));
            }
            previous = Some((*time, point));
        } else {
            previous = None;
        }
    }
}

/// Size of the pill for `items`, without painting.
///
/// The text uses the monospace family: it is the one bundled family with `→` and it keeps
/// the numbers from jumping as they change.
pub fn pill_size(ui: &egui::Ui, items: &[HudItem]) -> Vec2 {
    let font = hud_font();
    let text_width: f32 = items
        .iter()
        .map(|item| {
            ui.painter()
                .layout_no_wrap(item.text.clone(), font.clone(), item.color)
                .size()
                .x
        })
        .sum::<f32>()
        + ITEM_SPACING * (items.len().saturating_sub(1)) as f32;
    vec2(text_width + HORIZONTAL_PADDING, HUD_HEIGHT)
}

/// Paints `items` in one pill whose bottom centre is at `bottom_centre`.
/// Tabular-looking medium weight for the readouts.
fn hud_font() -> egui::FontId {
    theme::font_w(13.0, theme::Weight::Medium)
}

fn paint_pill(ui: &mut egui::Ui, bottom_centre: Pos2, items: &[HudItem]) -> Rect {
    let font = hud_font();
    let galleys: Vec<_> = items
        .iter()
        .map(|item| {
            ui.painter()
                .layout_no_wrap(item.text.clone(), font.clone(), item.color)
        })
        .collect();
    let size = pill_size(ui, items);
    let rect = Rect::from_min_size(bottom_centre - vec2(size.x / 2.0, size.y), size);

    let painter = ui.painter();
    // color: rgba(0, 0, 0, 0.55); border: rgba(1, 1, 1, 0.12); radius: height / 2.
    painter.rect(
        rect,
        CornerRadius::same((HUD_HEIGHT / 2.0) as u8),
        Color32::from_black_alpha(170),
        Stroke::new(1.0, Color32::from_white_alpha(22)),
        StrokeKind::Inside,
    );
    let mut x = rect.min.x + HORIZONTAL_PADDING / 2.0;
    let count = galleys.len();
    for (i, galley) in galleys.into_iter().enumerate() {
        let pos = pos2(x, rect.center().y - galley.size().y / 2.0);
        x += galley.size().x + ITEM_SPACING;
        painter.galley(pos, galley, Color32::PLACEHOLDER);
        if i + 1 < count {
            painter.circle_filled(
                pos2(x - ITEM_SPACING / 2.0, rect.center().y),
                1.6,
                Color32::from_white_alpha(70),
            );
        }
    }

    // Expose the pill text to accessibility (and egui_kittest).
    let text = hud_text(items);
    let response = ui.interact(rect, ui.scope_id().with("stream-hud"), Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(Role::Label, true, text.clone()));
    rect
}

#[cfg(test)]
mod tests {
    use super::*;
    use xcloud_proto::streaming_types::{StreamLifecycleState, StreamingStatsSnapshot};

    fn sample() -> StreamSnapshot {
        StreamSnapshot {
            lifecycle: StreamLifecycleState::Connected,
            hud_enabled: true,
            upscaled: true,
            output_height: 2160,
            upscaler_label: "FSR".into(),
            decode_ms_avg: 4.25,
            decode_ms_max: 7.9,
            gpu_ms: 0.812,
            frame_age_ms: 11.6,
            jitter_ms_avg: 1.94,
            jitter_ms_max: 12.0,
            present_ms: 18.3,
            stats: StreamingStatsSnapshot {
                bitrate_kbps: 14_560,
                frames_per_second: 59.6,
                round_trip_time_ms: 38.4,
                packets_lost: 3,
                negotiated_width: 2560,
                negotiated_height: 1440,
                ..StreamingStatsSnapshot::default()
            },
            ..StreamSnapshot::default()
        }
    }

    #[test]
    fn formats_a_sample_snapshot_like_streamhud_qml() {
        let items = hud_items(&sample());
        let texts: Vec<&str> = items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "1440p → 2160p FSR",
                "60 fps",
                "14.6 Mbps",
                "38 ms",
                "3 lost",
                "dec 4.2/7.9 ms",
                "gpu 0.81 ms",
                "age 12 ms",
                "jit 1.9/12.0 ms",
                "present 18 ms",
            ]
        );
        assert_eq!(items[0].color, theme::TEXT_PRIMARY);
        assert_eq!(items[3].color, theme::ACCENT, "38 ms is below 60");
        assert_eq!(items[4].color, theme::WARNING, "loss is always a warning");
        assert_eq!(
            hud_text(&items),
            "1440p → 2160p FSR  60 fps  14.6 Mbps  38 ms  3 lost  dec 4.2/7.9 ms  gpu 0.81 ms  age 12 ms  jit 1.9/12.0 ms  present 18 ms"
        );
        assert_eq!(items[8].color, theme::TEXT_SECONDARY);
        assert_eq!(items[9].color, theme::TEXT_SECONDARY);
    }

    #[test]
    fn resolution_without_upscaling_has_no_arrow() {
        let mut s = sample();
        s.upscaled = false;
        assert_eq!(hud_items(&s)[0].text, "1440p");
        s.upscaled = true;
        s.output_height = 0;
        assert_eq!(hud_items(&s)[0].text, "1440p", "unknown output height");
    }

    #[test]
    fn upscaler_label_follows_the_renderer() {
        let mut s = sample();
        s.upscaler_label = "FX".into();
        assert_eq!(hud_items(&s)[0].text, "1440p → 2160p FX", "MetalFX");
        s.upscaler_label.clear();
        assert_eq!(hud_items(&s)[0].text, "1440p → 2160p", "unknown upscaler");
    }

    #[test]
    fn zero_values_are_hidden() {
        let s = StreamSnapshot {
            lifecycle: StreamLifecycleState::Connected,
            hud_enabled: true,
            ..StreamSnapshot::default()
        };
        assert!(hud_items(&s).is_empty());

        let mut s = sample();
        s.stats.round_trip_time_ms = 0.0;
        s.stats.packets_lost = 0;
        s.decode_ms_max = 0.0;
        s.gpu_ms = 0.0;
        s.frame_age_ms = 0.0;
        s.jitter_ms_max = 0.0;
        s.present_ms = 0.0;
        let texts: Vec<String> = hud_items(&s).into_iter().map(|i| i.text).collect();
        assert_eq!(
            texts,
            [
                "1440p → 2160p FSR",
                "60 fps",
                "14.6 Mbps",
                "dec 4.2 ms",
                "jit 1.9 ms"
            ]
        );
        s.jitter_ms_avg = 0.0;
        let texts: Vec<String> = hud_items(&s).into_iter().map(|i| i.text).collect();
        assert_eq!(
            texts,
            ["1440p → 2160p FSR", "60 fps", "14.6 Mbps", "dec 4.2 ms"]
        );
    }

    #[test]
    fn cpu_video_marker_is_appended_only_when_asked() {
        let s = sample();
        assert_eq!(hud_items_for_path(&s, false), hud_items(&s));
        let items = hud_items_for_path(&s, true);
        let last = items.last().expect("marker");
        assert_eq!(last.text, CPU_VIDEO_LABEL);
        assert_eq!(last.color, theme::WARNING);
        assert_eq!(items.len(), hud_items(&s).len() + 1);
        // Even an otherwise empty pill carries the marker.
        let bare = StreamSnapshot {
            lifecycle: StreamLifecycleState::Connected,
            hud_enabled: true,
            ..StreamSnapshot::default()
        };
        assert_eq!(hud_text(&hud_items_for_path(&bare, true)), CPU_VIDEO_LABEL);
    }

    #[test]
    fn latency_is_colour_coded() {
        assert_eq!(latency_color(0.0), theme::ACCENT);
        assert_eq!(latency_color(59.9), theme::ACCENT);
        assert_eq!(latency_color(60.0), theme::WARNING);
        assert_eq!(latency_color(119.9), theme::WARNING);
        assert_eq!(latency_color(120.0), theme::DANGER);
        assert_eq!(latency_color(400.0), theme::DANGER);

        let mut s = sample();
        s.stats.round_trip_time_ms = 95.0;
        assert_eq!(hud_items(&s)[3].color, theme::WARNING);
        s.stats.round_trip_time_ms = 180.0;
        assert_eq!(hud_items(&s)[3].color, theme::DANGER);
    }

    #[test]
    fn visible_only_when_enabled_and_connected() {
        let mut s = sample();
        assert!(StreamHud::visible(&s));
        s.hud_enabled = false;
        assert!(!StreamHud::visible(&s));
        s.hud_enabled = true;
        s.lifecycle = StreamLifecycleState::ConnectingWebRTC;
        assert!(!StreamHud::visible(&s));
    }

    #[test]
    fn paints_the_pill_with_the_joined_text_as_label() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;

        let stream = StreamHandle::detached(sample());
        let hud = StreamHud::new(stream);
        // No `allow_missing_glyphs`: the test also proves the UI font has `→`.
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1600.0, 700.0))
            .with_context_setup(theme::apply)
            .build_ui(|ui| {
                let area = ui.max_rect();
                let rect = hud.show(ui, area).expect("visible");
                assert!((rect.height() - HUD_HEIGHT).abs() < 0.01);
                assert!((rect.max.y - (area.max.y - HUD_BOTTOM_MARGIN)).abs() < 0.01);
                assert!((rect.center().x - area.center().x).abs() < 0.5);
            });
        harness.run();
        let expected = hud_text(&hud_items(&sample()));
        harness.get_by_label(&expected);
    }

    #[test]
    fn history_is_bounded_throttled_and_resets_between_sessions() {
        let mut history = History::default();
        let mut s = sample();
        for second in 0..120 {
            history.sample(second as f64, &s);
            history.sample(second as f64 + 0.1, &s);
        }
        assert!(history.points.len() <= 61);
        assert_eq!(history.points.back().unwrap().0, 119.0);
        s.stats.round_trip_time_ms = f64::NAN;
        s.pipeline_stats_ready = true;
        s.stats.frames_per_second = 0.0;
        history.sample(120.0, &s);
        assert_eq!(history.points.back().unwrap().1[2], None);
        assert_eq!(history.points.back().unwrap().1[0], Some(0.0));
        s.lifecycle = xcloud_proto::streaming_types::StreamLifecycleState::Idle;
        history.sample(121.0, &s);
        assert!(history.points.is_empty());
    }

    #[test]
    fn details_expand_and_narrow_hud_stays_inside_window() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;
        let mut s = sample();
        s.pipeline_stats_ready = true;
        s.decode_stats.decode_errors = 7;
        let hud = StreamHud::new(StreamHandle::detached(s));
        let mut harness = Harness::builder()
            .with_size(egui::vec2(650.0, 750.0))
            .with_context_setup(theme::apply)
            .build_ui(|ui| {
                let area = ui.max_rect();
                let rect = hud.show(ui, area).unwrap();
                assert!(rect.height() > HUD_HEIGHT);
                assert!(rect.left() >= area.left() && rect.right() <= area.right());
            });
        harness.run_steps(3);
        assert!(harness.query_by_label("Decoder errors").is_none());
        harness.get_by_label("Detailed stats").click();
        harness.run_steps(4);
        harness.get_by_label("Decoder errors");
        harness.get_by_label("7");
        harness.get_by_label("Last 60 seconds · sampled once per second");
        harness.get_by_label("Video resyncs");
        harness.get_by_label_contains("RTT · ms");
        harness.get_by_label("RTT is network round-trip time, not input-to-screen latency. Gaps mean no measurement.");
    }

    #[test]
    fn hidden_hud_paints_nothing() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;

        let mut s = sample();
        s.hud_enabled = false;
        let hud = StreamHud::new(StreamHandle::detached(s));
        let mut harness = Harness::new_ui(|ui| {
            let area = ui.max_rect();
            assert!(hud.show(ui, area).is_none());
        });
        harness.run();
        assert!(harness.query_by_label_contains("fps").is_none());
    }
}
