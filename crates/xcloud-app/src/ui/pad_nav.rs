//! Controller input for the UI outside a stream.
//!
//! [`PadNav`] turns successive controller samples into [`NavAction`]s: buttons on their
//! press edge, directions on the edge and then repeating while held. The shell applies
//! them: directions move keyboard focus (`focus_nav`), A and B become the Enter and
//! Escape keys the widgets already react to.
//!
//! | controller          | UI                                             |
//! |---------------------|------------------------------------------------|
//! | D-pad / left stick  | move focus to the nearest widget that way      |
//! | A                   | Enter: activate the focused widget             |
//! | B                   | Escape: back, close the settings sheet         |
//! | Menu                | open or close the settings sheet               |

use egui::{Event, Key, Modifiers};
use xcloud_proto::input::gamepad::RawGamepadState;
use xcloud_proto::input::models::GamepadButtons;

use crate::ui::focus_nav::Direction;

/// Stick deflection that counts as a direction.
const STICK_THRESHOLD: f32 = 0.6;
/// Delay before a held direction starts repeating, then the repeat interval (seconds).
const REPEAT_DELAY: f64 = 0.40;
const REPEAT_INTERVAL: f64 = 0.11;

/// One navigation step produced by the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavAction {
    /// D-pad or left stick: move focus.
    Move(Direction),
    /// A: activate the focused widget.
    Accept,
    /// B: go back / close.
    Back,
    /// Menu: toggle the settings sheet.
    Menu,
}

/// The direction the D-pad or the left stick points at, if any. The D-pad wins; on the
/// stick the dominant axis decides, so a diagonal never yields two steps.
fn direction(state: &RawGamepadState) -> Option<Direction> {
    let b = state.buttons;
    if b.contains(GamepadButtons::DPAD_UP) {
        return Some(Direction::Up);
    }
    if b.contains(GamepadButtons::DPAD_DOWN) {
        return Some(Direction::Down);
    }
    if b.contains(GamepadButtons::DPAD_LEFT) {
        return Some(Direction::Left);
    }
    if b.contains(GamepadButtons::DPAD_RIGHT) {
        return Some(Direction::Right);
    }
    let (x, y) = (state.left_thumb.x, state.left_thumb.y);
    if x.abs().max(y.abs()) < STICK_THRESHOLD {
        return None;
    }
    // Raw sticks are up-positive.
    Some(if x.abs() > y.abs() {
        if x > 0.0 {
            Direction::Right
        } else {
            Direction::Left
        }
    } else if y > 0.0 {
        Direction::Up
    } else {
        Direction::Down
    })
}

/// Whether `state` holds anything navigation reacts to.
pub fn is_engaged(state: &RawGamepadState) -> bool {
    direction(state).is_some()
        || [GamepadButtons::A, GamepadButtons::B, GamepadButtons::MENU]
            .into_iter()
            .any(|bit| state.buttons.contains(bit))
}

/// Turns controller samples into navigation steps: buttons on their press edge,
/// directions on the edge and then repeating while held.
#[derive(Debug, Default)]
pub struct PadNav {
    buttons: GamepadButtons,
    held: Option<Direction>,
    next_repeat: f64,
    /// Ignore everything until the controller is back at rest (after a stream ends or
    /// the window regains focus, whatever is still held belongs to the previous owner).
    wait_for_rest: bool,
}

impl PadNav {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops held state; input is ignored until the controller is at rest once.
    pub fn suspend(&mut self) {
        *self = Self {
            wait_for_rest: true,
            ..Self::default()
        };
    }

    /// When the held direction repeats next (for scheduling a repaint).
    pub fn repeat_due(&self, now: f64) -> Option<f64> {
        self.held.map(|_| (self.next_repeat - now).max(0.0))
    }

    /// Feeds one sample taken at `now` (seconds) and returns the steps it triggers.
    pub fn update(&mut self, state: &RawGamepadState, now: f64) -> Vec<NavAction> {
        if self.wait_for_rest {
            if is_engaged(state) {
                return Vec::new();
            }
            self.wait_for_rest = false;
        }
        let mut actions = Vec::new();
        for (bit, action) in [
            (GamepadButtons::A, NavAction::Accept),
            (GamepadButtons::B, NavAction::Back),
            (GamepadButtons::MENU, NavAction::Menu),
        ] {
            if state.buttons.contains(bit) && !self.buttons.contains(bit) {
                actions.push(action);
            }
        }
        self.buttons = state.buttons;

        let direction = direction(state);
        if direction != self.held {
            self.held = direction;
            self.next_repeat = now + REPEAT_DELAY;
            actions.extend(direction.map(NavAction::Move));
        } else if direction.is_some() && now >= self.next_repeat {
            self.next_repeat = now + REPEAT_INTERVAL;
            actions.extend(direction.map(NavAction::Move));
        }
        actions
    }
}

