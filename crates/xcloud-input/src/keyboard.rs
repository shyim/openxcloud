//! Port of the keyboard virtual-controller tables in qt/qml/StreamView.qml
//! (`keyButtons`, `keyAxes` and `updateLeftStick`), documented in qt/README.md.
//!
//! Key names are the ones `StreamView.keyName()` produced: named keys as Qt spells
//! them (`Space`, `Return`, `Escape`, `Left`, `Shift`, ...) and printable keys as their
//! upper-cased text (`Q`, `W`, ...). The host maps its own key events to these names.

use xcloud_proto::input::models::Vec2;

/// What a key does on the virtual controller.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeyAction {
    /// A `GamepadInputSource::set_key` name, including `left-trigger` / `right-trigger`.
    Button(&'static str),
    /// Contribution to the left stick (GameController y-up). The host sums the
    /// contributions of every held key and calls `set_left_stick` (see
    /// [`left_stick_from_keys`]).
    LeftStick(Vec2),
}

/// `keyButtons[name]` / `keyAxes[name]`; `None` for keys the stream view ignores.
pub fn key_action(key_name: &str) -> Option<KeyAction> {
    use KeyAction::{Button, LeftStick};
    Some(match key_name {
        "Space" | "Return" | "Enter" => Button("a"),
        "Escape" | "Backspace" => Button("b"),
        "Q" => Button("x"),
        "E" => Button("y"),
        "Left" => Button("dpad-left"),
        "Right" => Button("dpad-right"),
        "Up" => Button("dpad-up"),
        "Down" => Button("dpad-down"),
        "Z" => Button("left-shoulder"),
        "C" => Button("right-shoulder"),
        "Shift" => Button("left-thumb"),
        "Control" => Button("right-thumb"),
        "Tab" => Button("menu"),
        "M" => Button("view"),
        "R" => Button("left-trigger"),
        "F" => Button("right-trigger"),
        "W" => LeftStick(Vec2::new(0.0, 1.0)),
        "S" => LeftStick(Vec2::new(0.0, -1.0)),
        "A" => LeftStick(Vec2::new(-1.0, 0.0)),
        "D" => LeftStick(Vec2::new(1.0, 0.0)),
        _ => return None,
    })
}

