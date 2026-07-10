//! The ramping-VUs schedule: how many VUs should be ACTIVE at time t for a
//! piecewise-linear VU-count ramp. JS-free and executor-agnostic — shared by the
//! sync `RampingVusExecutor` and the coroutine ramping-VUs executor (`k6_js::pool`)
//! so the active-count curve (and its round-not-truncate correctness) has one home.

use std::time::Duration;

use crate::config::Stage;

/// A piecewise-linear active-VU-count ramp. `timeline` holds one segment per stage
/// as `(stage_start, stage_end, from_vus, to_vus)`.
#[derive(Debug, Clone)]
pub struct VuRampSchedule {
    timeline: Vec<(Duration, Duration, u32, u32)>,
    total_duration: Duration,
}

impl VuRampSchedule {
    /// Build from a starting VU count and stages (each ramps linearly from the
    /// previous target to its own over its duration).
    pub fn new(start_vus: u32, stages: &[Stage]) -> Self {
        let mut timeline = Vec::with_capacity(stages.len());
        let mut offset = Duration::ZERO;
        let mut prev_target = start_vus;
        for stage in stages {
            let stage_end = offset + stage.duration;
            timeline.push((offset, stage_end, prev_target, stage.target));
            prev_target = stage.target;
            offset = stage_end;
        }
        Self {
            timeline,
            total_duration: offset,
        }
    }

    /// Total scheduled duration (sum of stage durations).
    pub fn total_duration(&self) -> Duration {
        self.total_duration
    }

    /// Desired active VU count at `elapsed`.
    ///
    /// Rounds, does NOT truncate: `as u32` floors, so a 0→5 ramp would hold one VU
    /// short for almost the whole ramp (4.9 → 4), costing VU-seconds and
    /// systematically under-running iterations vs upstream (measured ~12% short).
    /// Rounding tracks the intended linear count symmetrically up and down.
    pub fn interpolate(&self, elapsed: Duration) -> u32 {
        for &(stage_start, stage_end, from_vus, to_vus) in &self.timeline {
            if elapsed >= stage_start && elapsed < stage_end {
                let stage_duration = (stage_end - stage_start).as_secs_f64();
                let stage_elapsed = (elapsed - stage_start).as_secs_f64();
                let progress = stage_elapsed / stage_duration;
                return (from_vus as f64 + (to_vus as f64 - from_vus as f64) * progress).round()
                    as u32;
            }
        }
        0 // past all stages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolate_linear_rounds_not_truncates() {
        let s = VuRampSchedule::new(0, &[Stage { duration: Duration::from_secs(10), target: 10 }]);
        assert_eq!(s.interpolate(Duration::ZERO), 0);
        assert_eq!(s.interpolate(Duration::from_secs(5)), 5);
        // 4.9 rounds to 5 (truncation would give 4 — the ~12% under-run bug).
        assert_eq!(s.interpolate(Duration::from_millis(4900)), 5);
        assert_eq!(s.interpolate(Duration::from_secs(10)), 0); // past stage
        assert_eq!(s.total_duration(), Duration::from_secs(10));
    }

    #[test]
    fn interpolate_multi_stage_up_hold_down() {
        let s = VuRampSchedule::new(
            0,
            &[
                Stage { duration: Duration::from_secs(10), target: 10 }, // up
                Stage { duration: Duration::from_secs(10), target: 10 }, // hold
                Stage { duration: Duration::from_secs(10), target: 0 },  // down
            ],
        );
        assert_eq!(s.interpolate(Duration::from_secs(5)), 5);
        assert_eq!(s.interpolate(Duration::from_secs(15)), 10);
        assert_eq!(s.interpolate(Duration::from_secs(25)), 5);
        assert_eq!(s.total_duration(), Duration::from_secs(30));
    }
}
