//! [`LabelInterner`] — a bounded map from a label-value tuple to a small
//! series index, with an explicit capacity and an overflow counter.
//!
//! Blueprint §13 asks for metric label sets that "reject unbounded
//! cardinality by design": a label *name* (e.g. `"node"`) is a fixed
//! `&'static str` chosen once when a [`crate::metrics::MetricFamily`] is
//! registered, but a label *value* (e.g. a node id) is caller-supplied and
//! therefore unbounded in principle — a bug or a hostile input could mint
//! one series per distinct string forever. `LabelInterner` is the
//! mechanism that stops that: it hands out at most [`LabelInterner::capacity`]
//! distinct series indices, and every combination beyond that shares one
//! `overflow` slot, tracked by [`LabelInterner::overflow_total`].
//!
//! This is a **registration-time** structure, not the allocation-free hot
//! path: [`LabelInterner::intern`] takes a lock and may allocate (to store
//! a newly-seen combination, or as a throwaway lookup key even on a
//! cache hit). Callers are expected to intern once per distinct label
//! combination and cache the resulting handle — see
//! [`crate::metrics::MetricFamily::get_or_create`] — after which the
//! returned metric handle's own `.inc()`/`.set()`/`.observe()` is the part
//! that must not allocate.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The default cap on distinct label-value combinations per
/// [`crate::metrics::MetricFamily`], chosen to comfortably cover a
/// dataflow with hundreds of nodes/ports while still bounding a
/// misbehaving label to a fixed, small footprint.
pub const DEFAULT_MAX_SERIES: u32 = 256;

/// A bounded interner from a label-value tuple (e.g. `["camera",
/// "frames"]`) to a stable `u32` series index in `0..capacity`, plus one
/// reserved overflow index (`== capacity`) shared by every combination
/// seen after the cap is reached.
#[derive(Debug)]
pub struct LabelInterner {
    capacity: u32,
    table: Mutex<HashMap<Vec<String>, u32>>,
    overflow_total: AtomicU64,
}

impl LabelInterner {
    /// Builds an interner with room for `capacity` distinct combinations
    /// before falling back to the shared overflow index.
    #[must_use]
    pub fn new(capacity: u32) -> Self {
        Self {
            capacity,
            table: Mutex::new(HashMap::new()),
            overflow_total: AtomicU64::new(0),
        }
    }

    /// The configured capacity (distinct combinations before overflow).
    #[must_use]
    pub const fn capacity(&self) -> u32 {
        self.capacity
    }

    /// The reserved index used for every combination beyond
    /// [`LabelInterner::capacity`]. Always equal to `capacity` itself,
    /// so a caller sizing a `capacity + 1`-length series table can use
    /// this as the last valid index.
    #[must_use]
    pub const fn overflow_index(&self) -> u32 {
        self.capacity
    }

    /// How many `intern` calls were routed to the shared overflow index
    /// because the capacity was already exhausted by other combinations.
    #[must_use]
    pub fn overflow_total(&self) -> u64 {
        self.overflow_total.load(Ordering::Relaxed)
    }

    /// Resolves `values` to a stable series index.
    ///
    /// The same `values` (compared element-wise) always resolves to the
    /// same index for the lifetime of this interner, *unless* the
    /// capacity was already full the first time it was seen, in which
    /// case it always resolves to [`LabelInterner::overflow_index`].
    ///
    /// Not on the allocation-free hot path: see the module docs.
    ///
    /// # Panics
    ///
    /// Never. A poisoned internal lock (unreachable, since nothing in
    /// this crate panics while holding it) is recovered via
    /// [`std::sync::PoisonError::into_inner`] rather than propagated,
    /// matching [`astrs_time::HlcClock`]'s documented rationale for the
    /// same pattern: a stale-but-valid interner state is always safe to
    /// keep using.
    #[must_use]
    pub fn intern(&self, values: &[&str]) -> u32 {
        // `HashMap<Vec<String>, u32>` has no `Borrow` impl that accepts a
        // `&[&str]` lookup key (the referent types differ), so the owned
        // key is built unconditionally, even on what turns out to be a
        // cache hit. That is the registration-time cost the module docs
        // call out — real hot-path calls go through an already-resolved
        // handle, not through `intern` at all.
        let owned: Vec<String> = values.iter().map(|s| (*s).to_owned()).collect();
        let mut table = self
            .table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(&id) = table.get(&owned) {
            return id;
        }
        let next = u32::try_from(table.len()).unwrap_or(u32::MAX);
        if next >= self.capacity {
            self.overflow_total.fetch_add(1, Ordering::Relaxed);
            return self.overflow_index();
        }
        table.insert(owned, next);
        next
    }

    /// Every `(values, index)` pair interned so far, in no particular
    /// order.
    ///
    /// A snapshot-time operation (used by
    /// [`crate::metrics::MetricFamily::snapshot_into`] to label each
    /// exported point): it clones the whole table under the lock rather
    /// than holding the lock across the caller's iteration, so it is not
    /// meant for the allocation-free hot path either.
    #[must_use]
    pub fn entries(&self) -> Vec<(Vec<String>, u32)> {
        let table = self
            .table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        table.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn same_values_resolve_to_the_same_index() {
        let interner = LabelInterner::new(4);
        let a = interner.intern(&["camera", "frames"]);
        let b = interner.intern(&["camera", "frames"]);
        assert_eq!(a, b);
    }

    #[test]
    fn distinct_values_get_distinct_indices() {
        let interner = LabelInterner::new(4);
        let a = interner.intern(&["camera"]);
        let b = interner.intern(&["lidar"]);
        assert_ne!(a, b);
    }

    #[test]
    fn indices_are_assigned_in_first_seen_order() {
        let interner = LabelInterner::new(4);
        assert_eq!(interner.intern(&["a"]), 0);
        assert_eq!(interner.intern(&["b"]), 1);
        assert_eq!(interner.intern(&["a"]), 0, "already interned");
        assert_eq!(interner.intern(&["c"]), 2);
    }

    #[test]
    fn exhausting_capacity_routes_new_combinations_to_overflow() {
        let interner = LabelInterner::new(2);
        assert_eq!(interner.intern(&["a"]), 0);
        assert_eq!(interner.intern(&["b"]), 1);
        // Capacity is full: a third distinct combination overflows.
        assert_eq!(interner.intern(&["c"]), interner.overflow_index());
        assert_eq!(interner.intern(&["d"]), interner.overflow_index());
        assert_eq!(interner.overflow_total(), 2);
    }

    #[test]
    fn a_combination_seen_before_overflow_keeps_its_real_index_forever() {
        let interner = LabelInterner::new(1);
        assert_eq!(interner.intern(&["a"]), 0);
        let _ = interner.intern(&["b"]); // overflow
        assert_eq!(interner.intern(&["a"]), 0, "must not move to overflow");
        assert_eq!(interner.overflow_total(), 1);
    }

    #[test]
    fn zero_capacity_overflows_immediately() {
        let interner = LabelInterner::new(0);
        assert_eq!(interner.intern(&["anything"]), interner.overflow_index());
        assert_eq!(interner.overflow_index(), 0);
        assert_eq!(interner.overflow_total(), 1);
    }

    #[test]
    fn empty_label_values_are_a_valid_single_combination() {
        let interner = LabelInterner::new(4);
        assert_eq!(interner.intern(&[]), 0);
        assert_eq!(interner.intern(&[]), 0);
    }

    #[test]
    fn overflow_total_starts_at_zero() {
        let interner = LabelInterner::new(10);
        assert_eq!(interner.overflow_total(), 0);
        assert_eq!(interner.capacity(), 10);
    }
}
