//! Port of qt/src/streaming/GamepadHandler.{h,cpp}
//!
//! Converts platform-agnostic raw controller state into the Xbox input frame
//! (deadzone, trigger interpretation, sensitivity, A/B swap, Y inversion) and
//! recognises held-button chords for stream-level shortcuts.

use std::collections::HashMap;

use super::models::{
    ControllerSettings, GamepadButtons, GamepadInputFrame, TriggerInterpretationMode, Vec2,
};

/// Platform-agnostic controller state sampled by an input source (gilrs, keyboard, ...).
/// Y axes use GameController semantics: up is +1, down is -1.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RawGamepadState {
    pub buttons: GamepadButtons,
    pub left_thumb: Vec2,
    pub right_thumb: Vec2,
    pub left_trigger: f32,
    pub right_trigger: f32,
    pub left_trigger_pressed: bool,
    pub right_trigger_pressed: bool,
}

/// Stream-level shortcuts synthesised from controller chords.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShortcutAction {
    ToggleStatsHud,
    ToggleStreamMenu,
    DisconnectStream,
    MuteToggle,
}

/// A held-button combination that emits a single shortcut action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChordDefinition {
    pub buttons: GamepadButtons,
    pub hold_duration_ms: u32,
    pub action: ShortcutAction,
}

impl ChordDefinition {
    pub const fn new(
        buttons: GamepadButtons,
        hold_duration_ms: u32,
        action: ShortcutAction,
    ) -> Self {
        Self {
            buttons,
            hold_duration_ms,
            action,
        }
    }
}

/// Tracks chord hold timing so a shortcut fires once per sustained press.
///
/// State is keyed by [`ShortcutAction`] (the C++ keys by `int(action)`), so two
/// definitions sharing an action also share hold state, exactly as in Qt.
#[derive(Debug, Clone, Default)]
pub struct ChordRecognizer {
    definitions: Vec<ChordDefinition>,
    started_at: HashMap<ShortcutAction, u64>,
    fired: HashMap<ShortcutAction, bool>,
}

impl ChordRecognizer {
    pub fn new(definitions: Vec<ChordDefinition>) -> Self {
        Self {
            definitions,
            ..Self::default()
        }
    }

    pub fn definitions(&self) -> &[ChordDefinition] {
        &self.definitions
    }

    pub fn set_definitions(&mut self, definitions: Vec<ChordDefinition>) {
        self.definitions = definitions;
    }

    /// Feeds one sampled frame at monotonic time `now_ms`; returns the actions
    /// whose chord has just reached its hold duration.
    pub fn process(&mut self, frame: &GamepadInputFrame, now_ms: u64) -> Vec<ShortcutAction> {
        let mut actions = Vec::new();
        for definition in &self.definitions {
            let matched = (frame.buttons & definition.buttons) == definition.buttons;
            let key = definition.action;
            if matched {
                let start = *self.started_at.entry(key).or_insert(now_ms);
                let elapsed = now_ms.saturating_sub(start);
                let already_fired = self.fired.get(&key).copied().unwrap_or(false);
                if !already_fired && elapsed >= u64::from(definition.hold_duration_ms) {
                    actions.push(definition.action);
                    self.fired.insert(key, true);
                }
            } else {
                self.started_at.remove(&key);
                self.fired.remove(&key);
            }
        }
        actions
    }

    /// Forgets all hold state (e.g. when the controller disconnects).
    pub fn reset(&mut self) {
        self.started_at.clear();
        self.fired.clear();
    }
}

/// Converts sampled raw state into the binary frame used by the Xbox input protocol.
#[derive(Debug, Clone)]
pub struct GamepadHandler {
    gamepad_index: u8,
    left_auto_analog: bool,
    right_auto_analog: bool,
}

impl Default for GamepadHandler {
    fn default() -> Self {
        Self::new(0)
    }
}

impl GamepadHandler {
    pub fn new(gamepad_index: u8) -> Self {
        Self {
            gamepad_index,
            left_auto_analog: true,
            right_auto_analog: true,
        }
    }

    pub fn gamepad_index(&self) -> u8 {
        self.gamepad_index
    }