/// A key press followed by its release, as egui expects them from a keyboard.
pub fn key_tap(key: Key) -> [Event; 2] {
    [true, false].map(|pressed| Event::Key {
        key,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers: Modifiers::NONE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use xcloud_proto::input::models::Vec2;

    fn buttons(bits: GamepadButtons) -> RawGamepadState {
        RawGamepadState {
            buttons: bits,
            ..RawGamepadState::default()
        }
    }

    fn stick(x: f32, y: f32) -> RawGamepadState {
        RawGamepadState {
            left_thumb: Vec2::new(x, y),
            ..RawGamepadState::default()
        }
    }

    #[test]
    fn buttons_fire_once_per_press() {
        let mut nav = PadNav::new();
        let a = buttons(GamepadButtons::A);
        assert_eq!(nav.update(&a, 0.0), [NavAction::Accept]);
        assert!(nav.update(&a, 1.0).is_empty(), "held A does not repeat");
        assert!(nav.update(&RawGamepadState::default(), 1.1).is_empty());
        assert_eq!(
            nav.update(&buttons(GamepadButtons::B | GamepadButtons::MENU), 1.2),
            [NavAction::Back, NavAction::Menu]
        );
    }

    #[test]
    fn directions_repeat_while_held() {
        let mut nav = PadNav::new();
        let down = buttons(GamepadButtons::DPAD_DOWN);
        assert_eq!(nav.update(&down, 0.0), [NavAction::Move(Direction::Down)]);
        assert!(nav.update(&down, 0.2).is_empty(), "before the repeat delay");
        assert_eq!(
            nav.update(&down, REPEAT_DELAY),
            [NavAction::Move(Direction::Down)]
        );
        assert!(nav.update(&down, REPEAT_DELAY + 0.05).is_empty());
        assert_eq!(
            nav.update(&down, REPEAT_DELAY + REPEAT_INTERVAL),
            [NavAction::Move(Direction::Down)]
        );
        assert!(nav.repeat_due(REPEAT_DELAY + REPEAT_INTERVAL).is_some());
        assert!(nav.update(&RawGamepadState::default(), 2.0).is_empty());
        assert_eq!(nav.repeat_due(2.0), None);
    }

    #[test]
    fn the_stick_steps_along_its_dominant_axis() {
        let mut nav = PadNav::new();
        assert!(
            nav.update(&stick(0.3, 0.4), 0.0).is_empty(),
            "inside the threshold"
        );
        assert_eq!(
            nav.update(&stick(0.2, 0.9), 0.1),
            [NavAction::Move(Direction::Up)]
        );
        assert_eq!(
            nav.update(&stick(0.9, 0.5), 0.2),
            [NavAction::Move(Direction::Right)]
        );
        assert_eq!(
            nav.update(&stick(-0.9, 0.0), 0.3),
            [NavAction::Move(Direction::Left)]
        );
        assert_eq!(
            nav.update(&stick(0.0, -0.9), 0.4),
            [NavAction::Move(Direction::Down)]
        );
    }

    #[test]
    fn suspended_navigation_waits_for_the_controller_to_rest() {
        let mut nav = PadNav::new();
        nav.suspend();
        let held = buttons(GamepadButtons::A | GamepadButtons::DPAD_LEFT);
        assert!(nav.update(&held, 0.0).is_empty());
        assert!(nav.update(&held, 5.0).is_empty(), "still the old press");
        assert!(nav.update(&RawGamepadState::default(), 5.1).is_empty());
        assert_eq!(
            nav.update(&buttons(GamepadButtons::A), 5.2),
            [NavAction::Accept]
        );
    }

    #[test]
    fn key_taps_press_then_release() {
        let [down, up] = key_tap(Key::Enter);
        assert!(matches!(
            down,
            Event::Key {
                key: Key::Enter,
                pressed: true,
                ..
            }
        ));
        assert!(matches!(
            up,
            Event::Key {
                key: Key::Enter,
                pressed: false,
                ..
            }
        ));
    }
}
