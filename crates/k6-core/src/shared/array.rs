use std::sync::Arc;

/// A read-only array shared across all VUs.
///
/// k6-compatible: allocated once in init context, shared via `Arc`.
/// Read-only by design — no locks, no synchronization, no contention
/// at any VU count. This is why k6 makes SharedArray immutable.
#[derive(Clone)]
pub struct SharedArray<T: Send + Sync> {
    data: Arc<Vec<T>>,
    name: Arc<str>,
}

impl<T: Send + Sync> SharedArray<T> {
    /// Create a new SharedArray from the given data.
    ///
    /// The data is moved into an `Arc` — all clones share the same allocation.
    pub fn new(name: impl Into<String>, data: Vec<T>) -> Self {
        Self {
            data: Arc::new(data),
            name: Arc::from(name.into()),
        }
    }

    /// Get element by index.
    pub fn get(&self, index: usize) -> Option<&T> {
        self.data.get(index)
    }

    /// Number of elements.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the array is empty.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// The name of this SharedArray (for debugging/display).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Iterate over all elements.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.data.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn basic_access() {
        let arr = SharedArray::new("users", vec!["alice", "bob", "charlie"]);

        assert_eq!(arr.len(), 3);
        assert!(!arr.is_empty());
        assert_eq!(arr.get(0), Some(&"alice"));
        assert_eq!(arr.get(2), Some(&"charlie"));
        assert_eq!(arr.get(3), None);
        assert_eq!(arr.name(), "users");
    }

    #[test]
    fn clone_shares_data() {
        let arr = SharedArray::new("data", vec![1, 2, 3]);
        let arr2 = arr.clone();

        // Same underlying allocation
        assert!(Arc::ptr_eq(&arr.data, &arr2.data));
        assert_eq!(arr2.get(1), Some(&2));
    }

    #[test]
    fn concurrent_reads() {
        let arr = SharedArray::new("tokens", (0..1000).collect::<Vec<_>>());
        let sum = Arc::new(AtomicUsize::new(0));

        let mut handles = vec![];
        for _ in 0..20 {
            let arr = arr.clone();
            let sum = Arc::clone(&sum);
            handles.push(std::thread::spawn(move || {
                let mut local_sum = 0usize;
                for i in 0..arr.len() {
                    local_sum += arr.get(i).unwrap();
                }
                sum.fetch_add(local_sum, Ordering::Relaxed);
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // Sum of 0..1000 = 499500, times 20 threads
        let expected = 499500usize * 20;
        assert_eq!(sum.load(Ordering::Relaxed), expected);
    }

    #[test]
    fn empty_array() {
        let arr = SharedArray::<String>::new("empty", vec![]);
        assert!(arr.is_empty());
        assert_eq!(arr.len(), 0);
        assert_eq!(arr.get(0), None);
    }

    #[test]
    fn iterate() {
        let arr = SharedArray::new("nums", vec![10, 20, 30]);
        let collected: Vec<_> = arr.iter().copied().collect();
        assert_eq!(collected, vec![10, 20, 30]);
    }
}
