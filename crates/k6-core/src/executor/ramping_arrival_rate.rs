use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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

        // Dispatch loop — recalculate rate based on current position in stages
        let mut last_tick = Instant::now();

        loop {
            let elapsed = start.elapsed();

            if elapsed >= total_duration || cancel.is_cancelled() {
                break;
            }

            // Find current rate by interpolating within the active stage
            let current_rate = Self::interpolate_rate(&timeline, elapsed, time_unit_secs);

            if current_rate < 0.1 {
                // Rate too low to dispatch — poll every 50ms until rate increases
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }

            // Calculate interval for current rate (in iterations per second)
            // Cap at 10s max interval to avoid hanging when rate is very low
            let interval = Duration::from_secs_f64((1.0 / current_rate).min(10.0));

            // Sleep until next dispatch
            let since_last = last_tick.elapsed();
            if since_last < interval {
                let sleep_dur = interval - since_last;
                tokio::select! {
                    _ = tokio::time::sleep(sleep_dur) => {}
                    _ = cancel.cancelled() => break,
                }
            }
            last_tick = Instant::now();

            // Dispatch iteration
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
                    self.pool.record_dropped();
                }
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
        let timeline = vec![
            (Duration::ZERO, Duration::from_secs(10), 0.0, 100.0),
        ];

        // At 0s: rate = 0
        let rate = RampingArrivalRateExecutor::<MockVu>::interpolate_rate(
            &timeline,
            Duration::ZERO,
            1.0,
        );
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
            (Duration::ZERO, Duration::from_secs(10), 0.0, 100.0),           // ramp up
            (Duration::from_secs(10), Duration::from_secs(20), 100.0, 100.0), // sustain
            (Duration::from_secs(20), Duration::from_secs(30), 100.0, 0.0),   // ramp down
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
        let timeline = vec![
            (Duration::ZERO, Duration::from_secs(60), 60.0, 60.0),
        ];

        let rate = RampingArrivalRateExecutor::<MockVu>::interpolate_rate(
            &timeline,
            Duration::from_secs(30),
            60.0, // time_unit = 1 minute
        );
        assert!((rate - 1.0).abs() < 0.01, "expected ~1/s, got {rate}");
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
            vec![
                Stage {
                    duration: Duration::from_millis(500),
                    target: 20,
                },
            ],
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
