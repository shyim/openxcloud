//! Port of qt/src/app/NativeGamepad.{h,mm,cpp}: the physical-controller sampler.
//!
//! The Qt app read Apple's GameController framework (`NativeGamepad.mm`); every other
//! platform compiled the stub returning "no controller" (`NativeGamepad.cpp`). This port
//! uses gilrs everywhere gilrs builds (IOKit on macOS, XInput/evdev elsewhere) and keeps
//! the stub for iOS. The mapping to [`RawGamepadState`] is the one `NativeGamepad.mm`
//! produced:
//!
//! | GameController          | gilrs                         | protocol              |
//! |-------------------------|-------------------------------|-----------------------|
//! | buttonA/B/X/Y           | South/East/West/North         | A/B/X/Y               |
//! | buttonMenu              | Start                         | Menu                  |
//! | buttonOptions           | Select                        | View                  |
//! | buttonHome              | Mode                          | Nexus                 |
//! | left/rightShoulder      | LeftTrigger/RightTrigger      | Left/RightShoulder    |
//! | left/rightThumbstickBtn | LeftThumb/RightThumb          | Left/RightThumb       |
//! | dpad.up/down/left/right | DPadUp/Down/Left/Right        | Dpad*                 |
//! | thumbstick x/y (y-up)   | LeftStickX/Y, RightStickX/Y   | raw thumbs (y-up)     |
//! | left/rightTrigger.value | LeftTrigger2/RightTrigger2    | triggers 0..1 + flags |
//!
//! gilrs normalises stick Y so up is `+1`, matching GameController, so the values pass
//! through untouched; `GamepadHandler::read_frame` performs the protocol flip.
//!
//! The D-pad is read from the hat switch (`Axis::DPadX/DPadY`) whenever the controller has
//! one, and only from the `DPad*` buttons otherwise. gilrs's default `axis_dpad_to_button`
//! filter is disabled because it names the hat buttons with the generic native codes
//! (HID button usages 16..19), which collide with real buttons on some pads: the Google
//! Stadia controller reports its trigger "click" at usages 19/20, so with the filter every
//! right-trigger pull also pressed D-pad Right (radio station changes in racing games).

use xcloud_proto::input::packet::VibrationReport;

use crate::source::{NativeGamepadSample, Sampler};

