use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// An atomic counter shared across all VUs.
///
/// k6-rs extension (not in k6). Use cases:
/// - Unique request IDs
/// - Sequential user assignment
/// - Iteration counting across VUs
///
/// Zero contention — uses atomic fetch_add.
#[derive(Clone)]
pub struct SharedCounter {
    value: Arc<AtomicU64>,
}

impl SharedCounter {
    /// Create a new counter starting at 0.
    pub fn new() -> Self {
        Self {
            value: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Create a new counter starting at the given value.
    pub fn with_initial(initial: u64) -> Self {
        Self {
            value: Arc::new(AtomicU64::new(initial)),
        }
    }

    /// Get the next value and increment the counter.
    ///
    /// Returns the value before incrementing (like postfix `i++`).
    pub fn next(&self) -> u64 {
        self.value.fetch_add(1, Ordering::Relaxed)
    }

    /// Get the current value without incrementing.
    pub fn current(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }

    /// Reset the counter to 0.
    pub fn reset(&self) {
        self.value.store(0, Ordering::Relaxed);
    }
}

impl Default for SharedCounter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequential_increment() {
        let c = SharedCounter::new();

        assert_eq!(c.next(), 0);
        assert_eq!(c.next(), 1);
        assert_eq!(c.next(), 2);
        assert_eq!(c.current(), 3);
    }

    #[test]
    fn with_initial_value() {
        let c = SharedCounter::with_initial(100);
        assert_eq!(c.next(), 100);
        assert_eq!(c.next(), 101);
    }

    #[test]
    fn reset_counter() {
        let c = SharedCounter::new();
        c.next();
        c.next();
        c.reset();
        assert_eq!(c.current(), 0);
        assert_eq!(c.next(), 0);
    }

    #[test]
    fn concurrent_no_duplicates() {
        let c = SharedCounter::new();
        let mut handles = vec![];

        for _ in 0..20 {
            let c = c.clone();
            handles.push(std::thread::spawn(move || {
                let mut values = Vec::with_capacity(100);
                for _ in 0..100 {
                    values.push(c.next());
                }
                values
            }));
        }

        let mut all_values = vec![];
        for h in handles {
            all_values.extend(h.join().unwrap());
        }

        // 20 threads * 100 increments = 2000 unique values
        assert_eq!(all_values.len(), 2000);
        all_values.sort();
        all_values.dedup();
        assert_eq!(all_values.len(), 2000, "all values should be unique");
        assert_eq!(c.current(), 2000);
    }

    #[test]
    fn clone_shares_state() {
        let c1 = SharedCounter::new();
        let c2 = c1.clone();

        c1.next();
        assert_eq!(c2.next(), 1); // shares same atomic
    }
}
