//! Port of qt/src/app/NativeGamepad.{h,mm,cpp}: the physical-controller sampler.
//!
//! The Qt app read Apple's GameController framework (`NativeGamepad.mm`); every other
//! platform compiled the stub returning "no controller" (`NativeGamepad.cpp`). This port
//! reads controllers through one [`Backend`] per platform: GameController on macOS,
//! gilrs on Windows and Linux; iOS keeps the stub. Each backend maps to [`RawGamepadState`] the way `NativeGamepad.mm` did:
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
//! [`NativeGamepad`] is the platform's backend as the one controller mapped to the
//! stream, and [`SharedGamepad`] is the handle the stream and the UI share.

use xcloud_proto::input::packet::VibrationReport;

use crate::source::{NativeGamepadSample, Sampler};

#[cfg(target_os = "macos")]
mod game_controller;
#[cfg(target_os = "macos")]
mod gc_haptics;
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
mod gilrs_backend;
#[cfg(any(test, target_os = "macos"))]
mod rumble;

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

/// One way of reading physical controllers (a library or an OS framework).
///
/// A backend reports the single controller it would map to the stream, or
/// "disconnected" when it has none.
pub trait Backend: Send {
    /// Short name for logs.
    fn name(&self) -> &'static str;

    /// The current state of the backend's controller.
    fn sample(&mut self) -> NativeGamepadSample;

    /// Plays a server vibration report on the backend's controller.
    ///
    /// `Ok(false)` when it has no controller or the controller cannot rumble, `Ok(true)`
    /// when the report was started, `Err` when a controller that should rumble refused.
    /// `intensity` is the user's `vibration_intensity` scale (`0.0..=1.0`).
    fn set_vibration(&mut self, report: &VibrationReport, intensity: f32) -> Result<bool>;

    /// Stops any running rumble.
    fn stop_vibration(&mut self);
}

/// The physical controller mapped to the stream, read through the platform's backend.
pub struct NativeGamepad {
    /// `None` on a platform without one (iOS): never connected.
    backend: Option<Box<dyn Backend>>,
}

impl std::fmt::Debug for NativeGamepad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeGamepad")
            .field("backend", &self.backend.as_ref().map(|b| b.name()))
            .finish()
    }
}

impl Default for NativeGamepad {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeGamepad {
    /// The platform's backend: GameController on macOS, gilrs on Windows and Linux,
    /// none on iOS.
    pub fn new() -> Self {
        #[cfg(target_os = "macos")]
        let backend: Option<Box<dyn Backend>> =
            Some(Box::new(game_controller::GameControllerBackend::new()));
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        let backend: Option<Box<dyn Backend>> = Some(Box::new(gilrs_backend::GilrsBackend::new()));
        #[cfg(target_os = "ios")]
        let backend: Option<Box<dyn Backend>> = None;
        Self { backend }
    }

    /// A sampler over the given backend.
    pub fn with_backend(backend: Box<dyn Backend>) -> Self {
        Self {
            backend: Some(backend),
        }
    }

    /// `sampleNativeGamepad()`: the current state of the mapped controller.
    pub fn sample(&mut self) -> NativeGamepadSample {
        self.backend
            .as_mut()
            .map(|backend| backend.sample())
            .unwrap_or_default()
    }

    /// Plays a server vibration report on the mapped controller; `Ok(false)` without
    /// one. See [`Backend::set_vibration`].
    pub fn set_vibration(&mut self, report: &VibrationReport, intensity: f32) -> Result<bool> {
        match &mut self.backend {
            Some(backend) => backend.set_vibration(report, intensity),
            None => Ok(false),
        }
    }

    /// Stops any running rumble.
    pub fn stop_vibration(&mut self) {
        if let Some(backend) = &mut self.backend {
            backend.stop_vibration();
        }
    }

    /// Wraps the sampler for `GamepadInputSource::new`.
    pub fn into_sampler(self) -> Sampler {
        let mut native = self;
        Box::new(move || native.sample())
    }
}

/// The process's one physical controller, shared by everything that reads it.
///
/// The stream's input thread samples it while a game runs and the UI poller while none
/// does; both hold a clone of this handle instead of opening the controller themselves,
/// so there is a single backend, a single "active controller" choice and a single
/// rumble effect. The controller is opened on first use.
#[derive(Clone, Default)]
pub struct SharedGamepad {
    pad: std::sync::Arc<parking_lot::Mutex<Option<NativeGamepad>>>,
}

impl SharedGamepad {
    pub fn new() -> Self {
        Self::default()
    }

    fn with<R>(&self, f: impl FnOnce(&mut NativeGamepad) -> R) -> R {
        f(self.pad.lock().get_or_insert_with(NativeGamepad::new))
    }

    /// The current state of the mapped controller.
    pub fn sample(&self) -> NativeGamepadSample {
        self.with(NativeGamepad::sample)
    }

