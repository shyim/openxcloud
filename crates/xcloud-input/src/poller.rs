//! Background sampling of the controller for a UI (outside a stream).
//!
//! [`GamepadPoller`] samples the process's [`SharedGamepad`] on its own thread, keeps
//! the latest sample and wakes the UI only when there is something to react to, so an
//! idle window is not redrawn at the polling rate. While paused it leaves the controller
//! to the stream's input thread, which reads the same [`SharedGamepad`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use xcloud_proto::input::gamepad::RawGamepadState;

use crate::native::SharedGamepad;
use crate::source::NativeGamepadSample;

/// Sample period while the UI owns the controller.
const POLL_INTERVAL: Duration = Duration::from_millis(16);
/// Wake-up period while paused (a stream owns the controller).
const PAUSED_INTERVAL: Duration = Duration::from_millis(100);

/// Whether a controller state holds input the UI reacts to (a pressed navigation
/// button, a deflected stick). Analog noise below that must not wake the UI.
pub type Engaged = fn(&RawGamepadState) -> bool;

#[derive(Default)]
struct Shared {
    sample: Mutex<NativeGamepadSample>,
    paused: AtomicBool,
    /// Wake on every change, not only on engaged input (a live controller view).
    live: AtomicBool,
    stop: AtomicBool,
}

/// Samples the controller on a background thread; see the module documentation.
pub struct GamepadPoller {
    shared: Arc<Shared>,
}

impl GamepadPoller {
    /// Starts the thread reading `pad`. `wake` asks the UI for a frame; `engaged` tells
    /// input the UI reacts to from noise.
    pub fn spawn(
        pad: SharedGamepad,
        wake: impl Fn() + Send + 'static,
        engaged: Engaged,
    ) -> std::io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let thread_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("ui-gamepad".into())
            .spawn(move || run(&thread_shared, &pad, &wake, engaged))?;
        Ok(Self { shared })
    }

    /// The latest sample ("not connected" while paused).
    pub fn sample(&self) -> NativeGamepadSample {
        self.shared.sample.lock().clone()
    }

    /// Pauses sampling while a stream reads the controller.
    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    /// Wake on every change of the controller state (a live controller view).
    pub fn set_live(&self, live: bool) {
        self.shared.live.store(live, Ordering::Relaxed);
    }
}

impl Drop for GamepadPoller {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

/// What a new sample means for the UI, given the one it replaces.
fn should_wake(
    previous: &NativeGamepadSample,
    sample: &NativeGamepadSample,
    engaged: Engaged,
    live: bool,
) -> bool {
    if previous == sample {
        return false;
    }
    let now_engaged = engaged(&sample.state);
    // Connection, buttons and crossing the engagement threshold always matter; other
    // changes (stick travel, trigger pull) only while engaged or watched live.
    previous.connected != sample.connected
        || previous.state.buttons != sample.state.buttons
        || engaged(&previous.state) != now_engaged
        || now_engaged
        || live
}

fn run(shared: &Shared, pad: &SharedGamepad, wake: &dyn Fn(), engaged: Engaged) {
    let mut sampling = false;
    while !shared.stop.load(Ordering::Relaxed) {
        if shared.paused.load(Ordering::Relaxed) {
            if std::mem::take(&mut sampling) {
                *shared.sample.lock() = NativeGamepadSample::default();
            }
            std::thread::sleep(PAUSED_INTERVAL);
            continue;
        }
        sampling = true;
        let sample = pad.sample();
        let live = shared.live.load(Ordering::Relaxed);
        let woke = {
            let mut current = shared.sample.lock();
            let woke = should_wake(&current, &sample, engaged, live);
            *current = sample;
            woke
        };
        if woke {
            wake();
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xcloud_proto::input::models::{GamepadButtons, Vec2};

    fn sample(buttons: GamepadButtons, stick_x: f32) -> NativeGamepadSample {
        NativeGamepadSample {
            connected: true,
            name: "Test Pad".into(),
            state: RawGamepadState {
                buttons,
                left_thumb: Vec2::new(stick_x, 0.0),
                ..RawGamepadState::default()
            },
        }
    }

    fn stick_engaged(state: &RawGamepadState) -> bool {
        state.left_thumb.x.abs() > 0.5
    }

    #[test]
    fn only_input_the_ui_reacts_to_wakes_it() {
        let rest = sample(GamepadButtons::NONE, 0.0);
        let noise = sample(GamepadButtons::NONE, 0.05);
        let pushed = sample(GamepadButtons::NONE, 0.9);
        let pressed = sample(GamepadButtons::A, 0.0);

        assert!(!should_wake(&rest, &rest, stick_engaged, true), "unchanged");
        assert!(
            !should_wake(&rest, &noise, stick_engaged, false),
            "stick noise"
        );
        assert!(should_wake(&rest, &noise, stick_engaged, true), "live view");
        assert!(should_wake(&rest, &pushed, stick_engaged, false), "engaged");
        assert!(
            should_wake(&pushed, &rest, stick_engaged, false),
            "released"
        );
        assert!(should_wake(&rest, &pressed, stick_engaged, false), "button");
        assert!(
            should_wake(&NativeGamepadSample::default(), &rest, stick_engaged, false),
            "connected"
        );
    }
}
