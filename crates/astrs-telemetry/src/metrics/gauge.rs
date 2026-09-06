//! [`Gauge`] — an instantaneous, up-or-down metric handle.

use std::sync::atomic::{AtomicU64, Ordering};

/// A point-in-time reading that may increase or decrease, backed by one
/// [`AtomicU64`] holding an [`f64`]'s bit pattern.
///
/// Stable Rust has no `AtomicF64`; storing `f64::to_bits()` in an
/// `AtomicU64` and reading back via `f64::from_bits()` is the standard
/// allocation-free, lock-free way to get atomic float storage. `set` is a
/// single atomic store; `add`/`sub` need a compare-and-swap retry loop
/// (there is no atomic float add), but the loop touches only a register
/// and never allocates or blocks, so it stays on the record path the
/// blueprint (§13) requires to be allocation-free.
///
/// # Examples
///
/// ```
/// use astrs_telemetry::metrics::Gauge;
///
/// let gauge = Gauge::new();
/// gauge.set(4.0);
/// gauge.add(1.5);
/// assert_eq!(gauge.get(), 5.5);
/// gauge.sub(0.5);
/// assert_eq!(gauge.get(), 5.0);
/// ```
#[derive(Debug)]
pub struct Gauge {
    bits: AtomicU64,
}

impl Default for Gauge {
    fn default() -> Self {
        Self::new()
    }
}

impl Gauge {
    /// A gauge starting at `0.0`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_value(0.0)
    }

    /// A gauge starting at `value`.
    #[must_use]
    pub fn with_value(value: f64) -> Self {
        Self {
            bits: AtomicU64::new(value.to_bits()),
        }
    }

    /// Overwrites the current value.
    pub fn set(&self, value: f64) {
        self.bits.store(value.to_bits(), Ordering::Relaxed);
    }

    /// Adds `delta` to the current value (a negative `delta` decreases it).
    ///
    /// Implemented as a compare-and-swap retry loop; under contention this
    /// may retry several times, but it never allocates, blocks on a lock,
    /// or gives up (the loop is unconditional, matching
    /// [`std::sync::atomic::AtomicU64::fetch_update`]'s own contract).
    pub fn add(&self, delta: f64) {
        let _ = self
            .bits
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                Some((f64::from_bits(bits) + delta).to_bits())
            });
    }

    /// Subtracts `delta` from the current value. Equivalent to
    /// `self.add(-delta)`.
    pub fn sub(&self, delta: f64) {
        self.add(-delta);
    }

    /// Increments by `1.0`.
    pub fn inc(&self) {
        self.add(1.0);
    }

    /// Decrements by `1.0`.
    pub fn dec(&self) {
        self.add(-1.0);
    }

    /// The current value.
    #[must_use]
    pub fn get(&self) -> f64 {
        f64::from_bits(self.bits.load(Ordering::Relaxed))
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
        assert_eq!(Gauge::new().get(), 0.0);
    }

    #[test]
    fn set_overwrites() {
        let gauge = Gauge::new();
        gauge.set(3.5);
        assert_eq!(gauge.get(), 3.5);
        gauge.set(-1.0);
        assert_eq!(gauge.get(), -1.0);
    }

    #[test]
    fn add_and_sub_and_inc_and_dec() {
        let gauge = Gauge::with_value(10.0);
        gauge.add(2.5);
        assert_eq!(gauge.get(), 12.5);
        gauge.sub(0.5);
        assert_eq!(gauge.get(), 12.0);
        gauge.inc();
        assert_eq!(gauge.get(), 13.0);
        gauge.dec();
        assert_eq!(gauge.get(), 12.0);
    }

    #[test]
    fn survives_a_nan_value() {
        let gauge = Gauge::with_value(f64::NAN);
        assert!(gauge.get().is_nan());
    }

    #[test]
    fn concurrent_adds_all_land() {
        let gauge = Arc::new(Gauge::new());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let gauge = Arc::clone(&gauge);
                thread::spawn(move || {
                    for _ in 0..1_000 {
                        gauge.add(1.0);
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(gauge.get(), 8_000.0);
    }
}