    /// Radial-per-axis deadzone: values below `deadzone` are zeroed, the
    /// remainder is rescaled to `0.0..=1.0`.
    pub fn apply_deadzone(value: f32, deadzone: f32) -> f32 {
        let magnitude = value.abs();
        if magnitude < deadzone {
            return 0.0;
        }
        let sign = if value >= 0.0 { 1.0 } else { -1.0 };
        sign * (magnitude - deadzone) / (1.0 - deadzone.max(0.0001))
    }

    /// `Auto` mode: learn per side whether the controller provides analog
    /// travel. Mid-range values are evidence of analog hardware; a digital
    /// press with (near) zero travel is evidence of a digital-only trigger.
    fn resolve_auto_trigger_raw(&mut self, raw: f32, is_pressed: bool, left_side: bool) -> f32 {
        const DIGITAL_PRESSED_EPSILON: f32 = 0.02;
        const ANALOG_EVIDENCE_MIN: f32 = 0.05;
        const ANALOG_EVIDENCE_MAX: f32 = 0.95;

        let mut analog_mode = if left_side {
            self.left_auto_analog
        } else {
            self.right_auto_analog
        };

        if raw > ANALOG_EVIDENCE_MIN && raw < ANALOG_EVIDENCE_MAX {
            analog_mode = true;
        } else if is_pressed && raw <= DIGITAL_PRESSED_EPSILON {
            analog_mode = false;
        }

        if left_side {
            self.left_auto_analog = analog_mode;
        } else {
            self.right_auto_analog = analog_mode;
        }

        if analog_mode {
            return raw;
        }
        raw.max(if is_pressed { 1.0 } else { 0.0 })
    }

    /// Trigger interpretation resolved for one side; exposed for tests.
    pub fn resolve_trigger_raw(
        &mut self,
        raw_value: f32,
        is_pressed: bool,
        left_side: bool,
        settings: &ControllerSettings,
    ) -> f32 {
        let clamped = raw_value.clamp(0.0, 1.0);
        match settings.trigger_interpretation_mode {
            TriggerInterpretationMode::AnalogOnly => clamped,
            TriggerInterpretationMode::DigitalFallback => {
                clamped.max(if is_pressed { 1.0 } else { 0.0 })
            }
            TriggerInterpretationMode::Auto => {
                self.resolve_auto_trigger_raw(clamped, is_pressed, left_side)
            }
        }
    }

    /// Applies deadzone, trigger interpretation, sensitivity, swap and invert.
    pub fn read_frame(
        &mut self,
        state: &RawGamepadState,
        settings: &ControllerSettings,
    ) -> GamepadInputFrame {
        let mut buttons = state.buttons;

        if settings.swap_ab {
            let has_a = buttons.contains(GamepadButtons::A);
            let has_b = buttons.contains(GamepadButtons::B);
            buttons.set(GamepadButtons::A, has_b);
            buttons.set(GamepadButtons::B, has_a);
        }

        let sensitivity = settings.trigger_sensitivity.max(0.01);
        let left_raw = self.resolve_trigger_raw(
            state.left_trigger,
            state.left_trigger_pressed,
            true,
            settings,
        );
        let right_raw = self.resolve_trigger_raw(
            state.right_trigger,
            state.right_trigger_pressed,
            false,
            settings,
        );
        let triggers = Vec2 {
            x: (left_raw / sensitivity).min(1.0),
            y: (right_raw / sensitivity).min(1.0),
        };

        // GameController reports up as +1; the browser/xCloud protocol uses up as -1.
        let left_x = Self::apply_deadzone(state.left_thumb.x, settings.deadzone);
        let mut left_y = -Self::apply_deadzone(state.left_thumb.y, settings.deadzone);
        let right_x = Self::apply_deadzone(state.right_thumb.x, settings.deadzone);
        let mut right_y = -Self::apply_deadzone(state.right_thumb.y, settings.deadzone);

        if settings.invert_y {
            left_y = -left_y;
            right_y = -right_y;
        }

        GamepadInputFrame {
            gamepad_index: self.gamepad_index,
            buttons,
            left_thumb: Vec2 {
                x: left_x,
                y: left_y,
            },
            right_thumb: Vec2 {
                x: right_x,
                y: right_y,
            },
            triggers,
        }
    }

