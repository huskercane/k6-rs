use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::traits::{RunSummary, VirtualUser};

/// A fixed number of iterations shared across all VUs.
///
/// VUs grab iterations from a shared counter until the total is reached.
/// Faster VUs do more iterations. Ends when total iterations complete
/// or max_duration is reached.
pub struct SharedIterationsExecutor<V: VirtualUser + 'static> {
    vus: Vec<V>,
    total_iterations: u32,
    max_duration: Duration,
}

impl<V: VirtualUser + 'static> SharedIterationsExecutor<V> {
    pub fn new(vus: Vec<V>, total_iterations: u32, max_duration: Duration) -> Self {
        Self {
            vus,
            total_iterations,
            max_duration,
        }
    }

    pub async fn run(mut self, cancel: CancellationToken) -> Result<RunSummary> {
        let start = Instant::now();
        let deadline = start + self.max_duration;
        let completed = Arc::new(AtomicU64::new(0));
        let remaining = Arc::new(AtomicU32::new(self.total_iterations));

        let mut handles = Vec::with_capacity(self.vus.len());

        for mut vu in self.vus.drain(..) {
            let completed = Arc::clone(&completed);
            let remaining = Arc::clone(&remaining);
            let cancel = cancel.clone();

            let handle = tokio::task::spawn_blocking(move || {
                loop {
                    // Atomically claim an iteration via CAS loop
                    loop {
                        let current = remaining.load(Ordering::Relaxed);
                        if current == 0 {
                            return; // No iterations left
                        }
                        if remaining
                            .compare_exchange_weak(
                                current,
                                current - 1,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            )
                            .is_ok()
                        {
                            break; // Successfully claimed
                        }
                    }

                    if Instant::now() >= deadline || cancel.is_cancelled() {
                        break;
                    }

                    match vu.run_iteration() {
                        Ok(_) => {
                            completed.fetch_add(1, Ordering::Relaxed);
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
            iterations_completed: completed.load(Ordering::Relaxed),
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
    async fn exact_total_iterations() {
        let vus: Vec<MockVu> = (0..5).map(|_| MockVu).collect();
        // 5 VUs sharing 20 iterations
        let executor = SharedIterationsExecutor::new(vus, 20, Duration::from_secs(30));
        let summary = executor.run(CancellationToken::new()).await.unwrap();

        assert_eq!(summary.iterations_completed, 20);
    }

    #[tokio::test]
    async fn respects_max_duration() {
        let vus: Vec<MockVu> = (0..2).map(|_| MockVu).collect();
        let executor = SharedIterationsExecutor::new(vus, 10000, Duration::from_millis(50));
        let summary = executor.run(CancellationToken::new()).await.unwrap();

        assert!(summary.iterations_completed < 10000);
        assert!(summary.iterations_completed > 0);
    }
}
