//! The GameController backend (macOS): every controller Apple's framework supports
//! (Xbox, PlayStation, Switch, MFi and the generic HID pads it knows, such as the Stadia
//! controller), as the Qt app's `NativeGamepad.mm` read them.
//!
//! It is the only backend on macOS. Reading raw HID (as gilrs does) is not an option
//! there: over Bluetooth a Sony pad only sends the simple HID report (`0x01`) until
//! something requests its calibration feature report; macOS's `gamecontrollerd` does so
//! on connect, after which the pad sends the extended report (`0x31` / `0x11`) that the
//! descriptor declares as vendor-defined data, and a HID reader never sees a button or
//! axis change. The framework also is the only way to rumble a controller on macOS.
//!
//! The framework delivers values on the main queue, so this only produces input in a
//! process that runs the main run loop (the app, or the examples that spin it). Rumble goes
//! through the framework's haptics (`gc_haptics`).

use std::sync::Once;
use std::time::Instant;

use objc2::rc::{Retained, autoreleasepool};
use objc2_game_controller::{
    GCController, GCControllerButtonInput, GCControllerDirectionPad, GCDevice, GCExtendedGamepad,
};
use tracing::warn;
use xcloud_proto::input::gamepad::RawGamepadState;
use xcloud_proto::input::models::{GamepadButtons, Vec2};
use xcloud_proto::input::packet::VibrationReport;

use super::gc_haptics::Motors;
use super::rumble::{MotorLevels, levels_at};
use super::{Backend, Error, FALLBACK_CONTROLLER_NAME, Result};
use crate::source::NativeGamepadSample;

/// A vibration report being played: the framework takes motor levels, so the report's
/// delay / duration / repeat timeline is stepped from [`Backend::sample`].
struct Playing {
    report: VibrationReport,
    intensity: f32,
    started: Instant,
}

/// Samples the first extended gamepad GameController lists and rumbles it.
#[derive(Default)]
pub struct GameControllerBackend {
    /// The mapped controller's motors, opened on the first vibration report.
    motors: Option<Motors>,
    playing: Option<Playing>,
}

// SAFETY: the Objective-C objects in `motors` are only touched through `&mut self`, and
// the one instance lives behind `SharedGamepad`'s mutex, so access is serialised.
// Neither GameController's haptics nor CoreHaptics is tied to the main thread.
unsafe impl Send for GameControllerBackend {}

/// The controller mapped to the stream: the first extended gamepad listed.
fn mapped_controller() -> Option<Retained<GCController>> {
    static BACKGROUND: Once = Once::new();
    // SAFETY: argument-free GameController calls returning retained snapshots.
    unsafe {
        // Keep reading while another window is in front, like the other platforms'
        // backend; the callers decide what an unfocused window ignores.
        BACKGROUND.call_once(|| GCController::setShouldMonitorBackgroundEvents(true));
        GCController::controllers()
            .iter()
            .find(|controller| controller.extendedGamepad().is_some())
    }
}

impl GameControllerBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the motors, giving up on them when the framework refuses (the next report
    /// reopens them).
    fn apply(&mut self, levels: MotorLevels) {
        if let Some(motors) = &mut self.motors
            && let Err(error) = motors.apply(levels)
        {
            warn!(target: "Controller", "rumble stopped: {error}");
            self.motors = None;
            self.playing = None;
        }
    }

    /// Advances the report being played to now.
    fn step_rumble(&mut self) {
        let Some(playing) = &self.playing else {
            return;
        };
        let elapsed = playing.started.elapsed().as_millis() as u64;
        match levels_at(&playing.report, playing.intensity, elapsed) {
            Some(levels) => self.apply(levels),
            None => {
                self.playing = None;
                self.apply(MotorLevels::OFF);
            }
        }
    }
}

