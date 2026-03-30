use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::traits::{RunSummary, VirtualUser};
use crate::vu_pool::VuPool;

/// Dispatches iterations at a fixed rate, borrowing VUs from a pool.
///
/// This is the executor that causes OOM in k6. Our design guarantees
/// bounded memory: when the pool is exhausted (server too slow),
/// iterations are dropped rather than allocating more VUs.
///
/// The `dropped_iterations` count IS the load test result —
/// it tells you the server couldn't sustain the target rate.
pub struct ConstantArrivalRateExecutor<V: VirtualUser + 'static> {
    pool: Arc<VuPool<V>>,
    rate: u32,
    time_unit: Duration,
    duration: Duration,
}

impl<V: VirtualUser + 'static> ConstantArrivalRateExecutor<V> {
    pub fn new(
        pool: Arc<VuPool<V>>,
        rate: u32,
        time_unit: Duration,
        duration: Duration,
    ) -> Self {
        Self {
            pool,
            rate,
            time_unit,
            duration,
        }
    }

    pub async fn run(&self, cancel: CancellationToken) -> Result<RunSummary> {
        let start = Instant::now();
        let deadline = start + self.duration;
        let iterations_completed = Arc::new(AtomicU64::new(0));

        // Calculate interval between dispatches
        let interval = self.time_unit / self.rate;
        let mut ticker = tokio::time::interval(interval);
        // Don't try to "catch up" missed ticks — just skip them
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Track spawned tasks so we can wait for them
        let mut handles = vec![];

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = cancel.cancelled() => break,
            }

            if Instant::now() >= deadline {
                break;
            }

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
                        // guard dropped here → VU returned to pool
                    });
                    handles.push(handle);
                }
                None => {
                    self.pool.record_dropped();
                }
            }
        }

        // Wait for all in-flight iterations to complete
        for handle in handles {
            let _ = handle.await;
        }

        let elapsed = start.elapsed();

        Ok(RunSummary {
            iterations_completed: iterations_completed.load(Ordering::Relaxed),
            iterations_dropped: self.pool.dropped_iterations(),
            duration: elapsed,
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

    #[tokio::test]
    async fn fast_vu_no_drops() {
        // 10 VUs, 1ms per iteration, rate=50/s, 200ms duration
        // At 50/s for 200ms = ~10 iterations needed, 10 VUs available,
        // each finishes in 1ms — should never exhaust pool
        let vus: Vec<MockVu> = (0..10)
            .map(|_| MockVu::new(Duration::from_millis(1)))
            .collect();

        let pool = Arc::new(VuPool::new(vus));
        let executor = ConstantArrivalRateExecutor::new(
            pool.clone(),
            50,
            Duration::from_secs(1),
            Duration::from_millis(200),
        );

        let summary = executor.run(CancellationToken::new()).await.unwrap();

        assert_eq!(
            summary.iterations_dropped, 0,
            "fast VUs should not drop iterations"
        );
        assert!(
            summary.iterations_completed >= 5,
            "expected >= 5 completed, got {}",
            summary.iterations_completed
        );
    }

    #[tokio::test]
    async fn slow_vu_causes_drops() {
        // 2 VUs, 200ms per iteration, rate=50/s, 300ms duration
        // At 50/s we need a new VU every 20ms, but each VU takes 200ms
        // so only 2 can run at once → massive drops expected
        let vus: Vec<MockVu> = (0..2)
            .map(|_| MockVu::new(Duration::from_millis(200)))
            .collect();

        let pool = Arc::new(VuPool::new(vus));
        let executor = ConstantArrivalRateExecutor::new(
            pool.clone(),
            50,
            Duration::from_secs(1),
            Duration::from_millis(300),
        );

        let summary = executor.run(CancellationToken::new()).await.unwrap();

        assert!(
            summary.iterations_dropped > 0,
            "slow VUs should cause drops, got 0 drops"
        );
        assert!(
            summary.iterations_completed > 0,
            "should complete some iterations"
        );
        // Total attempts ≈ 50/s * 0.3s = 15
        let total = summary.iterations_completed + summary.iterations_dropped;
        assert!(
            total >= 5,
            "expected >= 5 total attempts, got {total}"
        );
    }

    #[tokio::test]
    async fn respects_cancellation() {
        let vus: Vec<MockVu> = (0..5)
            .map(|_| MockVu::new(Duration::from_millis(1)))
            .collect();

        let pool = Arc::new(VuPool::new(vus));
        let executor = ConstantArrivalRateExecutor::new(
            pool,
            100,
            Duration::from_secs(1),
            Duration::from_secs(60), // long duration
        );

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel_clone.cancel();
        });

        let summary = executor.run(cancel).await.unwrap();

        // Should have stopped well before 60s
        assert!(summary.duration < Duration::from_secs(1));
        assert!(summary.iterations_completed > 0);
    }

    #[tokio::test]
    async fn rate_approximation() {
        // 20 VUs, 1ms per iteration, rate=100/s, 500ms duration
        // Expected: ~50 iterations
        let vus: Vec<MockVu> = (0..20)
            .map(|_| MockVu::new(Duration::from_millis(1)))
            .collect();

        let pool = Arc::new(VuPool::new(vus));
        let executor = ConstantArrivalRateExecutor::new(
            pool.clone(),
            100,
            Duration::from_secs(1),
            Duration::from_millis(500),
        );

        let summary = executor.run(CancellationToken::new()).await.unwrap();

        // Allow ±50% tolerance for CI timing jitter
        assert!(
            summary.iterations_completed >= 25 && summary.iterations_completed <= 80,
            "expected ~50 iterations (25-80 range), got {}",
            summary.iterations_completed
        );
        assert_eq!(summary.iterations_dropped, 0);
    }

    #[tokio::test]
    async fn memory_bound_guarantee() {
        // The critical property: pool size never grows.
        // 3 VUs, slow iterations — pool is the memory bound.
        let vus: Vec<MockVu> = (0..3)
            .map(|_| MockVu::new(Duration::from_millis(50)))
            .collect();

        let pool = Arc::new(VuPool::new(vus));
        assert_eq!(pool.capacity(), 3);

        let executor = ConstantArrivalRateExecutor::new(
            pool.clone(),
            100,
            Duration::from_secs(1),
            Duration::from_millis(200),
        );

        let summary = executor.run(CancellationToken::new()).await.unwrap();

        // Pool capacity unchanged — this IS the memory guarantee
        assert_eq!(pool.capacity(), 3);
        // Some iterations must have been dropped (3 VUs can't sustain 100/s)
        assert!(summary.iterations_dropped > 0);
        // All VUs should be returned to pool after run
        assert_eq!(pool.available_count(), 3);
    }
}
