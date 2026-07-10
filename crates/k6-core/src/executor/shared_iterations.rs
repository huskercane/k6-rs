use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
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

                    // Count ATTEMPTED iterations, not just successful ones.
                    // Upstream's `dropped = totalIters - attemptedIters` treats
                    // an iteration whose script threw as attempted (it ran and
                    // still counts toward `iterations`); only iterations that
                    // never started are dropped. Incrementing on both arms keeps
                    // `completed + dropped == total` and matches that semantic.
                    if let Err(e) = vu.run_iteration() {
                        eprintln!("VU iteration error: {e}");
                    }
                    completed.fetch_add(1, Ordering::Relaxed);
                    vu.reset();
                }
            });

            handles.push(handle);
        }

        for handle in handles {
            let _ = handle.await;
        }

        let iterations_completed = completed.load(Ordering::Relaxed);

        Ok(RunSummary {
            iterations_completed,
            iterations_dropped: (self.total_iterations as u64).saturating_sub(iterations_completed),
            duration: start.elapsed(),
            ..Default::default()
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
            Ok(IterationResult {
                duration: Duration::from_millis(5),
            })
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
        assert_eq!(
            summary.iterations_completed + summary.iterations_dropped,
            10000
        );
        assert!(
            summary.iterations_dropped > 0,
            "max_duration should report unstarted shared iterations as dropped"
        );
    }

    #[tokio::test]
    async fn zero_max_duration_drops_all_iterations() {
        // Port of upstream TestSharedIterationsEmitDroppedIterations at the
        // summary boundary: if maxDuration prevents any work from starting,
        // every planned shared iteration is reported as dropped.
        let vus: Vec<MockVu> = (0..5).map(|_| MockVu).collect();
        let executor = SharedIterationsExecutor::new(vus, 100, Duration::ZERO);
        let summary = executor.run(CancellationToken::new()).await.unwrap();

        assert_eq!(summary.iterations_completed, 0);
        assert_eq!(summary.iterations_dropped, 100);
    }

    struct AlwaysErrsVu;
    impl VirtualUser for AlwaysErrsVu {
        fn run_iteration(&mut self) -> Result<IterationResult> {
            anyhow::bail!("intentional iteration error")
        }
        fn reset(&mut self) {}
    }

    #[tokio::test]
    async fn errored_iterations_count_as_attempted_not_dropped() {
        // Upstream counts a thrown iteration as attempted (dropped = total -
        // attempted), so a run where every iteration errors but all start
        // must report ZERO dropped, not `total` dropped. Locks the fix that
        // an errored iteration is not misclassified as never-started.
        let vus: Vec<AlwaysErrsVu> = (0..4).map(|_| AlwaysErrsVu).collect();
        let executor = SharedIterationsExecutor::new(vus, 20, Duration::from_secs(30));
        let summary = executor.run(CancellationToken::new()).await.unwrap();

        assert_eq!(summary.iterations_completed, 20, "all 20 were attempted");
        assert_eq!(
            summary.iterations_dropped, 0,
            "errored-but-started iterations are not dropped"
        );
    }
}