impl Backend for GameControllerBackend {
    fn name(&self) -> &'static str {
        "GameController"
    }

    fn sample(&mut self) -> NativeGamepadSample {
        autoreleasepool(|_| {
            let controller = mapped_controller();
            // Motors belong to one controller; drop them when it is gone or replaced.
            if self
                .motors
                .as_ref()
                .is_some_and(|motors| !controller.as_deref().is_some_and(|c| motors.belong_to(c)))
            {
                self.motors = None;
                self.playing = None;
            }
            self.step_rumble();
            let Some(controller) = controller else {
                return NativeGamepadSample::default();
            };
            // SAFETY: argument-free getters on a live controller.
            unsafe {
                let Some(pad) = controller.extendedGamepad() else {
                    return NativeGamepadSample::default();
                };
                let name = controller
                    .vendorName()
                    .map(|name| name.to_string())
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or_else(|| FALLBACK_CONTROLLER_NAME.to_owned());
                NativeGamepadSample {
                    connected: true,
                    name,
                    state: map(&pad),
                }
            }
        })
    }

    fn set_vibration(&mut self, report: &VibrationReport, intensity: f32) -> Result<bool> {
        autoreleasepool(|_| {
            let Some(controller) = mapped_controller() else {
                return Ok(false);
            };
            if !self
                .motors
                .as_ref()
                .is_some_and(|m| m.belong_to(&controller))
            {
                self.playing = None;
                self.motors = Motors::open(&controller).map_err(Error::ForceFeedback)?;
            }
            if self.motors.is_none() {
                return Ok(false);
            }
            // A new report replaces the previous one.
            self.playing = Some(Playing {
                report: *report,
                intensity,
                started: Instant::now(),
            });
            self.step_rumble();
            Ok(true)
        })
    }

    fn stop_vibration(&mut self) {
        self.playing = None;
        autoreleasepool(|_| self.apply(MotorLevels::OFF));
    }
}

unsafe fn stick(pad: &GCControllerDirectionPad) -> Vec2 {
    // SAFETY: argument-free getters on a live element.
    unsafe {
        Vec2::new(
            pad.xAxis().value().clamp(-1.0, 1.0),
            pad.yAxis().value().clamp(-1.0, 1.0),
        )
    }
}

/// `NativeGamepad.mm` body: one `GCExtendedGamepad` to raw state (y is up-positive,
/// like the gilrs backend).
unsafe fn map(pad: &GCExtendedGamepad) -> RawGamepadState {
    let mut state = RawGamepadState::default();
    let mut set = |button: &GCControllerButtonInput, bit: GamepadButtons| {
        // SAFETY: argument-free getter on a live element.
        if unsafe { button.isPressed() } {
            state.buttons.insert(bit);
        }
    };
    // SAFETY: argument-free getters on a live profile.
    unsafe {
        set(&pad.buttonA(), GamepadButtons::A);
        set(&pad.buttonB(), GamepadButtons::B);
        set(&pad.buttonX(), GamepadButtons::X);
        set(&pad.buttonY(), GamepadButtons::Y);
        set(&pad.buttonMenu(), GamepadButtons::MENU);
        set(&pad.leftShoulder(), GamepadButtons::LEFT_SHOULDER);
        set(&pad.rightShoulder(), GamepadButtons::RIGHT_SHOULDER);
        if let Some(button) = pad.buttonOptions() {
            set(&button, GamepadButtons::VIEW);
        }
        if let Some(button) = pad.buttonHome() {
            set(&button, GamepadButtons::NEXUS);
        }
        if let Some(button) = pad.leftThumbstickButton() {
            set(&button, GamepadButtons::LEFT_THUMB);
        }
        if let Some(button) = pad.rightThumbstickButton() {
            set(&button, GamepadButtons::RIGHT_THUMB);
        }
        let dpad = pad.dpad();
        set(&dpad.up(), GamepadButtons::DPAD_UP);
        set(&dpad.down(), GamepadButtons::DPAD_DOWN);
        set(&dpad.left(), GamepadButtons::DPAD_LEFT);
        set(&dpad.right(), GamepadButtons::DPAD_RIGHT);

        state.left_thumb = stick(&pad.leftThumbstick());
        state.right_thumb = stick(&pad.rightThumbstick());
        let (left, right) = (pad.leftTrigger(), pad.rightTrigger());
        state.left_trigger = left.value().clamp(0.0, 1.0);
        state.right_trigger = right.value().clamp(0.0, 1.0);
        state.left_trigger_pressed = left.isPressed();
        state.right_trigger_pressed = right.isPressed();
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_listed_controller_the_sample_is_disconnected() {
        // No run loop in a unit test, so the framework lists nothing.
        let mut backend = GameControllerBackend::new();
        let sample = backend.sample();
        if !sample.connected {
            assert_eq!(sample, NativeGamepadSample::default());
        }
        assert!(matches!(
            backend.set_vibration(&VibrationReport::default(), 1.0),
            Ok(false)
        ));
        backend.stop_vibration();
    }
}
