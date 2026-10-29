//! Keeps macOS from acting on the controller's system buttons while a game streams.
//!
//! By default the Home (Xbox/Guide) button opens the Games app / Apple Arcade and the
//! Share button takes a screenshot, even while another app is in front. The
//! GameController framework lets the frontmost app opt out per button with
//! `GCControllerElement.preferredSystemGestureState = .disabled`; Apple names game
//! streaming apps as the intended use. Input still reaches the app: gilrs reads the
//! raw HID reports, so Home keeps arriving as the Xbox Guide button.
//!
//! The setting is per controller element and only respected while the app is
//! frontmost, so [`SystemGestureGuard::update`] re-applies it periodically to catch
//! controllers that connect mid-stream. Call it from the main (UI) thread.

use std::time::{Duration, Instant};

/// How often the claim is re-applied to pick up newly connected controllers.
pub const REAPPLY_INTERVAL: Duration = Duration::from_secs(1);

/// Claims the system buttons while `captured` and hands them back otherwise.
#[derive(Debug, Default)]
pub struct SystemGestureGuard {
    captured: bool,
    last_apply: Option<Instant>,
    /// Elements changed by the last application (for logs and tests).
    last_count: usize,
}

impl SystemGestureGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the buttons are currently claimed.
    pub fn captured(&self) -> bool {
        self.captured
    }

    /// Number of controller buttons touched by the last application.
    pub fn last_count(&self) -> usize {
        self.last_count
    }

    /// Claims (`want == true`) or releases the system buttons. Cheap to call every
    /// frame: it only talks to GameController on a change or every
    /// [`REAPPLY_INTERVAL`] while claimed.
    pub fn update(&mut self, want: bool, now: Instant) {
        let due = match self.last_apply {
            None => true,
            Some(at) => now.duration_since(at) >= REAPPLY_INTERVAL,
        };
        if want == self.captured && !(want && due) {
            return;
        }
        let changed = want != self.captured;
        self.last_count = apply(want);
        self.captured = want;
        self.last_apply = Some(now);
        if changed {
            tracing::info!(
                target: "Controller",
                "{} system buttons ({} controller buttons)",
                if want { "claimed" } else { "released" },
                self.last_count
            );
        }
    }
}

impl Drop for SystemGestureGuard {
    fn drop(&mut self) {
        if self.captured {
            apply(false);
        }
    }
}

#[cfg(target_os = "macos")]
fn apply(capture: bool) -> usize {
    use objc2_game_controller::{GCController, GCSystemGestureState};

    let state = if capture {
        GCSystemGestureState::Disabled
    } else {
        GCSystemGestureState::Enabled
    };
    let mut count = 0;
    // SAFETY: GameController class/instance methods without arguments beyond the
    // enum; the returned collections are retained snapshots owned by us.
    unsafe {
        for controller in GCController::controllers().iter() {
            let profile = controller.physicalInputProfile();
            for button in profile.allButtons().allObjects().iter() {
                // Only the buttons the system binds (Home, Share, sometimes Menu).
                if button.isBoundToSystemGesture()
                    || button.preferredSystemGestureState() != GCSystemGestureState::Enabled
                {
                    button.setPreferredSystemGestureState(state);
                    count += 1;
                }
            }
            if let Some(gamepad) = controller.extendedGamepad()
                && let Some(home) = gamepad.buttonHome()
            {
                home.setPreferredSystemGestureState(state);
                count += 1;
            }
        }
    }
    count
}

#[cfg(not(target_os = "macos"))]
fn apply(_capture: bool) -> usize {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_on_change_and_reapplies_while_claimed() {
        let mut guard = SystemGestureGuard::new();
        let t0 = Instant::now();
        guard.update(false, t0);
        assert!(!guard.captured());
        guard.update(true, t0);
        assert!(guard.captured());
        let applied_at = guard.last_apply;
        // Within the interval nothing is re-applied.
        guard.update(true, t0 + Duration::from_millis(200));
        assert_eq!(guard.last_apply, applied_at);
        // After it, the claim is refreshed for late controllers.
        guard.update(true, t0 + REAPPLY_INTERVAL + Duration::from_millis(1));
        assert_ne!(guard.last_apply, applied_at);
        guard.update(false, t0 + Duration::from_secs(5));
        assert!(!guard.captured());
    }
}
