use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// Controls the maximum number of concurrent in-flight HTTP requests.
///
/// This is the second layer of memory bounding (after VU pool).
/// Even with all VUs active, this limits concurrent TCP connections
/// and their associated buffers (socket buffers, TLS state, response body).
///
/// Default: `max_vus * 2` — allows slight pipelining while bounding memory.
#[derive(Clone)]
pub struct Backpressure {
    semaphore: Arc<Semaphore>,
    max_in_flight: usize,
    current_in_flight: Arc<AtomicU64>,
}

/// A permit for an in-flight request. Released on drop.
pub struct InFlightPermit {
    _permit: OwnedSemaphorePermit,
    current_in_flight: Arc<AtomicU64>,
}

impl Backpressure {
    /// Create a new backpressure controller.
    ///
    /// `max_in_flight`: maximum concurrent HTTP requests allowed.
    pub fn new(max_in_flight: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_in_flight)),
            max_in_flight,
            current_in_flight: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Create with default concurrency: `max_vus * 2`.
    pub fn from_vus(max_vus: usize) -> Self {
        Self::new(max_vus * 2)
    }

    /// Acquire a permit to send an HTTP request.
    ///
    /// Blocks (async) if the maximum number of in-flight requests is reached.
    /// The permit is released when dropped.
    pub async fn acquire(&self) -> InFlightPermit {
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore should not be closed");

        self.current_in_flight.fetch_add(1, Ordering::Relaxed);

        InFlightPermit {
            _permit: permit,
            current_in_flight: Arc::clone(&self.current_in_flight),
        }
    }

    /// Try to acquire a permit without blocking.
    ///
    /// Returns `None` if the limit is reached.
    pub fn try_acquire(&self) -> Option<InFlightPermit> {
        let permit = self.semaphore.clone().try_acquire_owned().ok()?;

        self.current_in_flight.fetch_add(1, Ordering::Relaxed);

        Some(InFlightPermit {
            _permit: permit,
            current_in_flight: Arc::clone(&self.current_in_flight),
        })
    }

    /// Current number of in-flight requests.
    pub fn in_flight(&self) -> u64 {
        self.current_in_flight.load(Ordering::Relaxed)
    }

    /// Maximum allowed concurrent in-flight requests.
    pub fn max_in_flight(&self) -> usize {
        self.max_in_flight
    }

    /// Number of permits currently available.
    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }
}

