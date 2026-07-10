use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::config::Stage;
use crate::executor::arrival::ArrivalCurve;
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

        // The arrival integral lives in `ArrivalCurve` (shared, JS-free) — the
        // same curve the coroutine arrival-rate coordinator consumes, so arrival
        // instants are identical by construction. We dispatch to catch up to the
        // integral each tick, so the total iteration count equals the analytical
        // area under the ramp — matching upstream k6. (Sampling instantaneous
        // rate + sleeping `1/rate` instead under-dispatched ramps by ~8%.)
        let curve = ArrivalCurve::new(self.start_rate, &self.stages, self.time_unit);
        let total_duration = curve.total_duration();
        let mut dispatched: u64 = 0;

        loop {
            if cancel.is_cancelled() {
                break;
            }

            let elapsed = start.elapsed();
            let clamped = elapsed.min(total_duration);
            let target = curve.expected_arrivals(clamped);

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
            let inst_rate = curve.interpolate_rate(clamped);
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

    // The curve integral + rate interpolation are unit-tested in
    // `executor::arrival` (their extracted home); these tests exercise the
    // EXECUTOR's use of the shared curve — dispatch, drops, cancellation.

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
