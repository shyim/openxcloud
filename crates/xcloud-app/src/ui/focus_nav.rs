//! Directional keyboard focus, shared by the controller and the arrow keys.
//!
//! Widgets [`register`] themselves as targets while they are laid out; [`move_focus`]
//! then picks the nearest target in a direction and gives it egui's keyboard focus.
//! egui's own arrow-key search is not used (the shell takes the arrow keys before egui
//! sees them): it gets stuck between rows and lands on the buttons nested inside a
//! tile. [`end_frame`] rings the focused widget when it draws no ring of its own.
//!
//! Registration is explicit, so a new focusable widget must call [`register`]; the
//! `*_reachable_by_controller` tests fail for one that does not.

use egui::{Context, Id, Key, Rect, Response, Ui};

/// A direction focus can move in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

/// A widget the controller can move focus to, as laid out in one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Target {
    id: Id,
    rect: Rect,
    layer: egui::LayerId,
    /// Inside a scroll area (page content rather than chrome).
    scrolls: bool,
    kind: Kind,
}

/// How a target takes part in navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Normal,
    /// Left and right change the widget's value (a slider) instead of moving focus.
    Adjustable,
    /// A side link ("See all"): reachable, but anything else that way is preferred, so
    /// moving down a page runs through the games rather than the column of links.
    Secondary,
}

/// Score penalty of a [`Kind::Secondary`] target.
const SECONDARY_PENALTY: f32 = 600.0;

/// The targets of the frame being built and of the last complete frame. Navigation
/// happens between frames, so it searches the last complete one.
#[derive(Debug, Clone, Default)]
struct Targets {
    building: Vec<Target>,
    complete: Vec<Target>,
    /// The widget the controller just moved focus to; [`register`] scrolls it into
    /// view when it is laid out next.
    reveal: Option<Id>,
    /// The widget that drew its own focus ring, and in which frame.
    ring: Option<(Id, u64)>,
}

fn with_targets<R>(ctx: &Context, f: impl FnOnce(&mut Targets) -> R) -> R {
    ctx.data_mut(|d| f(d.get_temp_mut_or_default::<Targets>(Id::unique("focus_nav_targets"))))
}

/// Finishes the frame: its targets become the ones [`move_focus`] searches, and the
/// focused widget gets its ring. Call once after the frame's UI.
pub fn end_frame(ctx: &Context) {
    with_targets(ctx, |t| t.complete = std::mem::take(&mut t.building));
    paint_focus_ring(ctx);
}

/// Registers a widget as a controller navigation target and scrolls it into view when
/// the controller just moved focus to it.
pub fn register(ui: &Ui, response: &Response) {
    register_as(ui, response, Kind::Normal);
}

/// Like [`register`] for a widget whose value Left and Right adjust (a slider).
pub fn register_adjustable(ui: &Ui, response: &Response) {
    register_as(ui, response, Kind::Adjustable);
}

/// Like [`register`] for a side link that should not attract focus on the way past.
pub fn register_secondary(ui: &Ui, response: &Response) {
    register_as(ui, response, Kind::Secondary);
}

fn register_as(ui: &Ui, response: &Response, kind: Kind) {
    if !response.enabled() {
        return;
    }
    let scrolls = ui
        .stack()
        .iter()
        .any(|stack| stack.kind() == Some(egui::UiKind::ScrollArea));
    let reveal = with_targets(ui.ctx(), |t| {
        t.building.push(Target {
            id: response.id,
            rect: response.rect,
            layer: response.layer_id,
            scrolls,
            kind,
        });
        if t.reveal == Some(response.id) {
            t.reveal = None;
            true
        } else {
            false
        }
    });
    // Centring (rather than the minimal scroll) keeps the neighbours in every direction
    // on screen, which the virtualised lists need: a row that is not laid out cannot
    // take focus. Outside a scroll area the request would reach an unrelated one.
    if reveal && scrolls {
        response.scroll_to_me(Some(egui::Align::Center));
    }
}

/// How far `candidate` is from `current` when moving in `direction`; `None` when it does
/// not lie that way. Lower is better: the gap along the direction plus a heavier penalty
/// for being off to the side, so a neighbour in the same row or column wins.
fn score(current: Rect, candidate: Rect, direction: Direction) -> Option<f32> {
    // Work in a frame where the direction is +x.
    let along = |r: Rect| match direction {
        Direction::Right => (r.left(), r.right()),
        Direction::Left => (-r.right(), -r.left()),
        Direction::Down => (r.top(), r.bottom()),
        _ => (-r.bottom(), -r.top()),
    };
    let across = |r: Rect| match direction {
        Direction::Left | Direction::Right => (r.top(), r.bottom()),
        _ => (r.left(), r.right()),
    };
    let (cur_start, cur_end) = along(current);
    let (start, end) = along(candidate);
    let cur_mid = (cur_start + cur_end) / 2.0;
    // The candidate must begin past the current widget's middle and reach further.
    if start < cur_mid || end <= cur_end {
        return None;
    }
    let gap = (start - cur_end).max(0.0);
    let (cur_lo, cur_hi) = across(current);
    let (lo, hi) = across(candidate);
    let side = (lo - cur_hi).max(cur_lo - hi).max(0.0);
    // Among aligned candidates the one whose middle is closest across wins ties.
    let drift = ((lo + hi) / 2.0 - (cur_lo + cur_hi) / 2.0).abs();
    Some(gap + 3.0 * side + 0.01 * drift)
}