    /// A frame with no buttons and centred axes for this gamepad index.
    pub fn idle_frame(&self) -> GamepadInputFrame {
        GamepadInputFrame {
            gamepad_index: self.gamepad_index,
            ..GamepadInputFrame::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- tst_gamepadhandler.cpp ----------------------------------------------

    #[test]
    fn deadzone_zeroes_small_input() {
        assert_eq!(GamepadHandler::apply_deadzone(0.1, 0.15), 0.0);
    }

    #[test]
    fn deadzone_rescales_remainder() {
        // (0.575 - 0.15) / 0.85 = 0.5
        assert!((GamepadHandler::apply_deadzone(0.575, 0.15) - 0.5).abs() < 0.0001);
    }

    #[test]
    fn y_axis_is_inverted() {
        let mut handler = GamepadHandler::new(0);
        let state = RawGamepadState {
            left_thumb: Vec2 { x: 0.0, y: 1.0 }, // GameController: up is +1
            ..RawGamepadState::default()
        };
        let settings = ControllerSettings {
            deadzone: 0.15,
            ..ControllerSettings::default()
        };
        let frame = handler.read_frame(&state, &settings);
        assert_eq!(frame.left_thumb.y, -1.0);
    }

    #[test]
    fn invert_y_flips_axis() {
        let mut handler = GamepadHandler::new(0);
        let state = RawGamepadState {
            left_thumb: Vec2 { x: 0.0, y: 1.0 },
            ..RawGamepadState::default()
        };
        let settings = ControllerSettings {
            deadzone: 0.15,
            invert_y: true,
            ..ControllerSettings::default()
        };
        let frame = handler.read_frame(&state, &settings);
        assert_eq!(frame.left_thumb.y, 1.0);
    }

    #[test]
    fn swap_ab_swaps_face_buttons() {
        let mut handler = GamepadHandler::new(0);
        let state = RawGamepadState {
            buttons: GamepadButtons::A,
            ..RawGamepadState::default()
        };
        let settings = ControllerSettings {
            swap_ab: true,
            ..ControllerSettings::default()
        };
        let frame = handler.read_frame(&state, &settings);
        assert!(frame.buttons.contains(GamepadButtons::B));
        assert!(!frame.buttons.contains(GamepadButtons::A));
    }

    #[test]
    fn trigger_sensitivity_scales() {
        let mut handler = GamepadHandler::new(0);
        let state = RawGamepadState {
            left_trigger: 0.25,
            left_trigger_pressed: true,
            ..RawGamepadState::default()
        };
        let settings = ControllerSettings {
            trigger_sensitivity: 0.5,
            trigger_interpretation_mode: TriggerInterpretationMode::AnalogOnly,
            ..ControllerSettings::default()
        };
        let frame = handler.read_frame(&state, &settings);
        assert!((frame.triggers.x - 0.5).abs() < 0.001);
    }

    #[test]
    fn digital_trigger_fallback() {
        let mut handler = GamepadHandler::new(0);
        let settings = ControllerSettings {
            trigger_interpretation_mode: TriggerInterpretationMode::DigitalFallback,
            ..ControllerSettings::default()
        };
        assert_eq!(handler.resolve_trigger_raw(0.0, true, true, &settings), 1.0);
        assert_eq!(
            handler.resolve_trigger_raw(0.0, false, true, &settings),
            0.0
        );
    }

    #[test]
    fn chord_fires_once_after_hold() {
        let mut recognizer = ChordRecognizer::new(vec![ChordDefinition::new(
            GamepadButtons::MENU | GamepadButtons::VIEW,
            500,
            ShortcutAction::ToggleStatsHud,
        )]);
        let frame = GamepadInputFrame {
            buttons: GamepadButtons::MENU | GamepadButtons::VIEW,
            ..GamepadInputFrame::default()
        };

        assert!(recognizer.process(&frame, 1000).is_empty());
        assert_eq!(recognizer.process(&frame, 1600).len(), 1);
        // Still held: must not fire again.
        assert!(recognizer.process(&frame, 2200).is_empty());

        // Released resets the recognizer.
        let released = GamepadInputFrame::default();
        recognizer.process(&released, 2300);
        assert!(recognizer.process(&frame, 2400).is_empty());
        assert_eq!(recognizer.process(&frame, 3000).len(), 1);
    }

    // --- additional coverage (no Qt counterpart) ----------------------------

    #[test]
    fn chord_matches_superset_of_buttons() {
        let mut recognizer = ChordRecognizer::new(vec![ChordDefinition::new(
            GamepadButtons::MENU | GamepadButtons::VIEW,
            0,
            ShortcutAction::ToggleStreamMenu,
        )]);
        let frame = GamepadInputFrame {
            buttons: GamepadButtons::MENU | GamepadButtons::VIEW | GamepadButtons::A,
            ..GamepadInputFrame::default()
        };
        assert_eq!(
            recognizer.process(&frame, 10),
            vec![ShortcutAction::ToggleStreamMenu]
        );
        // Dropping one chord button releases it.
        let partial = GamepadInputFrame {
            buttons: GamepadButtons::MENU,
            ..GamepadInputFrame::default()
        };
        assert!(recognizer.process(&partial, 20).is_empty());
        assert_eq!(recognizer.process(&frame, 30).len(), 1);
    }

    #[test]
    fn chord_zero_hold_fires_immediately_and_reset_clears_state() {
        let mut recognizer = ChordRecognizer::new(vec![ChordDefinition::new(
            GamepadButtons::NEXUS,
            0,
            ShortcutAction::MuteToggle,
        )]);
        let frame = GamepadInputFrame {
            buttons: GamepadButtons::NEXUS,
            ..GamepadInputFrame::default()
        };
        assert_eq!(recognizer.process(&frame, 5).len(), 1);
        assert!(recognizer.process(&frame, 6).is_empty());
        recognizer.reset();
        assert_eq!(recognizer.process(&frame, 7).len(), 1);
    }

    #[test]
    fn auto_trigger_learns_digital_then_analog() {
        let mut handler = GamepadHandler::new(0);
        let settings = ControllerSettings::default(); // Auto

        // Starts in analog mode: a digital press with zero travel flips to digital
        // and the press counts as a full pull.
        assert_eq!(handler.resolve_trigger_raw(0.0, true, true, &settings), 1.0);
        // Still digital for the left side while no analog evidence arrives.
        assert_eq!(
            handler.resolve_trigger_raw(0.0, false, true, &settings),
            0.0
        );
        assert_eq!(
            handler.resolve_trigger_raw(0.01, true, true, &settings),
            1.0
        );
        // Mid-range travel is analog evidence: back to analog, pressed flag ignored.
        assert_eq!(handler.resolve_trigger_raw(0.5, true, true, &settings), 0.5);
        // A fully pulled trigger is not evidence either way: stays analog.
        assert_eq!(handler.resolve_trigger_raw(1.0, true, true, &settings), 1.0);
        assert_eq!(
            handler.resolve_trigger_raw(0.0, false, true, &settings),
            0.0
        );

        // Sides are independent: the right side was never taught digital.
        assert_eq!(
            handler.resolve_trigger_raw(0.0, false, false, &settings),
            0.0
        );
        assert_eq!(
            handler.resolve_trigger_raw(0.0, true, false, &settings),
            1.0
        );
        assert_eq!(
            handler.resolve_trigger_raw(0.0, false, true, &settings),
            0.0
        );
    }

    #[test]
    fn auto_trigger_pressed_in_analog_mode_keeps_raw_value() {
        let mut handler = GamepadHandler::new(0);
        let settings = ControllerSettings::default();
        // Pressed with large travel: analog mode retained, raw passes through.
        assert_eq!(
            handler.resolve_trigger_raw(0.97, true, false, &settings),
            0.97
        );
        // Pressed with tiny travel above the epsilon: no digital evidence, raw passes through.
        assert_eq!(
            handler.resolve_trigger_raw(0.03, true, false, &settings),
            0.03
        );
    }

    #[test]
    fn trigger_values_are_clamped_before_interpretation() {
        let mut handler = GamepadHandler::new(0);
        let settings = ControllerSettings {
            trigger_interpretation_mode: TriggerInterpretationMode::AnalogOnly,
            ..ControllerSettings::default()
        };
        assert_eq!(
            handler.resolve_trigger_raw(1.5, false, true, &settings),
            1.0
        );
        assert_eq!(
            handler.resolve_trigger_raw(-0.5, false, true, &settings),
            0.0
        );
    }

    #[test]
    fn sensitivity_output_saturates_at_one() {
        let mut handler = GamepadHandler::new(0);
        let state = RawGamepadState {
            left_trigger: 0.8,
            right_trigger: 1.0,
            ..RawGamepadState::default()
        };
        let settings = ControllerSettings {
            trigger_sensitivity: 0.5,
            trigger_interpretation_mode: TriggerInterpretationMode::AnalogOnly,
            ..ControllerSettings::default()
        };
        let frame = handler.read_frame(&state, &settings);
        assert_eq!(frame.triggers.x, 1.0);
        assert_eq!(frame.triggers.y, 1.0);

        // A sensitivity of 0 is clamped to 0.01 rather than dividing by zero.
        let settings = ControllerSettings {
            trigger_sensitivity: 0.0,
            trigger_interpretation_mode: TriggerInterpretationMode::AnalogOnly,
            ..ControllerSettings::default()
        };
        let frame = handler.read_frame(&state, &settings);
        assert_eq!(frame.triggers.x, 1.0);
    }

    #[test]
    fn deadzone_is_symmetric_and_handles_edge_values() {
        assert_eq!(GamepadHandler::apply_deadzone(-0.1, 0.15), 0.0);
        assert!((GamepadHandler::apply_deadzone(-0.575, 0.15) + 0.5).abs() < 0.0001);
        assert_eq!(GamepadHandler::apply_deadzone(1.0, 0.15), 1.0);
        assert_eq!(GamepadHandler::apply_deadzone(-1.0, 0.15), -1.0);
        // Zero deadzone is a pass-through (guarded against divide by zero).
        assert!((GamepadHandler::apply_deadzone(0.5, 0.0) - 0.5).abs() < 0.0001);
        assert_eq!(GamepadHandler::apply_deadzone(0.0, 0.0), 0.0);
    }

    #[test]
    fn read_frame_applies_deadzone_to_every_axis_and_keeps_index() {
        let mut handler = GamepadHandler::new(2);
        let state = RawGamepadState {
            buttons: GamepadButtons::X | GamepadButtons::DPAD_UP,
            left_thumb: Vec2 { x: 0.1, y: -0.1 },
            right_thumb: Vec2 { x: 0.575, y: -1.0 },
            ..RawGamepadState::default()
        };
        let frame = handler.read_frame(&state, &ControllerSettings::default());
        assert_eq!(frame.gamepad_index, 2);
        assert_eq!(frame.buttons, GamepadButtons::X | GamepadButtons::DPAD_UP);
        assert_eq!(frame.left_thumb, Vec2 { x: 0.0, y: 0.0 });
        assert!((frame.right_thumb.x - 0.5).abs() < 0.0001);
        // GameController down (-1) becomes protocol down (+1).
        assert_eq!(frame.right_thumb.y, 1.0);
        assert_eq!(frame.triggers, Vec2::ZERO);
    }

    #[test]
    fn swap_ab_handles_both_and_neither() {
        let mut handler = GamepadHandler::new(0);
        let settings = ControllerSettings {
            swap_ab: true,
            ..ControllerSettings::default()
        };
        let both = RawGamepadState {
            buttons: GamepadButtons::A | GamepadButtons::B | GamepadButtons::Y,
            ..RawGamepadState::default()
        };
        let frame = handler.read_frame(&both, &settings);
        assert_eq!(
            frame.buttons,
            GamepadButtons::A | GamepadButtons::B | GamepadButtons::Y
        );
        let only_b = RawGamepadState {
            buttons: GamepadButtons::B,
            ..RawGamepadState::default()
        };
        let frame = handler.read_frame(&only_b, &settings);
        assert_eq!(frame.buttons, GamepadButtons::A);
    }

    #[test]
    fn idle_frame_carries_gamepad_index() {
        let handler = GamepadHandler::new(3);
        assert_eq!(handler.gamepad_index(), 3);
        let frame = handler.idle_frame();
        assert_eq!(frame.gamepad_index, 3);
        assert_eq!(frame.buttons, GamepadButtons::NONE);
        assert_eq!(frame.left_thumb, Vec2::ZERO);
        assert_eq!(frame.right_thumb, Vec2::ZERO);
        assert_eq!(frame.triggers, Vec2::ZERO);
    }
}
