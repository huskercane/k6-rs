//! The arrival-rate curve: a JS-free, executor-agnostic model of *how many
//! iterations should have started by time t* for a piecewise-linear rate schedule.
//!
//! This is the single source of truth for the arrival integral. Both the sync
//! `RampingArrivalRateExecutor` and the coroutine arrival-rate coordinator
//! (`k6_js::pool`) consume it, so the k-th arrival's scheduled position is
//! identical by construction — arrival-instant parity is structural, not
//! re-derived. Constant-arrival-rate is the degenerate case of one flat stage
//! ([`ArrivalCurve::constant`]).

use std::time::Duration;

use crate::config::Stage;

/// A piecewise-linear arrival-rate schedule. `timeline` holds one segment per
/// stage as `(stage_start, stage_end, from_rate, to_rate)` where rates are in
/// iterations per `time_unit`.
#[derive(Debug, Clone)]
pub struct ArrivalCurve {
    timeline: Vec<(Duration, Duration, f64, f64)>,
    total_duration: Duration,
    time_unit_secs: f64,
}

impl ArrivalCurve {
    /// Build from a starting rate and a list of stages (each ramps linearly from
    /// the previous rate to its `target` over its `duration`).
    pub fn new(start_rate: f64, stages: &[Stage], time_unit: Duration) -> Self {
        let mut timeline = Vec::with_capacity(stages.len());
        let mut offset = Duration::ZERO;
        let mut prev_rate = start_rate;
        for stage in stages {
            let stage_end = offset + stage.duration;
            timeline.push((offset, stage_end, prev_rate, stage.target as f64));
            prev_rate = stage.target as f64;
            offset = stage_end;
        }
        Self {
            timeline,
            total_duration: offset,
            time_unit_secs: time_unit.as_secs_f64(),
        }
    }

    /// Constant-arrival-rate: a single flat stage at `rate` per `time_unit` for
    /// `duration`. The integral is then simply `rate * elapsed / time_unit`, so a
    /// coordinator driving off this curve dispatches exactly as a fixed-interval
    /// ticker would — one code path for both arrival executors.
    pub fn constant(rate: u32, time_unit: Duration, duration: Duration) -> Self {
        Self::new(
            rate as f64,
            &[Stage {
                duration,
                target: rate,
            }],
            time_unit,
        )
    }

    /// Total scheduled duration of the curve (sum of stage durations).
    pub fn total_duration(&self) -> Duration {
        self.total_duration
    }

    /// Cumulative number of arrivals that should have started by `elapsed` — the
    /// definite integral of the piecewise-linear rate curve from 0 to `elapsed`,
    /// in iterations.
    ///
    /// Within a stage the rate moves linearly from `from_rate` to `to_rate` (per
    /// time unit), so the area over a partial stage of length `u` is
    /// `from_rate*u + (to_rate - from_rate) * u^2 / (2 * stage_dur)`. Summing the
    /// fully/partially elapsed stages and dividing by the time unit gives the exact
    /// expected iteration count at `elapsed`. Integrating (rather than sampling the
    /// instantaneous rate and sleeping `1/rate`) is what removes the ~8%
    /// under-dispatch bias on ramps.
    pub fn expected_arrivals(&self, elapsed: Duration) -> f64 {
        let mut area = 0.0; // ∫ rate_in_time_unit dt
        for &(stage_start, stage_end, from_rate, to_rate) in &self.timeline {
            if elapsed <= stage_start {
                break;
            }
            let stage_dur = (stage_end - stage_start).as_secs_f64();
            if stage_dur <= 0.0 {
                continue;
            }
            let u = (elapsed.min(stage_end) - stage_start).as_secs_f64();
            area += from_rate * u + (to_rate - from_rate) * u * u / (2.0 * stage_dur);
        }
        area / self.time_unit_secs
    }

