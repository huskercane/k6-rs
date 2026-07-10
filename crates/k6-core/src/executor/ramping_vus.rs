use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::config::Stage;
use crate::executor::vu_ramp::VuRampSchedule;
use crate::traits::{RunSummary, VirtualUser};
use crate::vu_pool::VuPool;

/// Variable number of VUs ramping through stages.
///
/// Unlike arrival-rate executors, each VU runs iterations sequentially.
/// VUs are added/removed from the active set according to stages.
/// Used by the orchard load test scenario-based tests.
pub struct RampingVusExecutor<V: VirtualUser + 'static> {
    pool: Arc<VuPool<V>>,
    stages: Vec<Stage>,
    start_vus: u32,
}

impl<V: VirtualUser + 'static> RampingVusExecutor<V> {
    pub fn new(pool: Arc<VuPool<V>>, stages: Vec<Stage>, start_vus: u32) -> Self {
        Self {
            pool,
            stages,
            start_vus,
        }
    }

    pub async fn run(&self, cancel: CancellationToken) -> Result<RunSummary> {
        let start = Instant::now();
        let iterations_completed = Arc::new(AtomicU64::new(0));

        // The active-VU-count curve lives in `VuRampSchedule` (shared, JS-free) —
        // the same schedule the coroutine ramping-VUs executor consumes.
        let schedule = VuRampSchedule::new(self.start_vus, &self.stages);
        let total_duration = schedule.total_duration();

        // Track active VU handles
        let mut active_guards = Vec::new();
        let mut active_cancel_tokens: Vec<CancellationToken> = Vec::new();

        // Control loop — adjust active VU count every 100ms
        let mut ticker = tokio::time::interval(Duration::from_millis(100));

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = cancel.cancelled() => break,
            }

            let elapsed = start.elapsed();
            if elapsed >= total_duration {
                break;
            }

            // Calculate desired VU count
            let desired = schedule.interpolate(elapsed);

            let current = active_guards.len() as u32;

            if desired > current {
                // Scale up — spawn more VUs
                for _ in current..desired {
                    if let Some(guard) = self.pool.try_acquire_owned() {
                        let vu_cancel = CancellationToken::new();
                        let completed = Arc::clone(&iterations_completed);
                        let vu_cancel_clone = vu_cancel.clone();
                        let global_cancel = cancel.clone();

                        let handle = tokio::task::spawn_blocking(move || {
                            let mut guard = guard;
                            loop {
                                if vu_cancel_clone.is_cancelled() || global_cancel.is_cancelled() {
                                    break;
                                }
                                match guard.vu_mut().run_iteration() {
                                    Ok(_) => {
                                        completed.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Err(e) => {
                                        eprintln!("VU iteration error: {e}");
                                    }
                                }
                            }
                            // guard dropped here → VU returned to pool
                        });

                        active_guards.push(handle);
                        active_cancel_tokens.push(vu_cancel);
                    }
                }
            } else if desired < current {
                // Scale down — cancel excess VUs
                let remove_count = (current - desired) as usize;
                for _ in 0..remove_count {
                    if let Some(token) = active_cancel_tokens.pop() {
                        token.cancel();
                    }
                    if let Some(handle) = active_guards.pop() {
                        let _ = handle.await;
                    }
                }
            }
        }

        // Cancel all remaining VUs
        for token in &active_cancel_tokens {
            token.cancel();
        }
        for handle in active_guards {
            let _ = handle.await;
        }

        Ok(RunSummary {
            iterations_completed: iterations_completed.load(Ordering::Relaxed),
            iterations_dropped: 0, // ramping-vus never drops
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
            std::thread::sleep(Duration::from_millis(20));
            Ok(IterationResult {
                duration: Duration::from_millis(20),
            })
        }
        fn reset(&mut self) {}
    }

    // VU-count interpolation is unit-tested in `executor::vu_ramp`; these tests
    // exercise the executor's use of the shared schedule.

    #[tokio::test]
    async fn ramp_up_and_down() {
        let vus: Vec<MockVu> = (0..10).map(|_| MockVu).collect();
        let pool = Arc::new(VuPool::new(vus));

        let executor = RampingVusExecutor::new(
            pool.clone(),
            vec![
                Stage {
                    duration: Duration::from_millis(200),
                    target: 5,
                },
                Stage {
                    duration: Duration::from_millis(200),
                    target: 5,
                },
                Stage {
                    duration: Duration::from_millis(200),
                    target: 0,
                },
            ],
            0,
        );

        let summary = executor.run(CancellationToken::new()).await.unwrap();

        assert!(summary.iterations_completed > 0);
        assert_eq!(summary.iterations_dropped, 0);
        // All VUs should be returned
        assert_eq!(pool.available_count(), 10);
    }

    #[tokio::test]
    async fn respects_cancellation() {
        let vus: Vec<MockVu> = (0..5).map(|_| MockVu).collect();
        let pool = Arc::new(VuPool::new(vus));

        let executor = RampingVusExecutor::new(
            pool.clone(),
            vec![Stage {
                duration: Duration::from_secs(60),
                target: 5,
            }],
            0,
        );

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel_clone.cancel();
        });

        let summary = executor.run(cancel).await.unwrap();
        assert!(summary.duration < Duration::from_secs(2));
        assert_eq!(pool.available_count(), 5);
    }
}
