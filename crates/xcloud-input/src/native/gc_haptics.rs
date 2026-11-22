//! Rumble for the GameController backend.
//!
//! The framework has no "set motor level" call: `GCDeviceHaptics` hands out one
//! CoreHaptics engine per motor ("locality"). Each [`Motor`] keeps an engine with one
//! endless continuous pattern and turns it into a level control: the pattern's player is
//! started and stopped, and its intensity changed, as the level moves. This is the
//! scheme SDL uses for the same framework.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{AnyThread, msg_send};
use objc2_core_haptics::{
    CHHapticDynamicParameter, CHHapticDynamicParameterIDHapticIntensityControl, CHHapticEngine,
    CHHapticEvent, CHHapticEventParameter, CHHapticEventParameterIDHapticIntensity,
    CHHapticEventTypeHapticContinuous, CHHapticPattern, CHHapticPatternPlayer,
    CHHapticTimeImmediate,
};
use objc2_foundation::{NSArray, NSError, NSString};
use objc2_game_controller::{
    GCController, GCDeviceHaptics, GCHapticDurationInfinite, GCHapticsLocalityDefault,
    GCHapticsLocalityLeftHandle, GCHapticsLocalityLeftTrigger, GCHapticsLocalityRightHandle,
    GCHapticsLocalityRightTrigger,
};

use super::rumble::MotorLevels;

fn describe(error: Retained<NSError>) -> String {
    error.localizedDescription().to_string()
}

/// One motor of a controller as a level control.
struct Motor {
    /// Kept alive for the player; never used directly after start.
    _engine: Retained<CHHapticEngine>,
    player: Retained<ProtocolObject<dyn CHHapticPatternPlayer>>,
    running: bool,
    level: f32,
}

impl Motor {
    /// Opens the motor at `locality`; `Ok(None)` when the controller has none there.
    ///
    /// # Safety
    /// `haptics` must be a live `GCDeviceHaptics`.
    unsafe fn open(haptics: &GCDeviceHaptics, locality: &NSString) -> Result<Option<Self>, String> {
        // SAFETY: plain Objective-C calls on live objects with the argument types the
        // headers declare. `createEngineWithLocality:` is sent by hand because the
        // bindings only declare it for iOS, though macOS has had it since 11.0.
        unsafe {
            if !haptics.supportedLocalities().containsObject(locality) {
                return Ok(None);
            }
            let engine: Option<Retained<CHHapticEngine>> =
                msg_send![haptics, createEngineWithLocality: locality];
            let Some(engine) = engine else {
                return Ok(None);
            };
            engine.startAndReturnError().map_err(describe)?;
            let full = CHHapticEventParameter::initWithParameterID_value(
                CHHapticEventParameter::alloc(),
                CHHapticEventParameterIDHapticIntensity,
                1.0,
            );
            let event = CHHapticEvent::initWithEventType_parameters_relativeTime_duration(
                CHHapticEvent::alloc(),
                CHHapticEventTypeHapticContinuous,
                &NSArray::from_retained_slice(&[full]),
                0.0,
                f64::from(GCHapticDurationInfinite),
            );
            let pattern = CHHapticPattern::initWithEvents_parameters_error(
                CHHapticPattern::alloc(),
                &NSArray::from_retained_slice(&[event]),
                &NSArray::new(),
            )
            .map_err(describe)?;
            let player = engine
                .createPlayerWithPattern_error(&pattern)
                .map_err(describe)?;
            Ok(Some(Self {
                _engine: engine,
                player,
                running: false,
                level: 0.0,
            }))
        }
    }

    fn set(&mut self, level: f32) -> Result<(), String> {
        let level = level.clamp(0.0, 1.0);
        // SAFETY: calls on the live player with the declared argument types.
        unsafe {
            if level <= 0.0 {
                if self.running {
                    self.running = false;
                    self.level = 0.0;
                    self.player
                        .stopAtTime_error(CHHapticTimeImmediate)
                        .map_err(describe)?;
                }
                return Ok(());
            }
            if self.running && (level - self.level).abs() < 0.005 {
                return Ok(());
            }
            let intensity = CHHapticDynamicParameter::initWithParameterID_value_relativeTime(
                CHHapticDynamicParameter::alloc(),
                CHHapticDynamicParameterIDHapticIntensityControl,
                level,
                0.0,
            );
            self.player
                .sendParameters_atTime_error(
                    &NSArray::from_retained_slice(&[intensity]),
                    CHHapticTimeImmediate,
                )
                .map_err(describe)?;
            if !self.running {
                self.player
                    .startAtTime_error(CHHapticTimeImmediate)
                    .map_err(describe)?;
                self.running = true;
            }
            self.level = level;
        }
        Ok(())
    }
}

/// The motors of one controller.
pub struct Motors {
    controller: Retained<GCController>,
    left: Option<Motor>,
    right: Option<Motor>,
    left_trigger: Option<Motor>,
    right_trigger: Option<Motor>,
    /// The single motor of a controller without separate handles.
    whole: Option<Motor>,
}

impl Motors {
    /// Opens the controller's motors; `Ok(None)` when it cannot rumble.
    pub fn open(controller: &Retained<GCController>) -> Result<Option<Self>, String> {
        // SAFETY: getters on a live controller; the locality statics are framework
        // constants.
        unsafe {
            let Some(haptics) = controller.haptics() else {
                return Ok(None);
            };
            let left = Motor::open(&haptics, GCHapticsLocalityLeftHandle)?;
            let right = Motor::open(&haptics, GCHapticsLocalityRightHandle)?;
            let whole = if left.is_none() && right.is_none() {
                Motor::open(&haptics, GCHapticsLocalityDefault)?
            } else {
                None
            };
            if left.is_none() && right.is_none() && whole.is_none() {
                return Ok(None);
            }
            Ok(Some(Self {
                controller: controller.clone(),
                left,
                right,
                left_trigger: Motor::open(&haptics, GCHapticsLocalityLeftTrigger)?,
                right_trigger: Motor::open(&haptics, GCHapticsLocalityRightTrigger)?,
                whole,
            }))
        }
    }

    /// Whether these are `controller`'s motors.
    pub fn belong_to(&self, controller: &GCController) -> bool {
        std::ptr::eq(&*self.controller, controller)
    }

    /// Sets every motor. A trigger level the controller has no motor for is folded into
    /// the handle on its side, and everything into the single motor of a controller
    /// without separate handles.
    pub fn apply(&mut self, levels: MotorLevels) -> Result<(), String> {
        let mut left = levels.left;
        let mut right = levels.right;
        match &mut self.left_trigger {
            Some(motor) => motor.set(levels.left_trigger)?,
            None => left = left.max(levels.left_trigger),
        }
        match &mut self.right_trigger {
            Some(motor) => motor.set(levels.right_trigger)?,
            None => right = right.max(levels.right_trigger),
        }
        if let Some(motor) = &mut self.whole {
            return motor.set(left.max(right));
        }
        // One handle only: it carries both sides.
        match (&mut self.left, &mut self.right) {
            (Some(l), Some(r)) => {
                l.set(left)?;
                r.set(right)
            }
            (Some(only), None) | (None, Some(only)) => only.set(left.max(right)),
            (None, None) => Ok(()),
        }
    }
}