/// What a direction press does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    /// Focus moved (or there was nowhere to go).
    Handled,
    /// The focused widget wants the direction as this key (a slider's Left/Right).
    Key(Key),
}

/// Moves focus one step in `direction` among last frame's targets.
pub fn move_focus(ctx: &Context, direction: Direction) -> Move {
    let targets: Vec<Target> = with_targets(ctx, |t| t.complete.clone())
        .into_iter()
        .filter(|t| ctx.memory(|m| m.allows_interaction(t.layer)))
        .collect();
    let focused = ctx.memory(|m| m.focused());
    let current = focused.and_then(|id| {
        targets
            .iter()
            .find(|t| t.id == id)
            .copied()
            .map(|t| (t.rect, t.kind))
            .or_else(|| ctx.read_response(id).map(|r| (r.rect, Kind::Normal)))
    });
    let next = match current {
        Some((_, Kind::Adjustable)) if matches!(direction, Direction::Left | Direction::Right) => {
            return Move::Key(if direction == Direction::Left {
                Key::ArrowLeft
            } else {
                Key::ArrowRight
            });
        }
        Some((rect, _)) => targets
            .iter()
            .filter(|t| Some(t.id) != focused)
            .filter_map(|t| {
                let penalty = if t.kind == Kind::Secondary {
                    SECONDARY_PENALTY
                } else {
                    0.0
                };
                score(rect, t.rect, direction).map(|s| (s + penalty, t.id))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, id)| id),
        // Nothing focused yet: start at the top-left of the visible page content.
        None => {
            let screen = ctx.content_rect();
            let first = |scrolls: bool| {
                targets
                    .iter()
                    .filter(|t| t.scrolls == scrolls && screen.contains_rect(t.rect))
                    .min_by(|a, b| {
                        (a.rect.top(), a.rect.left())
                            .partial_cmp(&(b.rect.top(), b.rect.left()))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|t| t.id)
            };
            first(true).or_else(|| first(false))
        }
    };
    if let Some(id) = next {
        with_targets(ctx, |t| t.reveal = Some(id));
        ctx.memory_mut(|m| m.request_focus(id));
        ctx.request_repaint();
    }
    Move::Handled
}

/// Tells [`end_frame`] that the widget drew its own focus ring.
pub fn ring_drawn(ctx: &Context, id: Id) {
    let frame = ctx.cumulative_frame_nr();
    with_targets(ctx, |t| t.ring = Some((id, frame)));
}

/// Draws the focus ring around the focused widget when the widget did not draw one
/// itself.
fn paint_focus_ring(ctx: &Context) {
    let Some(id) = ctx.memory(|m| m.focused()) else {
        return;
    };
    let frame = ctx.cumulative_frame_nr();
    if with_targets(ctx, |t| t.ring) == Some((id, frame)) {
        return;
    }
    // Text fields show their own caret and frame.
    if ctx.text_edit_focused() {
        return;
    }
    let Some(response) = ctx.read_response(id) else {
        return;
    };
    let rect = response.rect.intersect(response.interact_rect);
    if !rect.is_positive() {
        return;
    }
    ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        Id::unique("focus_nav_ring"),
    ))
    .rect_stroke(
        rect.expand(2.0),
        egui::CornerRadius::same(10),
        crate::theme::focus_stroke(2.0),
        egui::StrokeKind::Outside,
    );
}

#[cfg(test)]
pub(crate) fn unreachable_by_controller<S>(h: &egui_kittest::Harness<'_, S>) -> Vec<String> {
    use egui_kittest::kittest::{NodeT, Queryable};
    let registered: Vec<u64> = with_targets(&h.ctx, |t| {
        t.complete.iter().map(|t| t.id.value()).collect()
    });
    let mut missing: Vec<String> = h
        .query_all(
            egui_kittest::kittest::By::new()
                .predicate(|n| n.data().supports_action(egui::accesskit::Action::Focus)),
        )
        .filter(|n| !registered.contains(&n.accesskit_node().locate().0.0))
        .map(|n| format!("{n:?}"))
        .collect();
    missing.sort();
    missing
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nearest_aligned_widget_wins() {
        let r = |x: f32, y: f32| Rect::from_min_size(egui::pos2(x, y), egui::vec2(100.0, 100.0));
        let current = r(200.0, 200.0);
        // Right: the neighbour in the row beats a closer one a row below.
        let neighbour = score(current, r(320.0, 200.0), Direction::Right).unwrap();
        let diagonal = score(current, r(310.0, 320.0), Direction::Right).unwrap();
        assert!(neighbour < diagonal);
        // Nothing behind or level with the current widget counts.
        assert_eq!(score(current, r(80.0, 200.0), Direction::Right), None);
        assert_eq!(score(current, r(200.0, 320.0), Direction::Right), None);
        assert!(score(current, r(200.0, 320.0), Direction::Down).is_some());
        assert!(score(current, r(80.0, 200.0), Direction::Left).is_some());
        assert!(score(current, r(200.0, 80.0), Direction::Up).is_some());
        // Off to the side costs more than distance straight ahead.
        let ahead = score(current, r(200.0, 600.0), Direction::Down).unwrap();
        let aside = score(current, r(500.0, 320.0), Direction::Down).unwrap();
        assert!(ahead < aside);
    }
}
