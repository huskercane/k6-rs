use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::traits::{RunSummary, VirtualUser};

/// Each VU runs a fixed number of iterations.
///
/// Total iterations = vus × iterations_per_vu.
/// Ends when all VUs finish their iterations or max_duration is reached.
pub struct PerVuIterationsExecutor<V: VirtualUser + 'static> {
    vus: Vec<V>,
    iterations_per_vu: u32,
    max_duration: Duration,
}

impl<V: VirtualUser + 'static> PerVuIterationsExecutor<V> {
    pub fn new(vus: Vec<V>, iterations_per_vu: u32, max_duration: Duration) -> Self {
        Self {
            vus,
            iterations_per_vu,
            max_duration,
        }
    }

    pub async fn run(mut self, cancel: CancellationToken) -> Result<RunSummary> {
        let start = Instant::now();
        let deadline = start + self.max_duration;
        let total_iterations = Arc::new(AtomicU64::new(0));

        let mut handles = Vec::with_capacity(self.vus.len());

        for mut vu in self.vus.drain(..) {
            let iterations = Arc::clone(&total_iterations);
            let cancel = cancel.clone();
            let iters = self.iterations_per_vu;

            let handle = tokio::task::spawn_blocking(move || {
                for _ in 0..iters {
                    if Instant::now() >= deadline || cancel.is_cancelled() {
                        break;
                    }
                    match vu.run_iteration() {
                        Ok(_) => {
                            iterations.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => eprintln!("VU iteration error: {e}"),
                    }
                    vu.reset();
                }
            });

            handles.push(handle);
        }

        for handle in handles {
            let _ = handle.await;
        }

        Ok(RunSummary {
            iterations_completed: total_iterations.load(Ordering::Relaxed),
            iterations_dropped: 0,
            duration: start.elapsed(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::IterationResult;

    struct MockVu;

    impl VirtualUser for MockVu {
        fn run_iteration(&mut self) -> Result<IterationResult> {
            std::thread::sleep(Duration::from_millis(5));
            Ok(IterationResult { duration: Duration::from_millis(5) })
        }
        fn reset(&mut self) {}
    }

    #[tokio::test]
    async fn exact_iteration_count() {
        let vus: Vec<MockVu> = (0..3).map(|_| MockVu).collect();
        let executor = PerVuIterationsExecutor::new(vus, 10, Duration::from_secs(30));
        let summary = executor.run(CancellationToken::new()).await.unwrap();

        // 3 VUs × 10 iterations = 30 total
        assert_eq!(summary.iterations_completed, 30);
    }

    #[tokio::test]
    async fn respects_max_duration() {
        let vus: Vec<MockVu> = (0..2).map(|_| MockVu).collect();
        // 1000 iterations but only 50ms max — should stop early
        let executor = PerVuIterationsExecutor::new(vus, 1000, Duration::from_millis(50));
        let summary = executor.run(CancellationToken::new()).await.unwrap();

        assert!(summary.iterations_completed < 1000);
        assert!(summary.iterations_completed > 0);
    }
}
