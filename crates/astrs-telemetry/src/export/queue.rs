//! [`BoundedQueue`] — a fixed-capacity FIFO with an overflow counter.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// A bounded FIFO queue that never grows past `capacity`: pushing onto a
/// full queue drops the **oldest** entry and counts the drop, rather than
/// growing without bound or rejecting the new entry.
///
/// Blueprint §13: "bounded queue with drop counter" is one of the OTLP
/// exporter's explicit requirements. Dropping the oldest entry (not the
/// newest) is a deliberate choice for telemetry data specifically: if a
/// collector is unreachable long enough to fill the queue, the most
/// useful thing this process can still send once connectivity returns is
/// its *most recent* state, not a backlog that is already stale.
///
/// # Examples
///
/// ```
/// use astrs_telemetry::export::BoundedQueue;
///
/// let queue = BoundedQueue::new(2);
/// queue.push(1);
/// queue.push(2);
/// queue.push(3); // queue is full: drops `1`, counts it.
/// assert_eq!(queue.dropped_total(), 1);
/// assert_eq!(queue.drain_up_to(10), vec![2, 3]);
/// ```
#[derive(Debug)]
pub struct BoundedQueue<T> {
    capacity: usize,
    inner: Mutex<VecDeque<T>>,
    dropped_total: AtomicU64,
}

impl<T> BoundedQueue<T> {
    /// Builds an empty queue holding at most `capacity` items (`0` is
    /// legal: every push is immediately dropped and counted).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            inner: Mutex::new(VecDeque::with_capacity(capacity.min(1024))),
            dropped_total: AtomicU64::new(0),
        }
    }

    /// The configured capacity.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Pushes `item`, dropping and counting the oldest entry first if the
    /// queue is already at capacity.
    pub fn push(&self, item: T) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while inner.len() >= self.capacity {
            if inner.pop_front().is_none() {
                // `capacity == 0`: nothing to evict, the new item itself
                // is the one being dropped (handled below).
                break;
            }
            self.dropped_total.fetch_add(1, Ordering::Relaxed);
        }
        if self.capacity == 0 {
            self.dropped_total.fetch_add(1, Ordering::Relaxed);
            return;
        }
        inner.push_back(item);
    }

    /// Removes and returns up to `max` items, oldest first.
    #[must_use]
    pub fn drain_up_to(&self, max: usize) -> Vec<T> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let take = max.min(inner.len());
        inner.drain(..take).collect()
    }

    /// Removes and returns every queued item.
    #[must_use]
    pub fn drain_all(&self) -> Vec<T> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.drain(..).collect()
    }

    /// The number of items currently queued.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Whether the queue currently holds no items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many items have been dropped (evicted for capacity, or
    /// refused outright by a zero-capacity queue) since construction.
    #[must_use]
    pub fn dropped_total(&self) -> u64 {
        self.dropped_total.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn push_and_drain_preserve_fifo_order() {
        let queue = BoundedQueue::new(4);
        queue.push(1);
        queue.push(2);
        queue.push(3);
        assert_eq!(queue.drain_up_to(10), vec![1, 2, 3]);
        assert!(queue.is_empty());
    }

    #[test]
    fn overflow_drops_the_oldest_entry_and_counts_it() {
        let queue = BoundedQueue::new(2);
        queue.push(1);
        queue.push(2);
        queue.push(3);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.dropped_total(), 1);
        assert_eq!(queue.drain_up_to(10), vec![2, 3]);
    }

    #[test]
    fn drain_up_to_takes_at_most_the_requested_amount() {
        let queue = BoundedQueue::new(10);
        for i in 0..5 {
            queue.push(i);
        }
        assert_eq!(queue.drain_up_to(2), vec![0, 1]);
        assert_eq!(queue.drain_up_to(10), vec![2, 3, 4]);
    }

    #[test]
    fn zero_capacity_drops_everything() {
        let queue: BoundedQueue<i32> = BoundedQueue::new(0);
        queue.push(1);
        queue.push(2);
        assert!(queue.is_empty());
        assert_eq!(queue.dropped_total(), 2);
    }

    #[test]
    fn drain_all_empties_the_queue() {
        let queue = BoundedQueue::new(4);
        queue.push(1);
        queue.push(2);
        assert_eq!(queue.drain_all(), vec![1, 2]);
        assert!(queue.is_empty());
        assert_eq!(queue.drain_all(), Vec::<i32>::new());
    }

    #[test]
    fn concurrent_pushes_never_exceed_capacity() {
        use std::sync::Arc;
        use std::thread;

        let queue = Arc::new(BoundedQueue::new(16));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let queue = Arc::clone(&queue);
                thread::spawn(move || {
                    for i in 0..100 {
                        queue.push(i);
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert!(queue.len() <= 16);
        assert_eq!(queue.len() as u64 + queue.dropped_total(), 800);
    }
}
