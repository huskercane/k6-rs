use std::sync::Arc;

use crossbeam_queue::ArrayQueue;

/// A lock-free queue for borrow/return patterns across VUs.
///
/// k6-rs extension (not in k6). Perfect for auth token pools:
/// each VU borrows a unique user, uses it for an iteration, returns it.
/// Prevents two VUs from using the same item simultaneously.
///
/// Fixed capacity, no allocation after init.
#[derive(Clone)]
pub struct SharedQueue<T: Send> {
    queue: Arc<ArrayQueue<T>>,
    capacity: usize,
}

impl<T: Send> SharedQueue<T> {
    /// Create a new SharedQueue from the given items.
    ///
    /// Capacity is fixed to the number of items provided.
    pub fn new(items: Vec<T>) -> Self {
        let capacity = items.len();
        let queue = ArrayQueue::new(capacity.max(1));
        for item in items {
            let _ = queue.push(item);
        }
        Self {
            queue: Arc::new(queue),
            capacity,
        }
    }

    /// Take an item from the queue (non-blocking).
    ///
    /// Returns `None` if the queue is empty (all items are borrowed).
    pub fn take(&self) -> Option<T> {
        self.queue.pop()
    }

    /// Return an item to the queue.
    ///
    /// Returns `Err(item)` if the queue is full (shouldn't happen in normal use).
    pub fn put(&self, item: T) -> Result<(), T> {
        self.queue.push(item)
    }

    /// Number of items currently available.
    pub fn available(&self) -> usize {
        self.queue.len()
    }

    /// Total capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn take_and_put() {
        let q = SharedQueue::new(vec!["user1", "user2", "user3"]);

        assert_eq!(q.available(), 3);
        assert_eq!(q.capacity(), 3);

        let u1 = q.take().unwrap();
        assert_eq!(u1, "user1");
        assert_eq!(q.available(), 2);

        let u2 = q.take().unwrap();
        let u3 = q.take().unwrap();
        assert!(q.take().is_none()); // empty

        // Return them
        q.put(u3).unwrap();
        q.put(u2).unwrap();
        q.put(u1).unwrap();
        assert_eq!(q.available(), 3);
    }

    #[test]
    fn empty_queue() {
        let q = SharedQueue::<i32>::new(vec![]);
        assert!(q.is_empty());
        assert_eq!(q.capacity(), 0);
        assert!(q.take().is_none());
    }

    #[test]
    fn no_duplicates_under_contention() {
        // 10 tokens, 20 threads — each token should be used by at most one thread at a time
        let q = SharedQueue::new((0..10u32).collect::<Vec<_>>());
        let uses = Arc::new(AtomicU32::new(0));

        let mut handles = vec![];
        for _ in 0..20 {
            let q = q.clone();
            let uses = Arc::clone(&uses);
            handles.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    if let Some(token) = q.take() {
                        uses.fetch_add(1, Ordering::Relaxed);
                        // "Use" the token
                        std::thread::yield_now();
                        // Return it
                        q.put(token).unwrap();
                    }
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // All tokens should be back
        assert_eq!(q.available(), 10);
        // Some work should have been done
        assert!(uses.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn clone_shares_queue() {
        let q1 = SharedQueue::new(vec![42]);
        let q2 = q1.clone();

        let val = q1.take().unwrap();
        assert!(q2.take().is_none()); // same underlying queue

        q2.put(val).unwrap();
        assert_eq!(q1.available(), 1);
    }
}
