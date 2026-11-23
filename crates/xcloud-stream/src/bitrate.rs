//! Loss-aware bitrate limit for the REMB the pipeline sends.
//!
//! The sender treats REMB as a ceiling. Asking for the configured maximum regardless of
//! the link leaves any reduction to the sender, which only sees the loss the receiver
//! reports after retransmits repaired it. On a lossy link that keeps the stream at a
//! bitrate where every frame is many packets and a single unrecovered one freezes the
//! picture until a keyframe. This controller lowers the ceiling while packets are being
//! lost and raises it again when the link is clean, trading sharpness for a picture that
//! keeps moving.
//!
//! It is the classic loss-based rule (multiplicative decrease, slow increase) once per
//! REMB interval:
//!
//! | interval                                              | limit                         |
//! |-------------------------------------------------------|-------------------------------|
//! | loss at or above [`HIGH_LOSS`]                        | lowered                       |
//! | a frame was dropped and loss is above [`LOW_LOSS`]    | lowered                       |
//! | loss at or below [`LOW_LOSS`]                         | raised by 8 % plus 100 kbit/s |
//! | otherwise                                             | held                          |

/// Loss at or below this is a clean link: the limit climbs.
pub const LOW_LOSS: f32 = 0.01;
/// Loss at or above this lowers the limit.
pub const HIGH_LOSS: f32 = 0.05;
/// Loss at or above this lowers the limit harder.
const HEAVY_LOSS: f32 = 0.20;
/// The limit never goes below this (or the configured maximum, when that is lower).
const FLOOR_BPS: u32 = 2_000_000;

/// What the link did during one REMB interval.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Interval {
    /// Fraction of video packets that did not arrive first time (`0.0..=1.0`),
    /// before retransmits.
    pub loss: f32,
    /// Frames given up on (each one froze the picture until a keyframe).
    pub frames_dropped: u32,
    /// Bitrate that actually arrived, 0 when unknown.
    pub received_bps: u32,
}

/// The bitrate ceiling to ask the sender for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitrateController {
    max_bps: u32,
    target_bps: u32,
}

impl BitrateController {
    pub fn new(max_bps: u32) -> Self {
        Self {
            max_bps,
            target_bps: max_bps,
        }
    }

    /// Changes the configured maximum; the limit starts over from it.
    pub fn set_max(&mut self, max_bps: u32) {
        *self = Self::new(max_bps);
    }

    /// Forgets the link's history (a new stream).
    pub fn reset(&mut self) {
        self.target_bps = self.max_bps;
    }

    /// The ceiling to send.
    pub fn target_bps(&self) -> u32 {
        self.target_bps
    }

    /// Whether the ceiling is below the configured maximum.
    pub fn is_limiting(&self) -> bool {
        self.target_bps < self.max_bps
    }

    /// Folds one interval in and returns the new ceiling.
    pub fn update(&mut self, interval: Interval) -> u32 {
        let floor = FLOOR_BPS.min(self.max_bps);
        let lossy =
            interval.loss >= HIGH_LOSS || (interval.frames_dropped > 0 && interval.loss > LOW_LOSS);
        if lossy {
            // Start from what actually arrived: lowering a ceiling the sender is not
            // reaching changes nothing. A stalled interval reads artificially low, so
            // one step never more than halves the base.
            let base = match interval.received_bps {
                0 => self.target_bps,
                received => self.target_bps.min(received.max(self.target_bps / 2)),
            };
            let factor = if interval.loss >= HEAVY_LOSS {
                0.7
            } else {
                0.85
            };
            self.target_bps = ((f64::from(base) * factor) as u32).max(floor);
        } else if interval.loss <= LOW_LOSS {
            let raised = (f64::from(self.target_bps) * 1.08) as u32 + 100_000;
            self.target_bps = raised.min(self.max_bps);
        }
        self.target_bps = self.target_bps.clamp(floor, self.max_bps);
        self.target_bps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: u32 = 20_000_000;

    fn interval(loss: f32, frames_dropped: u32, received_bps: u32) -> Interval {
        Interval {
            loss,
            frames_dropped,
            received_bps,
        }
    }

    #[test]
    fn a_clean_link_keeps_the_maximum() {
        let mut c = BitrateController::new(MAX);
        for _ in 0..10 {
            assert_eq!(c.update(interval(0.0, 0, 15_000_000)), MAX);
        }
        assert!(!c.is_limiting());
        // A stray lost frame without measurable loss is not a lossy link.
        assert_eq!(c.update(interval(0.002, 1, 15_000_000)), MAX);
        // Moderate loss holds.
        assert_eq!(c.update(interval(0.03, 0, 15_000_000)), MAX);
    }

    #[test]
    fn loss_lowers_the_limit_from_what_arrives() {
        let mut c = BitrateController::new(MAX);
        // 8 % loss while 12 Mbit/s arrive: 85 % of that, not of the 20 Mbit/s ceiling.
        assert_eq!(c.update(interval(0.08, 0, 12_000_000)), 10_200_000);
        assert!(c.is_limiting());
        // Heavy loss cuts harder.
        assert_eq!(c.update(interval(0.25, 0, 10_000_000)), 7_000_000);
        // A dropped frame with some loss lowers it too.
        assert_eq!(c.update(interval(0.02, 1, 7_000_000)), 5_950_000);
        // Without a measurement the previous limit is the base.
        assert_eq!(c.update(interval(0.08, 0, 0)), 5_057_500);
    }

    #[test]
    fn a_stalled_interval_cannot_collapse_the_limit() {
        let mut c = BitrateController::new(MAX);
        // Almost nothing arrived during a freeze: the base is half the limit at worst.
        assert_eq!(c.update(interval(0.10, 3, 500_000)), 8_500_000);
    }

    #[test]
    fn the_limit_has_a_floor_and_climbs_back() {
        let mut c = BitrateController::new(MAX);
        for _ in 0..40 {
            c.update(interval(0.30, 2, 1_000_000));
        }
        assert_eq!(c.target_bps(), FLOOR_BPS);
        // Clean again: 8 % plus 100 kbit/s a second, back to the maximum.
        assert_eq!(c.update(interval(0.0, 0, 2_000_000)), 2_260_000);
        let mut seconds = 1;
        while c.is_limiting() {
            c.update(interval(0.0, 0, c.target_bps()));
            seconds += 1;
        }
        assert_eq!(c.target_bps(), MAX);
        assert!((20..=30).contains(&seconds), "took {seconds} s");
    }

    #[test]
    fn a_low_maximum_is_its_own_floor() {
        let mut c = BitrateController::new(1_500_000);
        assert_eq!(c.update(interval(0.5, 5, 1_000_000)), 1_500_000);
        assert!(!c.is_limiting());
        c.set_max(MAX);
        assert_eq!(c.target_bps(), MAX);
        c.update(interval(0.10, 0, 10_000_000));
        c.reset();
        assert_eq!(c.target_bps(), MAX);
    }
}
