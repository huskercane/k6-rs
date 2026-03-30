use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crossbeam_queue::ArrayQueue;

use crate::traits::VirtualUser;

/// A fixed-size pool of pre-allocated virtual users.
///
/// VUs are borrowed for a single iteration and returned when done.
/// If the pool is exhausted (all VUs busy), iterations are dropped
/// rather than allocating new VUs — this guarantees bounded memory.
pub struct VuPool<V: VirtualUser> {
    /// All VUs, indexed by VuId. UnsafeCell for interior mutability —
    /// safety is guaranteed by the ArrayQueue which ensures exclusive access.
    vus: Vec<UnsafeCell<Option<V>>>,
    /// Queue of available VU indices.
    available: Arc<ArrayQueue<usize>>,
    /// Count of iterations dropped because pool was exhausted.
    dropped_iterations: AtomicU64,
    /// Total capacity.
    capacity: usize,
}

/// A borrowed VU that is automatically returned to the pool on drop.
pub struct VuGuard<'pool, V: VirtualUser> {
    vu: Option<V>,
    id: usize,
    pool: &'pool VuPool<V>,
}

/// An owned VU guard that holds an `Arc<VuPool>` — can be sent to `spawn_blocking`.
pub struct OwnedVuGuard<V: VirtualUser> {
    vu: Option<V>,
    id: usize,
    pool: Arc<VuPool<V>>,
}

impl<V: VirtualUser> VuPool<V> {
    /// Create a new pool from a pre-built vector of VUs.
    ///
    /// All VUs are immediately available for borrowing.
    pub fn new(vus: Vec<V>) -> Self {
        let capacity = vus.len();
        let available = Arc::new(ArrayQueue::new(capacity));

        let mut vu_slots: Vec<UnsafeCell<Option<V>>> = Vec::with_capacity(capacity);
        for (i, vu) in vus.into_iter().enumerate() {
            vu_slots.push(UnsafeCell::new(Some(vu)));
            // Safe: queue capacity == number of VUs
            let _ = available.push(i);
        }

        Self {
            vus: vu_slots,
            available,
            dropped_iterations: AtomicU64::new(0),
            capacity,
        }
    }

    /// Try to borrow a VU without blocking.
    ///
    /// Returns `Some(VuGuard)` if a VU is available, `None` if pool is exhausted.
    /// When `None` is returned, `dropped_iterations` is NOT incremented —
    /// the caller decides whether to count it as dropped.
    pub fn try_acquire(&self) -> Option<VuGuard<'_, V>> {
        let id = self.available.pop()?;
        // Safety: we only hand out each index once (popped from queue),
        // and the slot is always Some when the index is in the queue.
        let vu = unsafe { (*self.vus[id].get()).take() };
        Some(VuGuard {
            vu,
            id,
            pool: self,
        })
    }

    /// Record a dropped iteration (pool was exhausted).
    pub fn record_dropped(&self) {
        self.dropped_iterations.fetch_add(1, Ordering::Relaxed);
    }

    /// Number of iterations dropped due to pool exhaustion.
    pub fn dropped_iterations(&self) -> u64 {
        self.dropped_iterations.load(Ordering::Relaxed)
    }

    /// Total pool capacity (number of VUs allocated).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of VUs currently available (not borrowed).
    pub fn available_count(&self) -> usize {
        self.available.len()
    }

    /// Try to borrow a VU with an owned guard (for `spawn_blocking`).
    ///
    /// The returned `OwnedVuGuard` holds an `Arc<VuPool>` and is `Send + 'static`.
    pub fn try_acquire_owned(self: &Arc<Self>) -> Option<OwnedVuGuard<V>> {
        let id = self.available.pop()?;
        let vu = unsafe { (*self.vus[id].get()).take() };
        Some(OwnedVuGuard {
            vu,
            id,
            pool: Arc::clone(self),
        })
    }

    fn return_vu(&self, id: usize, mut vu: V) {
        vu.reset();
        // Safety: we only access vus[id] when we hold exclusive ownership
        // of this index (popped from queue, not yet pushed back).
        unsafe {
            *self.vus[id].get() = Some(vu);
        }
        let _ = self.available.push(id);
    }
}

// Safety: VuPool is safe to share across threads because:
// - Each VU index is only handed out once (via ArrayQueue pop)
// - The mutable access to vus[id] only happens when we hold the index exclusively
// - AtomicU64 is inherently thread-safe
unsafe impl<V: VirtualUser> Sync for VuPool<V> {}

impl<V: VirtualUser> VuGuard<'_, V> {
    /// Get a mutable reference to the borrowed VU.
    pub fn vu_mut(&mut self) -> &mut V {
        self.vu.as_mut().expect("VU already returned")
    }
}

