//! The in-process metrics registry (blueprint §13).
//!
//! A lightweight, process-global home for [`Counter`], [`Gauge`] and
//! [`Histogram`] handles, with bounded-cardinality label sets and an
//! allocation-free record path once a handle has been resolved:
//!
//! | Module | Contents |
//! |---|---|
//! | [`counter`] | [`Counter`] — a monotonic `AtomicU64` |
//! | [`gauge`] | [`Gauge`] — a bit-punned `f64` in an `AtomicU64` |
//! | [`histogram`] | [`Histogram`] — fixed, pre-sized buckets |
//! | [`interner`] | [`LabelInterner`] — the bounded-cardinality mechanism |
//! | [`family`] | [`MetricFamily`] — one named metric's labelled series |
//! | [`registry`] | [`MetricRegistry`] — the process-global collection of families |
//!
//! # The allocation-free record path
//!
//! Every method on an already-resolved [`Counter`]/[`Gauge`]/[`Histogram`]
//! handle — `inc`, `add`, `set`, `observe` — touches only atomics: no
//! locks, no heap allocation. *Resolving* a handle (registering a metric,
//! or looking one up by label values through [`MetricFamily::get_or_create`])
//! is not on that path and may allocate; the intended usage is to resolve
//! once (typically at node/output setup) and reuse the handle for every
//! subsequent update. The unit test `metrics::tests::proves_the_record_path_is_allocation_free`
//! pins this with a counting global allocator rather than leaving it as
//! an unverified claim.
//!
//! # Bounded cardinality
//!
//! A [`MetricFamily`]'s label *keys* are fixed `&'static str`s chosen at
//! registration; label *values* are arbitrary caller-supplied strings
//! funnelled through a [`LabelInterner`] capped at a fixed number of
//! distinct combinations. Beyond the cap, every further combination
//! shares one overflow series and increments a counter — a mislabeled or
//! hostile input degrades a metric's resolution, it does not grow memory
//! without bound.
//!
//! # Examples
//!
//! ```
//! use astrs_telemetry::metrics::MetricRegistry;
//! use astrs_time::HlcTimestamp;
//!
//! let registry = MetricRegistry::new();
//! // A deliberately tiny capacity to demonstrate the overflow point;
//! // real callers pick something generous, e.g. `MetricRegistry::default_family_capacity()`.
//! let published = registry.register_counter_family("published_total", &["port"], 1);
//! published.get_or_create(&["image"]).inc();
//! published.get_or_create(&["depth"]).inc(); // capacity 1: this overflows.
//!
//! let batch = registry.snapshot(HlcTimestamp::new(1, 0), "camera_node");
//! assert_eq!(batch.points.len(), 2); // the "image" series, plus the overflow point
//! ```

pub mod counter;
pub mod family;
pub mod gauge;
pub mod histogram;
pub mod interner;
pub mod registry;

pub use counter::Counter;
pub use family::{MetricFamily, MetricSeries, OVERFLOW_LABEL_VALUE};
pub use gauge::Gauge;
pub use histogram::{DEFAULT_LATENCY_BUCKETS_SECONDS, Histogram};
pub use interner::{DEFAULT_MAX_SERIES, LabelInterner};
pub use registry::MetricRegistry;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::alloc_guard::allocations_so_far;

    /// Verifies the claim the module docs make: once a [`Counter`],
    /// [`Gauge`] and [`Histogram`] handle have been resolved, recording
    /// through them in a tight loop performs zero heap allocations.
    ///
    /// Uses the counting global allocator installed for this crate's test
    /// binary (see [`crate::alloc_guard`]) rather than reasoning about it
    /// from the source alone — the whole point of the claim is that it is
    /// checkable, not merely plausible.
    ///
    /// The allocation counter is process-wide, not per-thread (see
    /// [`crate::alloc_guard`]'s docs for why), so it is exact under a
    /// process-per-test runner (`cargo nextest`, this project's primary
    /// one) but could in principle see a stray allocation from an
    /// unrelated test's thread finishing mid-measurement under plain
    /// `cargo test`'s default multi-threaded-in-one-process harness. A
    /// genuine regression in this crate's record path allocates on
    /// *every* attempt; unrelated cross-talk does not, so retrying a
    /// handful of times and accepting the first clean run tells the two
    /// apart without weakening the assertion itself (each attempt still
    /// demands an exact zero-allocation delta).
    #[test]
    fn proves_the_record_path_is_allocation_free() {
        let registry = MetricRegistry::new();
        // Resolve every handle *before* the measurement window: this is
        // the "registration, not hot path" cost the module docs call out.
        let counter = registry.register_counter("hot_counter");
        let gauge = registry.register_gauge("hot_gauge");
        let histogram = registry.register_histogram("hot_histogram", vec![0.1, 1.0, 10.0]);
        let family = registry.register_counter_family("hot_family", &["port"], 8);
        let labelled = family.get_or_create(&["image"]);

        const MAX_ATTEMPTS: u32 = 5;
        let mut clean_run = false;
        let mut attempts_run = 0u32;
        for _ in 0..MAX_ATTEMPTS {
            attempts_run += 1;
            let before = allocations_so_far();
            for i in 0..10_000u64 {
                counter.inc();
                gauge.add(1.0);
                histogram.observe(0.5);
                labelled.inc_by(i);
            }
            let after = allocations_so_far();
            if after == before {
                clean_run = true;
                break;
            }
        }

        assert!(
            clean_run,
            "recording through already-resolved handles allocated on every one of {MAX_ATTEMPTS} attempts"
        );
        // Sanity: the loops really ran and landed their updates -- scaled
        // by however many attempts it actually took to see a clean one.
        assert_eq!(counter.get(), 10_000 * u64::from(attempts_run));
        assert_eq!(
            labelled.get(),
            (0..10_000u64).sum::<u64>() * u64::from(attempts_run)
        );
    }
}
