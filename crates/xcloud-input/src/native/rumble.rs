//! The timeline of a server vibration report, for backends that drive motor levels
//! themselves (gilrs schedules its own effects).
//!
//! A report plays `repeat_count + 1` cycles; each cycle waits `delay_ms` and then runs
//! the motors for `duration_ms`.

use xcloud_proto::input::packet::VibrationReport;

/// Motor levels `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MotorLevels {
    pub left: f32,
    pub right: f32,
    pub left_trigger: f32,
    pub right_trigger: f32,
}

impl MotorLevels {
    pub const OFF: Self = Self {
        left: 0.0,
        right: 0.0,
        left_trigger: 0.0,
        right_trigger: 0.0,
    };

    /// The report's motor strengths scaled by the user's `intensity`.
    pub fn of(report: &VibrationReport, intensity: f32) -> Self {
        let scale = intensity.clamp(0.0, 1.0);
        let level = |percent: f32| percent.clamp(0.0, 1.0) * scale;
        Self {
            left: level(report.left_motor_percent),
            right: level(report.right_motor_percent),
            left_trigger: level(report.left_trigger_motor_percent),
            right_trigger: level(report.right_trigger_motor_percent),
        }
    }
}

/// The levels `elapsed_ms` after the report arrived; `None` once it has finished.
pub fn levels_at(report: &VibrationReport, intensity: f32, elapsed_ms: u64) -> Option<MotorLevels> {
    let (delay, duration) = (u64::from(report.delay_ms), u64::from(report.duration_ms));
    let cycle = delay + duration;
    let cycles = u64::from(report.repeat_count) + 1;
    if duration == 0 || elapsed_ms >= cycle * cycles {
        return None;
    }
    Some(if elapsed_ms % cycle < delay {
        MotorLevels::OFF
    } else {
        MotorLevels::of(report, intensity)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> VibrationReport {
        VibrationReport {
            left_motor_percent: 1.0,
            right_motor_percent: 0.5,
            left_trigger_motor_percent: 0.2,
            duration_ms: 100,
            delay_ms: 50,
            repeat_count: 1,
            ..VibrationReport::default()
        }
    }

    #[test]
    fn levels_scale_with_the_user_intensity() {
        let levels = MotorLevels::of(&report(), 0.5);
        assert_eq!(levels.left, 0.5);
        assert_eq!(levels.right, 0.25);
        assert_eq!(levels.left_trigger, 0.1);
        assert_eq!(levels.right_trigger, 0.0);
        assert_eq!(MotorLevels::of(&report(), 0.0), MotorLevels::OFF);
        assert_eq!(
            MotorLevels::of(&report(), 7.0).left,
            1.0,
            "intensity is clamped"
        );
    }

    #[test]
    fn each_cycle_waits_then_plays() {
        let r = report();
        let on = Some(MotorLevels::of(&r, 1.0));
        let off = Some(MotorLevels::OFF);
        assert_eq!(levels_at(&r, 1.0, 0), off, "first delay");
        assert_eq!(levels_at(&r, 1.0, 49), off);
        assert_eq!(levels_at(&r, 1.0, 50), on, "first play");
        assert_eq!(levels_at(&r, 1.0, 149), on);
        assert_eq!(levels_at(&r, 1.0, 150), off, "second cycle's delay");
        assert_eq!(levels_at(&r, 1.0, 200), on);
        assert_eq!(levels_at(&r, 1.0, 299), on);
        assert_eq!(levels_at(&r, 1.0, 300), None, "finished after two cycles");
    }

    #[test]
    fn a_report_without_duration_never_plays() {
        let r = VibrationReport {
            left_motor_percent: 1.0,
            ..VibrationReport::default()
        };
        assert_eq!(levels_at(&r, 1.0, 0), None);
    }
}