    /// Instantaneous rate (iterations/second) at `elapsed` — used only to predict
    /// a sleep-until-next-arrival granularity; the catch-up loop against
    /// [`Self::expected_arrivals`] guarantees the count regardless.
    pub fn interpolate_rate(&self, elapsed: Duration) -> f64 {
        for &(stage_start, stage_end, from_rate, to_rate) in &self.timeline {
            if elapsed >= stage_start && elapsed < stage_end {
                let stage_duration = (stage_end - stage_start).as_secs_f64();
                let stage_elapsed = (elapsed - stage_start).as_secs_f64();
                let progress = stage_elapsed / stage_duration;
                let rate_in_time_unit = from_rate + (to_rate - from_rate) * progress;
                return rate_in_time_unit / self.time_unit_secs;
            }
        }
        0.0 // past all stages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expected_arrivals_integrates_the_ramp() {
        // Constant 50/s for 1s → exactly 50 (rectangle).
        let flat = ArrivalCurve::new(
            50.0,
            &[Stage { duration: Duration::from_secs(1), target: 50 }],
            Duration::from_secs(1),
        );
        assert!((flat.expected_arrivals(Duration::from_secs(1)) - 50.0).abs() < 1e-9);

        // Linear ramp 0→100/s over 2s → triangle = 100; halfway (1s) → 25.
        let ramp = ArrivalCurve::new(
            0.0,
            &[Stage { duration: Duration::from_secs(2), target: 100 }],
            Duration::from_secs(1),
        );
        assert!((ramp.expected_arrivals(Duration::from_secs(2)) - 100.0).abs() < 1e-9);
        assert!((ramp.expected_arrivals(Duration::from_secs(1)) - 25.0).abs() < 1e-9);

        // 0→50→50→0 (2s,1s,2s) = triangle+rect+triangle = 150.
        let multi = ArrivalCurve::new(
            0.0,
            &[
                Stage { duration: Duration::from_secs(2), target: 50 },
                Stage { duration: Duration::from_secs(1), target: 50 },
                Stage { duration: Duration::from_secs(2), target: 0 },
            ],
            Duration::from_secs(1),
        );
        assert!((multi.expected_arrivals(Duration::from_secs(5)) - 150.0).abs() < 1e-9);
        assert_eq!(multi.total_duration(), Duration::from_secs(5));
    }

    #[test]
    fn constant_curve_matches_flat_rate() {
        let c = ArrivalCurve::constant(50, Duration::from_secs(1), Duration::from_secs(2));
        // Flat 50/s → 50 by 1s, 100 by 2s.
        assert!((c.expected_arrivals(Duration::from_secs(1)) - 50.0).abs() < 1e-9);
        assert!((c.expected_arrivals(Duration::from_secs(2)) - 100.0).abs() < 1e-9);
        assert!((c.interpolate_rate(Duration::from_millis(500)) - 50.0).abs() < 1e-9);
    }

    #[test]
    fn interpolate_rate_multi_stage_and_time_unit() {
        let curve = ArrivalCurve::new(
            0.0,
            &[
                Stage { duration: Duration::from_secs(10), target: 100 }, // ramp up
                Stage { duration: Duration::from_secs(10), target: 100 }, // sustain
                Stage { duration: Duration::from_secs(10), target: 0 },   // ramp down
            ],
            Duration::from_secs(1),
        );
        assert!((curve.interpolate_rate(Duration::from_secs(5)) - 50.0).abs() < 0.1);
        assert!((curve.interpolate_rate(Duration::from_secs(15)) - 100.0).abs() < 0.1);
        assert!((curve.interpolate_rate(Duration::from_secs(25)) - 50.0).abs() < 0.1);

        // 60 per minute = 1/s.
        let per_min = ArrivalCurve::new(
            60.0,
            &[Stage { duration: Duration::from_secs(60), target: 60 }],
            Duration::from_secs(60),
        );
        assert!((per_min.interpolate_rate(Duration::from_secs(30)) - 1.0).abs() < 0.01);
    }
}