    /// Plays a server vibration report; see [`NativeGamepad::set_vibration`].
    pub fn set_vibration(&self, report: &VibrationReport, intensity: f32) -> Result<bool> {
        self.with(|pad| pad.set_vibration(report, intensity))
    }

    /// Stops any running rumble.
    pub fn stop_vibration(&self) {
        self.with(NativeGamepad::stop_vibration);
    }

    /// A sampler for `GamepadInputSource::new` reading this controller.
    pub fn sampler(&self) -> Sampler {
        let pad = self.clone();
        Box::new(move || pad.sample())
    }
}

impl std::fmt::Debug for SharedGamepad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedGamepad")
            .field("opened", &self.pad.lock().is_some())
            .finish()
    }
}

/// Diagnostic one-liner of the gilrs mapping for the `probe` example: button bits,
/// trigger values and pressed flags as the protocol will see them.
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
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

/// The platform sampler for [`crate::source::GamepadInputSource::new`].
pub fn native_sampler() -> Sampler {
    NativeGamepad::new().into_sampler()
}

/// Convenience for hosts that only hold a [`Sampler`]: a report is "unsupported" unless
/// they keep a [`NativeGamepad`] themselves and call `set_vibration` on it.
pub fn vibration_unsupported(_report: &VibrationReport) -> Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use parking_lot::Mutex;
    use xcloud_proto::input::gamepad::RawGamepadState;

    use super::*;

    /// A scripted backend: connected or not, counting the rumble calls it receives.
    #[derive(Default)]
    struct Script {
        connected: bool,
        rumbles: usize,
        stops: usize,
    }

    struct Fake(&'static str, Arc<Mutex<Script>>);

    impl Backend for Fake {
        fn name(&self) -> &'static str {
            self.0
        }

        fn sample(&mut self) -> NativeGamepadSample {
            if self.1.lock().connected {
                NativeGamepadSample {
                    connected: true,
                    name: self.0.to_owned(),
                    state: RawGamepadState::default(),
                }
            } else {
                NativeGamepadSample::default()
            }
        }

        fn set_vibration(&mut self, _report: &VibrationReport, _intensity: f32) -> Result<bool> {
            self.1.lock().rumbles += 1;
            Ok(true)
        }

        fn stop_vibration(&mut self) {
            self.1.lock().stops += 1;
        }
    }

    fn scripted() -> (NativeGamepad, Arc<Mutex<Script>>) {
        let script = Arc::default();
        let pad = NativeGamepad::with_backend(Box::new(Fake("fake", Arc::clone(&script))));
        (pad, script)
    }

    #[test]
    fn stub_is_never_connected() {
        let sample = sample_stub();
        assert!(!sample.connected);
        assert!(sample.name.is_empty());
        assert_eq!(sample.state, RawGamepadState::default());
    }

    #[test]
    fn samples_and_rumble_go_through_the_backend() {
        let (mut pad, script) = scripted();
        assert!(!pad.sample().connected, "nothing connected");
        script.lock().connected = true;
        assert_eq!(pad.sample().name, "fake");

        let report = VibrationReport::default();
        assert!(matches!(pad.set_vibration(&report, 1.0), Ok(true)));
        pad.stop_vibration();
        let script = script.lock();
        assert_eq!((script.rumbles, script.stops), (1, 1));
    }

    #[test]
    fn without_a_backend_there_is_no_controller() {
        let mut pad = NativeGamepad { backend: None };
        assert_eq!(pad.sample(), NativeGamepadSample::default());
        assert!(matches!(
            pad.set_vibration(&VibrationReport::default(), 1.0),
            Ok(false)
        ));
        pad.stop_vibration();
    }

    #[test]
    fn the_platform_sampler_never_invents_a_controller() {
        fn assert_send<T: Send>() {}
        assert_send::<NativeGamepad>();
        // Headless CI has no controller; whatever the backends do, a sample without a
        // name is the default "disconnected" one.
        let mut native = NativeGamepad::new();
        let sample = native.sample();
        if sample.name.is_empty() {
            assert_eq!(sample, NativeGamepadSample::default());
        }
        let mut sampler = native.into_sampler();
        assert_eq!(sampler().connected, sample.connected);
    }

    #[test]
    fn shared_gamepad_clones_read_one_controller() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SharedGamepad>();

        let pad = SharedGamepad::new();
        let other = pad.clone();
        assert!(
            format!("{pad:?}").contains("opened: false"),
            "opened lazily"
        );
        let sample = other.sample();
        assert!(
            format!("{pad:?}").contains("opened: true"),
            "by either clone"
        );
        // Both handles and the stream's sampler see the same controller.
        let mut sampler = pad.sampler();
        assert_eq!(sampler().connected, sample.connected);
        assert_eq!(pad.sample().name, sample.name);
        pad.stop_vibration();
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