impl Drop for InFlightPermit {
    fn drop(&mut self) {
        self.current_in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Global rate limiter using a token bucket algorithm.
///
/// Limits the total number of HTTP requests per second across all VUs.
/// This implements k6's `rps` option.
#[derive(Clone)]
pub struct RateLimiter {
    /// None = unlimited
    inner: Option<Arc<RateLimiterInner>>,
}

struct RateLimiterInner {
    semaphore: Arc<Semaphore>,
}

impl RateLimiter {
    /// Create a rate limiter. `rps = 0` means unlimited.
    pub fn new(rps: u32) -> Self {
        if rps == 0 {
            return Self { inner: None };
        }
        let inner = RateLimiterInner {
            semaphore: Arc::new(Semaphore::new(rps as usize)),
        };
        Self {
            inner: Some(Arc::new(inner)),
        }
    }

    /// Returns true if rate limiting is active.
    pub fn is_active(&self) -> bool {
        self.inner.is_some()
    }

    /// Acquire a token to send a request. Blocks if rate limit exceeded.
    pub async fn acquire(&self) {
        if let Some(ref inner) = self.inner {
            let permit = inner
                .semaphore
                .clone()
                .acquire_owned()
                .await
                .expect("semaphore should not be closed");
            // Drop permit immediately — the replenish task adds permits back each second
            drop(permit);
        }
    }

    /// Start the token replenishment task. Must be called once before `acquire`.
    /// Adds `rps` tokens per second. Returns a handle to the background task.
    pub fn start_replenish(&self, cancel: CancellationToken) -> Option<tokio::task::JoinHandle<()>> {
        let inner = self.inner.as_ref()?;
        let sem = Arc::clone(&inner.semaphore);
        let max_permits = sem.available_permits() + 1; // initial capacity

        Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = interval.tick() => {
                        // Replenish to max
                        let available = sem.available_permits();
                        let to_add = max_permits.saturating_sub(available);
                        if to_add > 0 {
                            sem.add_permits(to_add);
                        }
                    }
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn acquire_and_release() {
        let bp = Backpressure::new(3);

        assert_eq!(bp.in_flight(), 0);
        assert_eq!(bp.available_permits(), 3);

        let p1 = bp.acquire().await;
        assert_eq!(bp.in_flight(), 1);
        assert_eq!(bp.available_permits(), 2);

        let p2 = bp.acquire().await;
        assert_eq!(bp.in_flight(), 2);

        drop(p1);
        assert_eq!(bp.in_flight(), 1);
        assert_eq!(bp.available_permits(), 2);

        drop(p2);
        assert_eq!(bp.in_flight(), 0);
        assert_eq!(bp.available_permits(), 3);
    }

    #[tokio::test]
    async fn blocks_at_limit() {
        let bp = Backpressure::new(2);

        let _p1 = bp.acquire().await;
        let _p2 = bp.acquire().await;

        // Pool is full — try_acquire should return None
        assert!(bp.try_acquire().is_none());
        assert_eq!(bp.in_flight(), 2);

        // Async acquire should block — verify with a timeout
        let bp_clone = bp.clone();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            bp_clone.acquire(),
        )
        .await;

        assert!(result.is_err(), "acquire should have timed out");
    }

    #[tokio::test]
    async fn unblocks_on_release() {
        let bp = Backpressure::new(1);

        let p1 = bp.acquire().await;
        assert_eq!(bp.in_flight(), 1);

        // Spawn a task that waits for a permit
        let bp_clone = bp.clone();
        let handle = tokio::spawn(async move {
            let _p = bp_clone.acquire().await;
            assert_eq!(bp_clone.in_flight(), 1);
        });

        // Release the first permit — should unblock the spawned task
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        drop(p1);

        handle.await.unwrap();
        assert_eq!(bp.in_flight(), 0);
    }

    #[tokio::test]
    async fn from_vus_default() {
        let bp = Backpressure::from_vus(50);
        assert_eq!(bp.max_in_flight(), 100);
        assert_eq!(bp.available_permits(), 100);
    }

    #[tokio::test]
    async fn concurrent_acquire_release() {
        let bp = Backpressure::new(10);
        let mut handles = vec![];

        for _ in 0..50 {
            let bp = bp.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..20 {
                    let _permit = bp.acquire().await;
                    // Simulate some work
                    tokio::time::sleep(std::time::Duration::from_micros(100)).await;
                    // permit dropped here
                }
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        // All permits should be returned
        assert_eq!(bp.in_flight(), 0);
        assert_eq!(bp.available_permits(), 10);
    }

    #[tokio::test]
    async fn never_exceeds_max_in_flight() {
        use std::sync::atomic::AtomicU64;

        let bp = Backpressure::new(5);
        let max_seen = Arc::new(AtomicU64::new(0));

        let mut handles = vec![];
        for _ in 0..20 {
            let bp = bp.clone();
            let max_seen = Arc::clone(&max_seen);
            handles.push(tokio::spawn(async move {
                for _ in 0..50 {
                    let _permit = bp.acquire().await;
                    let current = bp.in_flight();
                    max_seen.fetch_max(current, Ordering::Relaxed);
                    assert!(current <= 5, "in_flight exceeded max: {current}");
                    tokio::task::yield_now().await;
                }
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let max = max_seen.load(Ordering::Relaxed);
        assert!(max <= 5, "max in_flight seen was {max}, expected <= 5");
    }

    #[test]
    fn rate_limiter_unlimited() {
        let rl = RateLimiter::new(0);
        assert!(!rl.is_active());
    }

    #[test]
    fn rate_limiter_active() {
        let rl = RateLimiter::new(100);
        assert!(rl.is_active());
    }

    #[tokio::test]
    async fn rate_limiter_acquire_within_budget() {
        let rl = RateLimiter::new(10);
        let cancel = CancellationToken::new();
        let _handle = rl.start_replenish(cancel.clone());

        // Should be able to acquire 10 tokens immediately
        for _ in 0..10 {
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                rl.acquire(),
            )
            .await
            .expect("should acquire within budget");
        }

        cancel.cancel();
    }
}