impl<V: VirtualUser> Drop for VuGuard<'_, V> {
    fn drop(&mut self) {
        if let Some(vu) = self.vu.take() {
            self.pool.return_vu(self.id, vu);
        }
    }
}

impl<V: VirtualUser> OwnedVuGuard<V> {
    /// Get a mutable reference to the borrowed VU.
    pub fn vu_mut(&mut self) -> &mut V {
        self.vu.as_mut().expect("VU already returned")
    }
}

impl<V: VirtualUser> Drop for OwnedVuGuard<V> {
    fn drop(&mut self) {
        if let Some(vu) = self.vu.take() {
            self.pool.return_vu(self.id, vu);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::IterationResult;
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    /// A mock VU for testing.
    struct MockVu {
        iterations_run: u32,
        resets: u32,
    }

    impl MockVu {
        fn new(_id: u32) -> Self {
            Self {
                iterations_run: 0,
                resets: 0,
            }
        }
    }

    impl VirtualUser for MockVu {
        fn run_iteration(&mut self) -> anyhow::Result<IterationResult> {
            self.iterations_run += 1;
            Ok(IterationResult {
                duration: Duration::from_millis(10),
            })
        }

        fn reset(&mut self) {
            self.resets += 1;
        }
    }

    #[test]
    fn borrow_and_return() {
        let vus = vec![MockVu::new(0), MockVu::new(1), MockVu::new(2)];
        let pool = VuPool::new(vus);

        assert_eq!(pool.capacity(), 3);
        assert_eq!(pool.available_count(), 3);

        // Borrow one
        let mut guard = pool.try_acquire().expect("should get a VU");
        assert_eq!(pool.available_count(), 2);

        // Run an iteration
        let result = guard.vu_mut().run_iteration().unwrap();
        assert_eq!(result.duration, Duration::from_millis(10));

        // Return it
        drop(guard);
        assert_eq!(pool.available_count(), 3);
    }

    #[test]
    fn exhaustion_returns_none() {
        let vus = vec![MockVu::new(0), MockVu::new(1)];
        let pool = VuPool::new(vus);

        let _g1 = pool.try_acquire().expect("VU 1");
        let _g2 = pool.try_acquire().expect("VU 2");

        // Pool exhausted
        assert!(pool.try_acquire().is_none());
        assert_eq!(pool.available_count(), 0);
    }

    #[test]
    fn dropped_iterations_counter() {
        let vus = vec![MockVu::new(0)];
        let pool = VuPool::new(vus);

        assert_eq!(pool.dropped_iterations(), 0);

        pool.record_dropped();
        pool.record_dropped();
        pool.record_dropped();

        assert_eq!(pool.dropped_iterations(), 3);
    }

    #[test]
    fn vu_reset_called_on_return() {
        let vus = vec![MockVu::new(0)];
        let pool = VuPool::new(vus);

        // Borrow, run, return
        {
            let mut guard = pool.try_acquire().unwrap();
            guard.vu_mut().run_iteration().unwrap();
        }

        // Borrow again — should have been reset
        {
            let mut guard = pool.try_acquire().unwrap();
            assert_eq!(guard.vu_mut().resets, 1);
        }
    }

    #[test]
    fn concurrent_borrow_return() {
        // Use 2 VUs and 20 threads, each holding the VU for a bit,
        // to guarantee contention and drops.
        let vus: Vec<MockVu> = (0..2).map(MockVu::new).collect();
        let pool = Arc::new(VuPool::new(vus));
        let total_iterations = Arc::new(AtomicU32::new(0));

        let mut handles = vec![];
        for _ in 0..20 {
            let pool = Arc::clone(&pool);
            let count = Arc::clone(&total_iterations);
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    if let Some(mut guard) = pool.try_acquire() {
                        guard.vu_mut().run_iteration().unwrap();
                        count.fetch_add(1, Ordering::Relaxed);
                        // Hold the VU briefly to create contention
                        std::thread::sleep(std::time::Duration::from_micros(10));
                    } else {
                        pool.record_dropped();
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let completed = total_iterations.load(Ordering::Relaxed) as u64;
        let dropped = pool.dropped_iterations();

        // Total attempts = 20 threads * 100 iterations = 2000
        assert_eq!(completed + dropped, 2000);
        // With 2 VUs and 20 threads holding VUs for 10us, drops are guaranteed
        assert!(dropped > 0, "expected some dropped iterations");
        // All VUs should be back in the pool
        assert_eq!(pool.available_count(), 2);
    }
}
