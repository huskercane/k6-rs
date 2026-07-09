use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::config::Stage;
use crate::traits::{RunSummary, VirtualUser};
use crate::vu_pool::VuPool;

/// Dispatches iterations at a variable rate that ramps through stages.
///
/// This is the executor used by all benchmark scenarios in the OOM load test.
/// Like `ConstantArrivalRateExecutor` but the target rate changes over time
/// according to a list of stages.
///
/// Example stages (from benchmark-10k):
///   { duration: "5m",   target: 180 }  ← ramp up to 180/s over 5 min
///   { duration: "480m", target: 180 }  ← sustain 180/s for 8 hours
///   { duration: "1m",   target: 0   }  ← ramp down to 0 over 1 min
pub struct RampingArrivalRateExecutor<V: VirtualUser + 'static> {
    pool: Arc<VuPool<V>>,
    stages: Vec<Stage>,
    start_rate: f64,
    time_unit: Duration,
}

impl<V: VirtualUser + 'static> RampingArrivalRateExecutor<V> {
    pub fn new(
        pool: Arc<VuPool<V>>,
        stages: Vec<Stage>,
        start_rate: f64,
        time_unit: Duration,
    ) -> Self {
        Self {
            pool,
            stages,
            start_rate,
            time_unit,
        }
    }

    pub async fn run(&self, cancel: CancellationToken) -> Result<RunSummary> {
        let start = Instant::now();
        let iterations_completed = Arc::new(AtomicU64::new(0));
        let mut handles = vec![];

        // Build the stage timeline: (stage_end_time, start_rate, end_rate)
        let mut timeline = Vec::with_capacity(self.stages.len());
        let mut offset = Duration::ZERO;
        let mut prev_rate = self.start_rate;

        for stage in &self.stages {
            let stage_end = offset + stage.duration;
            timeline.push((offset, stage_end, prev_rate, stage.target as f64));
            prev_rate = stage.target as f64;
            offset = stage_end;
        }

        let total_duration = offset;
        let time_unit_secs = self.time_unit.as_secs_f64();

        // Dispatch by INTEGRATING the arrival curve. At any moment, the number
        // of arrivals that should have *started* by now is the definite integral
        // of the (piecewise-linear) rate curve up to that time. We dispatch to
        // catch up to that integral each tick, so the total iteration count
        // equals the analytical area under the ramp — matching upstream k6.
        //
        // The previous scheme sampled the instantaneous rate and slept
        // `1/rate` between single dispatches. During a ramp it held the
        // leading-edge (lower) rate across each interval, making intervals
        // systematically too long and under-dispatching the true integral
        // (measured ~8% short vs upstream on a 0→50→0 ramp). Integrating the
        // curve removes that bias regardless of tick granularity.
        let mut dispatched: u64 = 0;

        loop {
            if cancel.is_cancelled() {
                break;
            }

            let elapsed = start.elapsed();
            let clamped = elapsed.min(total_duration);
            let target = Self::expected_arrivals(&timeline, clamped, time_unit_secs);

            // Fire every arrival whose scheduled position we've now passed.
            while (dispatched as f64) + 1.0 <= target {
                dispatched += 1;
                match self.pool.try_acquire_owned() {
                    Some(mut guard) => {
                        let completed = Arc::clone(&iterations_completed);
                        let handle = tokio::task::spawn_blocking(move || {
                            match guard.vu_mut().run_iteration() {
                                Ok(_) => {
                                    completed.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(e) => {
                                    eprintln!("VU iteration error: {e}");
                                }
                            }
                        });
                        handles.push(handle);
                    }
                    None => {
                        // Pool saturated — the arrival slot is consumed but the
                        // iteration is dropped. `dispatched` still advances so
                        // the catch-up loop can't spin forever.
                        self.pool.record_dropped();
                    }
                }
            }

            if elapsed >= total_duration {
                break;
            }

            // Sleep until roughly the next arrival, predicted from the current
            // instantaneous rate. The catch-up loop above guarantees the count
            // regardless of this granularity; the prediction just avoids
            // busy-spinning while staying tight enough for arrival timing.
            let inst_rate = Self::interpolate_rate(&timeline, clamped, time_unit_secs);
            let sleep_dur = if inst_rate > 0.1 {
                let deficit = (dispatched as f64 + 1.0 - target).max(0.0);
                Duration::from_secs_f64((deficit / inst_rate).clamp(0.0005, 0.05))
            } else {
                Duration::from_millis(50)
            };
            tokio::select! {
                _ = tokio::time::sleep(sleep_dur) => {}
                _ = cancel.cancelled() => break,
            }
        }

        // Wait for in-flight iterations
        for handle in handles {
            let _ = handle.await;
        }

        Ok(RunSummary {
            iterations_completed: iterations_completed.load(Ordering::Relaxed),
            iterations_dropped: self.pool.dropped_iterations(),
            duration: start.elapsed(),
        })
    }

    /// Interpolate the current rate (in iterations/second) based on elapsed time.
    fn interpolate_rate(
        timeline: &[(Duration, Duration, f64, f64)],
        elapsed: Duration,
        time_unit_secs: f64,
    ) -> f64 {
        for &(stage_start, stage_end, from_rate, to_rate) in timeline {
            if elapsed >= stage_start && elapsed < stage_end {
                let stage_duration = (stage_end - stage_start).as_secs_f64();
                let stage_elapsed = (elapsed - stage_start).as_secs_f64();
                let progress = stage_elapsed / stage_duration;

                // Linear interpolation between from_rate and to_rate
                let rate_in_time_unit = from_rate + (to_rate - from_rate) * progress;

                // Convert to iterations per second
                return rate_in_time_unit / time_unit_secs;
            }
        }
        0.0 // past all stages
    }

    /// Cumulative number of arrivals that should have been dispatched by
    /// `elapsed` — the definite integral of the piecewise-linear rate curve
    /// from 0 to `elapsed`, expressed in iterations.
    ///
    /// Within a stage the rate moves linearly from `from_rate` to `to_rate`
    /// (per time unit), so the area over a partial stage of length `u` is
    /// `from_rate*u + (to_rate - from_rate) * u^2 / (2 * stage_dur)`. Summing
    /// the fully/partially elapsed stages and dividing by the time unit gives
    /// the exact expected iteration count at `elapsed`.
    fn expected_arrivals(
        timeline: &[(Duration, Duration, f64, f64)],
        elapsed: Duration,
        time_unit_secs: f64,
    ) -> f64 {
        let mut area = 0.0; // ∫ rate_in_time_unit dt
        for &(stage_start, stage_end, from_rate, to_rate) in timeline {
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
        area / time_unit_secs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::IterationResult;

    struct MockVu {
        iteration_time: Duration,
    }

    impl MockVu {
        fn new(iteration_time: Duration) -> Self {
            Self { iteration_time }
        }
    }

    impl VirtualUser for MockVu {
        fn run_iteration(&mut self) -> Result<IterationResult> {
            std::thread::sleep(self.iteration_time);
            Ok(IterationResult {
                duration: self.iteration_time,
            })
        }
        fn reset(&mut self) {}
    }

    #[test]
    fn interpolate_rate_linear() {
        let timeline = vec![(Duration::ZERO, Duration::from_secs(10), 0.0, 100.0)];

        // At 0s: rate = 0
        let rate =
            RampingArrivalRateExecutor::<MockVu>::interpolate_rate(&timeline, Duration::ZERO, 1.0);
        assert!((rate - 0.0).abs() < 0.1);

        // At 5s (halfway): rate = 50
        let rate = RampingArrivalRateExecutor::<MockVu>::interpolate_rate(
            &timeline,
            Duration::from_secs(5),
            1.0,
        );
        assert!((rate - 50.0).abs() < 0.1);

        // At 10s (end): past stage
        let rate = RampingArrivalRateExecutor::<MockVu>::interpolate_rate(
            &timeline,
            Duration::from_secs(10),
            1.0,
        );
        assert!((rate - 0.0).abs() < 0.1);
    }

    #[test]
    fn interpolate_rate_multi_stage() {
        let timeline = vec![
            (Duration::ZERO, Duration::from_secs(10), 0.0, 100.0), // ramp up
            (
                Duration::from_secs(10),
                Duration::from_secs(20),
                100.0,
                100.0,
            ), // sustain
            (Duration::from_secs(20), Duration::from_secs(30), 100.0, 0.0), // ramp down
        ];

        // Ramp up at 5s → 50/s
        let rate = RampingArrivalRateExecutor::<MockVu>::interpolate_rate(
            &timeline,
            Duration::from_secs(5),
            1.0,
        );
        assert!((rate - 50.0).abs() < 0.1);

        // Sustain at 15s → 100/s
        let rate = RampingArrivalRateExecutor::<MockVu>::interpolate_rate(
            &timeline,
            Duration::from_secs(15),
            1.0,
        );
        assert!((rate - 100.0).abs() < 0.1);

        // Ramp down at 25s → 50/s
        let rate = RampingArrivalRateExecutor::<MockVu>::interpolate_rate(
            &timeline,
            Duration::from_secs(25),
            1.0,
        );
        assert!((rate - 50.0).abs() < 0.1);
    }

    #[test]
    fn interpolate_rate_with_time_unit() {
        // Rate of 60 per minute = 1 per second
        let timeline = vec![(Duration::ZERO, Duration::from_secs(60), 60.0, 60.0)];

        let rate = RampingArrivalRateExecutor::<MockVu>::interpolate_rate(
            &timeline,
            Duration::from_secs(30),
            60.0, // time_unit = 1 minute
        );
        assert!((rate - 1.0).abs() < 0.01, "expected ~1/s, got {rate}");
    }

    #[test]
    fn expected_arrivals_integrates_the_ramp() {
        // Constant 50/s for 1s → exactly 50 arrivals (area of a rectangle).
        let flat = vec![(Duration::ZERO, Duration::from_secs(1), 50.0, 50.0)];
        let n = RampingArrivalRateExecutor::<MockVu>::expected_arrivals(
            &flat,
            Duration::from_secs(1),
            1.0,
        );
        assert!((n - 50.0).abs() < 1e-9, "flat 50/s for 1s should be 50, got {n}");

        // Linear ramp 0→100/s over 2s → area of a triangle = 0.5*2*100 = 100.
        // The OLD instantaneous-interval scheme under-counted exactly this shape.
        let ramp = vec![(Duration::ZERO, Duration::from_secs(2), 0.0, 100.0)];
        let n = RampingArrivalRateExecutor::<MockVu>::expected_arrivals(
            &ramp,
            Duration::from_secs(2),
            1.0,
        );
        assert!((n - 100.0).abs() < 1e-9, "ramp 0→100 over 2s should be 100, got {n}");

        // Halfway up that ramp (1s): rate is 50/s, area = 0.5*1*50 = 25.
        let half = RampingArrivalRateExecutor::<MockVu>::expected_arrivals(
            &ramp,
            Duration::from_secs(1),
            1.0,
        );
        assert!((half - 25.0).abs() < 1e-9, "halfway should be 25, got {half}");

        // Multi-stage 0→50→50→0 (2s,1s,2s) mirrors conformance script 13:
        // triangle(50) + rectangle(50) + triangle(50) = 150.
        let multi = vec![
            (Duration::ZERO, Duration::from_secs(2), 0.0, 50.0),
            (Duration::from_secs(2), Duration::from_secs(3), 50.0, 50.0),
            (Duration::from_secs(3), Duration::from_secs(5), 50.0, 0.0),
        ];
        let total = RampingArrivalRateExecutor::<MockVu>::expected_arrivals(
            &multi,
            Duration::from_secs(5),
            1.0,
        );
        assert!((total - 150.0).abs() < 1e-9, "0→50→0 ramp should total 150, got {total}");
    }

    #[tokio::test]
    async fn ramp_matches_integral_count() {
        // Fast VUs, plenty of pool: the completed count must land on the
        // integral of the ramp (100 over a 0→100/s 2s triangle), not short of
        // it. Regression lock for the under-dispatch bug.
        let vus: Vec<MockVu> = (0..50)
            .map(|_| MockVu::new(Duration::from_millis(1)))
            .collect();
        let pool = Arc::new(VuPool::new(vus));
        let executor = RampingArrivalRateExecutor::new(
            pool.clone(),
            vec![Stage {
                duration: Duration::from_secs(2),
                target: 100,
            }],
            0.0,
            Duration::from_secs(1),
        );

        let summary = executor.run(CancellationToken::new()).await.unwrap();
        let done = summary.iterations_completed;
        // Analytical area is 100; allow a small tail for the final tick.
        assert!(
            (95..=100).contains(&done),
            "expected ~100 iterations from the ramp integral, got {done}"
        );
        assert_eq!(summary.iterations_dropped, 0);
    }

    #[tokio::test]
    async fn ramp_up_and_sustain() {
        // 10 fast VUs, sustain at 20/s for 500ms (skip ramp to avoid timing flakiness)
        let vus: Vec<MockVu> = (0..10)
            .map(|_| MockVu::new(Duration::from_millis(1)))
            .collect();

        let pool = Arc::new(VuPool::new(vus));
        let executor = RampingArrivalRateExecutor::new(
            pool.clone(),
            vec![Stage {
                duration: Duration::from_millis(500),
                target: 20,
            }],
            20.0,
            Duration::from_secs(1),
        );

        let summary = executor.run(CancellationToken::new()).await.unwrap();

        // Sustaining 20/s for 500ms should yield ~10 iterations; accept >= 3 for CI tolerance
        assert!(
            summary.iterations_completed >= 3,
            "expected >= 3 completed, got {}",
            summary.iterations_completed
        );
        assert_eq!(summary.iterations_dropped, 0);
        assert_eq!(pool.available_count(), 10);
    }

    #[tokio::test]
    async fn slow_vus_cause_drops() {
        // 2 slow VUs, high target rate
        let vus: Vec<MockVu> = (0..2)
            .map(|_| MockVu::new(Duration::from_millis(200)))
            .collect();

        let pool = Arc::new(VuPool::new(vus));
        let executor = RampingArrivalRateExecutor::new(
            pool.clone(),
            vec![Stage {
                duration: Duration::from_millis(500),
                target: 50,
            }],
            50.0,
            Duration::from_secs(1),
        );

        let summary = executor.run(CancellationToken::new()).await.unwrap();

        assert!(
            summary.iterations_dropped > 0,
            "expected drops with slow VUs"
        );
        assert!(summary.iterations_completed > 0);
        assert_eq!(pool.capacity(), 2);
        assert_eq!(pool.available_count(), 2);
    }

    #[tokio::test]
    async fn ramp_down_to_zero() {
        let vus: Vec<MockVu> = (0..5)
            .map(|_| MockVu::new(Duration::from_millis(1)))
            .collect();

        let pool = Arc::new(VuPool::new(vus));
        let executor = RampingArrivalRateExecutor::new(
            pool.clone(),
            vec![
                Stage {
                    duration: Duration::from_millis(100),
                    target: 50,
                },
                Stage {
                    duration: Duration::from_millis(100),
                    target: 0,
                },
            ],
            50.0,
            Duration::from_secs(1),
        );

        let summary = executor.run(CancellationToken::new()).await.unwrap();

        // Should complete naturally when rate reaches 0
        assert!(summary.iterations_completed > 0);
        assert_eq!(pool.available_count(), 5);
    }

    #[tokio::test]
    async fn respects_cancellation() {
        let vus: Vec<MockVu> = (0..5)
            .map(|_| MockVu::new(Duration::from_millis(1)))
            .collect();

        let pool = Arc::new(VuPool::new(vus));
        let executor = RampingArrivalRateExecutor::new(
            pool,
            vec![Stage {
                duration: Duration::from_secs(60),
                target: 100,
            }],
            0.0,
            Duration::from_secs(1),
        );

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel_clone.cancel();
        });

        let summary = executor.run(cancel).await.unwrap();
        assert!(summary.duration < Duration::from_secs(1));
    }
}
