use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::config::Stage;
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

        // Build timeline: (stage_end_time, from_vus, to_vus)
        let mut timeline = Vec::with_capacity(self.stages.len());
        let mut offset = Duration::ZERO;
        let mut prev_target = self.start_vus;

        for stage in &self.stages {
            let stage_end = offset + stage.duration;
            timeline.push((offset, stage_end, prev_target, stage.target));
            prev_target = stage.target;
            offset = stage_end;
        }

        let total_duration = offset;

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
            let desired = Self::interpolate_vus(&timeline, elapsed);

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
        })
    }

    fn interpolate_vus(timeline: &[(Duration, Duration, u32, u32)], elapsed: Duration) -> u32 {
        for &(stage_start, stage_end, from_vus, to_vus) in timeline {
            if elapsed >= stage_start && elapsed < stage_end {
                let stage_duration = (stage_end - stage_start).as_secs_f64();
                let stage_elapsed = (elapsed - stage_start).as_secs_f64();
                let progress = stage_elapsed / stage_duration;

                return (from_vus as f64 + (to_vus as f64 - from_vus as f64) * progress) as u32;
            }
        }
        0
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

    #[test]
    fn interpolate_vus_linear() {
        let timeline = vec![
            (Duration::ZERO, Duration::from_secs(10), 0, 10),
        ];

        assert_eq!(RampingVusExecutor::<MockVu>::interpolate_vus(&timeline, Duration::ZERO), 0);
        assert_eq!(RampingVusExecutor::<MockVu>::interpolate_vus(&timeline, Duration::from_secs(5)), 5);
        assert_eq!(RampingVusExecutor::<MockVu>::interpolate_vus(&timeline, Duration::from_secs(10)), 0); // past stage
    }

    #[tokio::test]
    async fn ramp_up_and_down() {
        let vus: Vec<MockVu> = (0..10).map(|_| MockVu).collect();
        let pool = Arc::new(VuPool::new(vus));

        let executor = RampingVusExecutor::new(
            pool.clone(),
            vec![
                Stage { duration: Duration::from_millis(200), target: 5 },
                Stage { duration: Duration::from_millis(200), target: 5 },
                Stage { duration: Duration::from_millis(200), target: 0 },
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
            vec![Stage { duration: Duration::from_secs(60), target: 5 }],
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
