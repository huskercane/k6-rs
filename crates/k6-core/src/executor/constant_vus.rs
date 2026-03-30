use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::traits::{RunSummary, VirtualUser};

/// Runs a fixed number of VUs for a specified duration.
///
/// Each VU executes iterations sequentially in its own blocking task.
/// When the duration expires (or cancellation is triggered), VUs finish
/// their current iteration and stop.
pub struct ConstantVusExecutor<V: VirtualUser + 'static> {
    vus: Vec<V>,
    duration: Duration,
}

impl<V: VirtualUser + 'static> ConstantVusExecutor<V> {
    pub fn new(vus: Vec<V>, duration: Duration) -> Self {
        Self { vus, duration }
    }

    pub async fn run(mut self, cancel: CancellationToken) -> Result<RunSummary> {
        let start = Instant::now();
        let deadline = start + self.duration;
        let total_iterations = Arc::new(AtomicU64::new(0));

        let mut handles = Vec::with_capacity(self.vus.len());

        // Drain VUs into blocking tasks — each VU runs its own loop
        for mut vu in self.vus.drain(..) {
            let iterations = Arc::clone(&total_iterations);
            let cancel = cancel.clone();

            let handle = tokio::task::spawn_blocking(move || {
                loop {
                    if Instant::now() >= deadline || cancel.is_cancelled() {
                        break;
                    }

                    match vu.run_iteration() {
                        Ok(_result) => {
                            iterations.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => {
                            eprintln!("VU iteration error: {e}");
                        }
                    }

                    vu.reset();
                }
            });

            handles.push(handle);
        }

        // Wait for all VUs to finish
        for handle in handles {
            let _ = handle.await;
        }

        let elapsed = start.elapsed();

        Ok(RunSummary {
            iterations_completed: total_iterations.load(Ordering::Relaxed),
            iterations_dropped: 0, // constant-vus never drops
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
    async fn runs_for_duration() {
        let vus: Vec<MockVu> = (0..3)
            .map(|_| MockVu::new(Duration::from_millis(10)))
            .collect();

        let executor = ConstantVusExecutor::new(vus, Duration::from_millis(200));
        let summary = executor.run(CancellationToken::new()).await.unwrap();

        // 3 VUs, each doing ~10ms iterations for 200ms ≈ 20 iterations each ≈ 60 total
        // Allow wide tolerance for CI/thread scheduling
        assert!(
            summary.iterations_completed >= 20,
            "expected >= 20 iterations, got {}",
            summary.iterations_completed
        );
        assert!(
            summary.iterations_completed <= 100,
            "expected <= 100 iterations, got {}",
            summary.iterations_completed
        );
        assert_eq!(summary.iterations_dropped, 0);
        assert!(summary.duration >= Duration::from_millis(180));
    }

    #[tokio::test]
    async fn respects_cancellation() {
        let vus: Vec<MockVu> = (0..2)
            .map(|_| MockVu::new(Duration::from_millis(10)))
            .collect();

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        // Cancel after 50ms
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel_clone.cancel();
        });

        let executor = ConstantVusExecutor::new(vus, Duration::from_secs(60));
        let summary = executor.run(cancel).await.unwrap();

        // Should have stopped well before the 60s duration
        assert!(summary.duration < Duration::from_secs(1));
        assert!(summary.iterations_completed > 0);
    }

    #[tokio::test]
    async fn single_vu() {
        let vus = vec![MockVu::new(Duration::from_millis(50))];

        let executor = ConstantVusExecutor::new(vus, Duration::from_millis(200));
        let summary = executor.run(CancellationToken::new()).await.unwrap();

        // 1 VU, 50ms per iteration, 200ms duration ≈ 4 iterations
        assert!(
            summary.iterations_completed >= 2 && summary.iterations_completed <= 8,
            "expected 2-8 iterations, got {}",
            summary.iterations_completed
        );
    }

    #[tokio::test]
    async fn zero_duration() {
        let vus = vec![MockVu::new(Duration::from_millis(1))];

        let executor = ConstantVusExecutor::new(vus, Duration::ZERO);
        let summary = executor.run(CancellationToken::new()).await.unwrap();

        // Should stop immediately (or after 1 iteration since check is at loop top)
        assert!(summary.iterations_completed <= 1);
    }
}