/// Errors of the native sampler. The only fallible operation is rumble.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// gilrs could not be initialised; the sampler then reports "no controller".
    #[error("gilrs unavailable: {0}")]
    Unavailable(String),
    /// A force-feedback call failed on a controller that claims to support it.
    #[error("force feedback: {0}")]
    ForceFeedback(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Vendor name used when the controller does not report one (`@"Game controller"`).
pub const FALLBACK_CONTROLLER_NAME: &str = "Game controller";

/// `sampleNativeGamepad()` on a platform without a controller backend: never connected.
pub fn sample_stub() -> NativeGamepadSample {
    NativeGamepadSample::default()
}

#[cfg(not(target_os = "ios"))]
pub use gilrs_backend::NativeGamepad;

/// Diagnostic one-liner of [`gilrs_backend::map_gamepad`] for the `probe` example:
/// button bits, trigger values and pressed flags as the protocol will see them.
#[cfg(not(target_os = "ios"))]
pub fn map_gamepad_debug(pad: &gilrs::Gamepad<'_>) -> Option<String> {
    let s = gilrs_backend::map_gamepad(pad);
    Some(format!(
        "buttons={:#06x} LT={:.3}{} RT={:.3}{} L=({:.2},{:.2}) R=({:.2},{:.2})",
        s.buttons.bits(),
        s.left_trigger,
        if s.left_trigger_pressed { "*" } else { "" },
        s.right_trigger,
        if s.right_trigger_pressed { "*" } else { "" },
        s.left_thumb.x,
        s.left_thumb.y,
        s.right_thumb.x,
        s.right_thumb.y,
    ))
}

#[cfg(not(target_os = "ios"))]
mod gilrs_backend {
    use gilrs::ff::{BaseEffect, BaseEffectType, Effect, EffectBuilder, Repeat, Replay, Ticks};
    use gilrs::{Axis, Button, EventType, Gamepad, GamepadId, Gilrs, GilrsBuilder};
    use tracing::{info, warn};
    use xcloud_proto::input::gamepad::RawGamepadState;
    use xcloud_proto::input::models::{GamepadButtons, Vec2};
    use xcloud_proto::input::packet::VibrationReport;

    use super::{Error, FALLBACK_CONTROLLER_NAME, Result};
    use crate::source::{NativeGamepadSample, Sampler};

    /// A `GCControllerButtonInput.isPressed` style read: gilrs reports analog triggers
    /// as buttons with a value, so `pressed` is its own threshold.
    fn trigger(pad: &Gamepad<'_>, button: Button) -> (f32, bool) {
        match pad.button_data(button) {
            Some(data) => (data.value().clamp(0.0, 1.0), data.is_pressed()),
            None => (0.0, false),
        }
    }

    /// Whether the controller exposes its D-pad as a hat switch (macOS IOKit, evdev).
    fn has_hat(pad: &Gamepad<'_>) -> bool {
        pad.axis_code(Axis::DPadX).is_some() || pad.axis_code(Axis::DPadY).is_some()
    }

    /// Hat axes to D-pad bits. gilrs normalises the hat like a stick: `x` right is `+1`,
    /// `y` up is `+1` (the sign its own `axis_dpad_to_button` filter uses).
    pub fn dpad_from_hat(x: f32, y: f32) -> GamepadButtons {
        let mut bits = GamepadButtons::default();
        if x > 0.5 {
            bits.insert(GamepadButtons::DPAD_RIGHT);
        } else if x < -0.5 {
            bits.insert(GamepadButtons::DPAD_LEFT);
        }
        if y > 0.5 {
            bits.insert(GamepadButtons::DPAD_UP);
        } else if y < -0.5 {
            bits.insert(GamepadButtons::DPAD_DOWN);
        }
        bits
    }

    /// `NativeGamepad.mm` body: one `GCExtendedGamepad` to raw state.
    pub fn map_gamepad(pad: &Gamepad<'_>) -> RawGamepadState {
        const BUTTONS: [(Button, GamepadButtons); 11] = [
            (Button::South, GamepadButtons::A),
            (Button::East, GamepadButtons::B),
            (Button::West, GamepadButtons::X),
            (Button::North, GamepadButtons::Y),
            (Button::Start, GamepadButtons::MENU),
            (Button::Select, GamepadButtons::VIEW),
            (Button::Mode, GamepadButtons::NEXUS),
            (Button::LeftTrigger, GamepadButtons::LEFT_SHOULDER),
            (Button::RightTrigger, GamepadButtons::RIGHT_SHOULDER),
            (Button::LeftThumb, GamepadButtons::LEFT_THUMB),
            (Button::RightThumb, GamepadButtons::RIGHT_THUMB),
        ];
        const DPAD_BUTTONS: [(Button, GamepadButtons); 4] = [
            (Button::DPadUp, GamepadButtons::DPAD_UP),
            (Button::DPadDown, GamepadButtons::DPAD_DOWN),
            (Button::DPadLeft, GamepadButtons::DPAD_LEFT),
            (Button::DPadRight, GamepadButtons::DPAD_RIGHT),
        ];
        let mut state = RawGamepadState::default();
        for (button, bit) in BUTTONS {
            if pad.is_pressed(button) {
                state.buttons.insert(bit);
            }
        }
        if has_hat(pad) {
            state.buttons.insert(dpad_from_hat(
                pad.value(Axis::DPadX),
                pad.value(Axis::DPadY),
            ));
        } else {
            for (button, bit) in DPAD_BUTTONS {
                if pad.is_pressed(button) {
                    state.buttons.insert(bit);
                }
            }
        }
        state.left_thumb = Vec2::new(
            pad.value(Axis::LeftStickX).clamp(-1.0, 1.0),
            pad.value(Axis::LeftStickY).clamp(-1.0, 1.0),
        );
        state.right_thumb = Vec2::new(
            pad.value(Axis::RightStickX).clamp(-1.0, 1.0),
            pad.value(Axis::RightStickY).clamp(-1.0, 1.0),
        );
        let (left, left_pressed) = trigger(pad, Button::LeftTrigger2);
        let (right, right_pressed) = trigger(pad, Button::RightTrigger2);
        state.left_trigger = left;
        state.right_trigger = right;
        state.left_trigger_pressed = left_pressed;
        state.right_trigger_pressed = right_pressed;
        state
    }

    /// `controller.vendorName ?: @"Game controller"`.
    fn controller_name(pad: &Gamepad<'_>) -> String {
        let name = pad.name().trim();
        if name.is_empty() {
            FALLBACK_CONTROLLER_NAME.to_string()
        } else {
            name.to_string()
        }
    }

    /// The gilrs context plus the controller currently mapped to the stream.
    ///
    /// `GCController.controllers` picked the first extended gamepad on every poll; this
    /// port sticks to the first controller it saw until it disconnects, then takes the
    /// next connected one, so a second controller joining does not steal the stream.
    pub struct NativeGamepad {
        gilrs: Option<Gilrs>,
        active: Option<GamepadId>,
        rumble: Option<Effect>,
    }

    impl std::fmt::Debug for NativeGamepad {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("NativeGamepad")
                .field("available", &self.gilrs.is_some())
                .field("active", &self.active)
                .field("rumbling", &self.rumble.is_some())
                .finish()
        }
    }

    impl Default for NativeGamepad {
        fn default() -> Self {
            Self::new()
        }
    }

    impl NativeGamepad {
        /// Initialises gilrs once. When the platform has no backend the dummy context
        /// gilrs hands back is used (never connected); any other failure is logged and the
        /// sampler reports "no controller" forever.
        pub fn new() -> Self {
            // No default filters: the hat->button filter is what collides with real
            // buttons (module doc); the deadzone is applied by `GamepadHandler` anyway.
            let gilrs = match GilrsBuilder::new().with_default_filters(false).build() {
                Ok(gilrs) => Some(gilrs),
                Err(gilrs::Error::NotImplemented(dummy)) => {
                    info!(target: "Controller", "gilrs has no backend on this platform; keyboard only");
                    Some(dummy)
                }
                Err(error) => {
                    warn!(target: "Controller", "gilrs init failed: {error}; keyboard only");
                    None
                }
            };
            Self {
                gilrs,
                active: None,
                rumble: None,
            }
        }

        /// Whether gilrs initialised; `false` means every sample is "disconnected".
        pub fn is_available(&self) -> bool {
            self.gilrs.is_some()
        }

        /// Drains the gilrs event queue so cached state and connection flags are fresh.
        fn pump_events(&mut self) {
            let Some(gilrs) = self.gilrs.as_mut() else {
                return;
            };
            while let Some(event) = gilrs.next_event() {
                match event.event {
                    EventType::Connected => {
                        info!(target: "Controller", "gilrs: {} connected", gilrs.gamepad(event.id).name());
                    }
                    EventType::Disconnected => {
                        info!(target: "Controller", "gilrs: gamepad {:?} disconnected", event.id);
                        if self.active == Some(event.id) {
                            self.active = None;
                            self.rumble = None;
                        }
                    }
                    _ => {}
                }
            }
        }

        /// `sampleNativeGamepad()`: the current state of the mapped controller.
        pub fn sample(&mut self) -> NativeGamepadSample {
            self.pump_events();
            let Some(gilrs) = self.gilrs.as_ref() else {
                return NativeGamepadSample::default();
            };
            let active = self
                .active
                .and_then(|id| gilrs.connected_gamepad(id).map(|_| id))
                .or_else(|| gilrs.gamepads().next().map(|(id, _)| id));
            if active != self.active {
                self.rumble = None;
                self.active = active;
            }
            let Some(pad) = active.and_then(|id| gilrs.connected_gamepad(id)) else {
                return NativeGamepadSample::default();
            };
            NativeGamepadSample {
                connected: true,
                name: controller_name(&pad),
                state: map_gamepad(&pad),
            }
        }

        /// Plays a server vibration report on the mapped controller.
        ///
        /// Returns `Ok(false)` when there is no gilrs context, no controller, or the
        /// controller has no force feedback (the Qt app never rumbled at all), `Ok(true)`
        /// when an effect was started and `Err` when gilrs refused one it should play.
        /// `intensity` is the user's `vibration_intensity` scale (`0.0..=1.0`); the trigger
        /// motors have no gilrs equivalent and are folded into the body motors.
        pub fn set_vibration(&mut self, report: &VibrationReport, intensity: f32) -> Result<bool> {
            let Some(gilrs) = self.gilrs.as_mut() else {
                return Ok(false);
            };
            let Some(id) = self.active else {
                return Ok(false);
            };
            let Some(pad) = gilrs.connected_gamepad(id) else {
                return Ok(false);
            };
            if !pad.is_ff_supported() {
                return Ok(false);
            }
            // Stop whatever is still playing: a new report replaces the previous one.
            self.rumble = None;

            let scale = intensity.clamp(0.0, 1.0);
            let strong = report
                .left_motor_percent
                .max(report.left_trigger_motor_percent);
            let weak = report
                .right_motor_percent
                .max(report.right_trigger_motor_percent);
            let magnitude =
                |percent: f32| (percent.clamp(0.0, 1.0) * scale * f32::from(u16::MAX)) as u16;
            if (magnitude(strong) == 0 && magnitude(weak) == 0) || report.duration_ms == 0 {
                return Ok(true);
            }

            let scheduling = Replay {
                after: Ticks::from_ms(u32::from(report.delay_ms)),
                play_for: Ticks::from_ms(u32::from(report.duration_ms)),
                with_delay: Ticks::from_ms(u32::from(report.delay_ms)),
            };
            let cycles = u32::from(report.repeat_count) + 1;
            let total_ms = cycles * (u32::from(report.duration_ms) + u32::from(report.delay_ms));
            let effect = EffectBuilder::new()
                .add_effect(BaseEffect {
                    kind: BaseEffectType::Strong {
                        magnitude: magnitude(strong),
                    },
                    scheduling,
                    ..BaseEffect::default()
                })
                .add_effect(BaseEffect {
                    kind: BaseEffectType::Weak {
                        magnitude: magnitude(weak),
                    },
                    scheduling,
                    ..BaseEffect::default()
                })
                .gamepads(&[id])
                .repeat(Repeat::For(Ticks::from_ms(total_ms)))
                .finish(gilrs)
                .map_err(|error| Error::ForceFeedback(error.to_string()))?;
            effect
                .play()
                .map_err(|error| Error::ForceFeedback(error.to_string()))?;
            self.rumble = Some(effect);
            Ok(true)
        }

        /// Stops any running rumble.
        pub fn stop_vibration(&mut self) {
            self.rumble = None;
        }

        /// Wraps the sampler for `GamepadInputSource::new`.
        pub fn into_sampler(self) -> Sampler {
            let mut native = self;
            Box::new(move || native.sample())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn hat_axes_map_to_dpad_bits() {
            assert_eq!(dpad_from_hat(0.0, 0.0), GamepadButtons::default());
            assert_eq!(dpad_from_hat(1.0, 0.0), GamepadButtons::DPAD_RIGHT);
            assert_eq!(dpad_from_hat(-1.0, 0.0), GamepadButtons::DPAD_LEFT);
            assert_eq!(dpad_from_hat(0.0, 1.0), GamepadButtons::DPAD_UP);
            assert_eq!(dpad_from_hat(0.0, -1.0), GamepadButtons::DPAD_DOWN);
            assert_eq!(
                dpad_from_hat(1.0, -1.0),
                GamepadButtons::DPAD_RIGHT | GamepadButtons::DPAD_DOWN
            );
            // Centre jitter never registers.
            assert_eq!(dpad_from_hat(0.3, -0.4), GamepadButtons::default());
        }

        #[test]
        fn native_gamepad_is_send() {
            fn assert_send<T: Send>() {}
            assert_send::<NativeGamepad>();
        }

        #[test]
        fn samples_without_hardware_report_disconnected() {
            // Headless CI has no controller; gilrs may or may not initialise, but the
            // sampler must never claim a connection it does not have.
            let mut native = NativeGamepad::new();
            let sample = native.sample();
            if !native.is_available() || sample.name.is_empty() {
                assert_eq!(sample, NativeGamepadSample::default());
            }
            let mut sampler = native.into_sampler();
            let again = sampler();
            assert_eq!(again.connected, sample.connected);
        }

        #[test]
        fn vibration_without_controller_is_unsupported() {
            let mut native = NativeGamepad::new();
            native.active = None;
            let report = VibrationReport {
                left_motor_percent: 1.0,
                duration_ms: 100,
                ..VibrationReport::default()
            };
            assert!(matches!(native.set_vibration(&report, 1.0), Ok(false)));
            native.stop_vibration();
        }
    }
}

/// The platform sampler for [`crate::source::GamepadInputSource::new`]: gilrs where it
/// builds, the "no controller" stub on iOS.
pub fn native_sampler() -> Sampler {
    #[cfg(not(target_os = "ios"))]
    {
        NativeGamepad::new().into_sampler()
    }
    #[cfg(target_os = "ios")]
    {
        Box::new(sample_stub)
    }
}

/// Convenience for hosts that only hold a [`Sampler`]: a report is "unsupported" unless
/// they keep a [`NativeGamepad`] themselves and call `set_vibration` on it.
pub fn vibration_unsupported(_report: &VibrationReport) -> Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use xcloud_proto::input::gamepad::RawGamepadState;

    #[test]
    fn stub_is_never_connected() {
        let sample = sample_stub();
        assert!(!sample.connected);
        assert!(sample.name.is_empty());
        assert_eq!(sample.state, RawGamepadState::default());
    }

    #[test]
    fn vibration_fallback_reports_unsupported() {
        assert!(matches!(
            vibration_unsupported(&VibrationReport::default()),
            Ok(false)
        ));
    }

    #[test]
    fn error_messages_name_the_source() {
        assert_eq!(
            Error::Unavailable("x".into()).to_string(),
            "gilrs unavailable: x"
        );
        assert_eq!(
            Error::ForceFeedback("y".into()).to_string(),
            "force feedback: y"
        );
    }
}