/// `StreamView.updateLeftStick()`: the summed stick deflection of every held key.
/// Opposite keys cancel; `GamepadInputSource::set_left_stick` clamps the result.
pub fn left_stick_from_keys<'a>(held: impl IntoIterator<Item = &'a str>) -> Vec2 {
    held.into_iter()
        .filter_map(|key| match key_action(key) {
            Some(KeyAction::LeftStick(axis)) => Some(axis),
            _ => None,
        })
        .fold(Vec2::ZERO, |acc, axis| {
            Vec2::new(acc.x + axis.x, acc.y + axis.y)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::button_from_name;
    use xcloud_proto::input::models::GamepadButtons;

    fn button(key: &str) -> &'static str {
        match key_action(key) {
            Some(KeyAction::Button(name)) => name,
            other => panic!("{key}: expected a button, got {other:?}"),
        }
    }

    // One test per row of the qt/README.md table.

    #[test]
    fn space_or_return_is_a() {
        assert_eq!(button("Space"), "a");
        assert_eq!(button("Return"), "a");
        assert_eq!(button("Enter"), "a");
        assert_eq!(button_from_name(button("Space")), Some(GamepadButtons::A));
    }

    #[test]
    fn escape_or_backspace_is_b() {
        assert_eq!(button("Escape"), "b");
        assert_eq!(button("Backspace"), "b");
        assert_eq!(button_from_name("b"), Some(GamepadButtons::B));
    }

    #[test]
    fn q_is_x() {
        assert_eq!(button("Q"), "x");
        assert_eq!(button_from_name("x"), Some(GamepadButtons::X));
    }

    #[test]
    fn e_is_y() {
        assert_eq!(button("E"), "y");
        assert_eq!(button_from_name("y"), Some(GamepadButtons::Y));
    }

    #[test]
    fn arrow_keys_are_dpad() {
        assert_eq!(button("Left"), "dpad-left");
        assert_eq!(button("Right"), "dpad-right");
        assert_eq!(button("Up"), "dpad-up");
        assert_eq!(button("Down"), "dpad-down");
        assert_eq!(
            button_from_name("dpad-left"),
            Some(GamepadButtons::DPAD_LEFT)
        );
        assert_eq!(
            button_from_name("dpad-right"),
            Some(GamepadButtons::DPAD_RIGHT)
        );
        assert_eq!(button_from_name("dpad-up"), Some(GamepadButtons::DPAD_UP));
        assert_eq!(
            button_from_name("dpad-down"),
            Some(GamepadButtons::DPAD_DOWN)
        );
    }

    #[test]
    fn wasd_is_left_stick() {
        assert_eq!(
            key_action("W"),
            Some(KeyAction::LeftStick(Vec2::new(0.0, 1.0)))
        );
        assert_eq!(
            key_action("S"),
            Some(KeyAction::LeftStick(Vec2::new(0.0, -1.0)))
        );
        assert_eq!(
            key_action("A"),
            Some(KeyAction::LeftStick(Vec2::new(-1.0, 0.0)))
        );
        assert_eq!(
            key_action("D"),
            Some(KeyAction::LeftStick(Vec2::new(1.0, 0.0)))
        );
    }

    #[test]
    fn z_and_c_are_shoulders() {
        assert_eq!(button("Z"), "left-shoulder");
        assert_eq!(button("C"), "right-shoulder");
        assert_eq!(
            button_from_name("left-shoulder"),
            Some(GamepadButtons::LEFT_SHOULDER)
        );
        assert_eq!(
            button_from_name("right-shoulder"),
            Some(GamepadButtons::RIGHT_SHOULDER)
        );
    }

    #[test]
    fn shift_and_control_are_stick_clicks() {
        assert_eq!(button("Shift"), "left-thumb");
        assert_eq!(button("Control"), "right-thumb");
        assert_eq!(
            button_from_name("left-thumb"),
            Some(GamepadButtons::LEFT_THUMB)
        );
        assert_eq!(
            button_from_name("right-thumb"),
            Some(GamepadButtons::RIGHT_THUMB)
        );
    }

    #[test]
    fn tab_is_menu() {
        assert_eq!(button("Tab"), "menu");
        assert_eq!(button_from_name("menu"), Some(GamepadButtons::MENU));
    }

    #[test]
    fn m_is_view() {
        assert_eq!(button("M"), "view");
        assert_eq!(button_from_name("view"), Some(GamepadButtons::VIEW));
    }

    #[test]
    fn r_and_f_are_triggers() {
        assert_eq!(button("R"), "left-trigger");
        assert_eq!(button("F"), "right-trigger");
    }

    #[test]
    fn unmapped_keys_are_ignored() {
        assert_eq!(key_action("P"), None);
        assert_eq!(key_action(""), None);
        assert_eq!(key_action("space"), None, "names are the QML spellings");
    }

    #[test]
    fn left_stick_sums_held_keys() {
        assert_eq!(left_stick_from_keys([]), Vec2::ZERO);
        assert_eq!(left_stick_from_keys(["W"]), Vec2::new(0.0, 1.0));
        assert_eq!(left_stick_from_keys(["W", "D"]), Vec2::new(1.0, 1.0));
        assert_eq!(left_stick_from_keys(["W", "S"]), Vec2::ZERO);
        assert_eq!(
            left_stick_from_keys(["A", "Space", "S"]),
            Vec2::new(-1.0, -1.0)
        );
    }

    #[test]
    fn every_button_key_names_a_setkey_target() {
        for key in [
            "Space",
            "Return",
            "Enter",
            "Escape",
            "Backspace",
            "Q",
            "E",
            "Left",
            "Right",
            "Up",
            "Down",
            "Z",
            "C",
            "Shift",
            "Control",
            "Tab",
            "M",
        ] {
            assert!(button_from_name(button(key)).is_some(), "{key}");
        }
    }
}
