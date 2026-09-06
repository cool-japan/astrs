//! [`Counter`] — a monotonically increasing metric handle.

use std::sync::atomic::{AtomicU64, Ordering};

/// A monotonically increasing count, backed by one [`AtomicU64`].
///
/// Every method is allocation-free and lock-free: this is the "record
/// path" the blueprint (§13) requires to stay allocation-free even under a
/// tight loop. Obtain a handle once (via
/// [`crate::metrics::MetricRegistry::register_counter`] or
/// [`crate::metrics::MetricFamily::get_or_create`]) and reuse it —
/// re-resolving a handle by name/labels on every call is the part that is
/// *not* free (see [`crate::metrics::interner`]).
///
/// # Examples
///
/// ```
/// use astrs_telemetry::metrics::Counter;
///
/// let counter = Counter::new();
/// counter.inc();
/// counter.inc_by(4);
/// assert_eq!(counter.get(), 5);
/// ```
#[derive(Debug, Default)]
pub struct Counter {
    value: AtomicU64,
}

impl Counter {
    /// A counter starting at zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            value: AtomicU64::new(0),
        }
    }

    /// Increments by one.
    pub fn inc(&self) {
        self.inc_by(1);
    }

    /// Increments by `delta`.
    ///
    /// Wraps on overflow (`u64::MAX` observations is not a realistic
    /// count within a process lifetime) rather than saturating, matching
    /// the wrapping semantics of every counter format this crate exports
    /// to (OTLP `sum` data points and Prometheus counters are both defined
    /// to wrap and rely on the reader detecting a decrease as a reset).
    pub fn inc_by(&self, delta: u64) {
        self.value.fetch_add(delta, Ordering::Relaxed);
    }

    /// The current value.
    #[must_use]
    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn starts_at_zero() {
        assert_eq!(Counter::new().get(), 0);
    }

    #[test]
    fn inc_and_inc_by_accumulate() {
        let counter = Counter::new();
        counter.inc();
        counter.inc();
        counter.inc_by(10);
        assert_eq!(counter.get(), 12);
    }

    #[test]
    fn wraps_on_overflow_rather_than_panicking() {
        let counter = Counter::new();
        counter.inc_by(u64::MAX);
        counter.inc();
        assert_eq!(counter.get(), 0);
    }

    #[test]
    fn concurrent_increments_all_land() {
        let counter = Arc::new(Counter::new());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let counter = Arc::clone(&counter);
                thread::spawn(move || {
                    for _ in 0..1_000 {
                        counter.inc();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(counter.get(), 8_000);
    }

    #[test]
    fn default_matches_new() {
        assert_eq!(Counter::default().get(), Counter::new().get());
    }
}
