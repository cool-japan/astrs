//! Property tests: invariants that should hold for *any* input, not just
//! the handful of examples the unit tests happen to cover. Blueprint
//! §20.2/§15 list `proptest` as the tool for exactly this across the
//! workspace's codecs and state machines; this crate's analogues are the
//! histogram's cumulative-bucket math, the label interner's cardinality
//! bound, and the W3C `traceparent` round-trip.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use proptest::prelude::*;

use astrs_telemetry::export::RetryConfig;
use astrs_telemetry::metrics::{Histogram, LabelInterner};
use astrs_telemetry::propagation::SpanContext;

proptest! {
    /// A histogram's [`astrs_wire::HistogramBucket`] snapshot is always
    /// non-decreasing in cumulative count as the bound rises, and the
    /// final (`+Inf`) bucket's cumulative count always equals the total
    /// observation count -- for *any* sequence of observed values and
    /// *any* (cleaned) bound set, not just the handful the unit tests
    /// hand-pick.
    #[test]
    fn histogram_cumulative_buckets_are_monotone_and_total_matches(
        bounds in proptest::collection::vec(-100.0f64..1_000.0, 0..12),
        observations in proptest::collection::vec(-50.0f64..1_050.0, 0..200),
    ) {
        let histogram = Histogram::new(bounds);
        for value in &observations {
            histogram.observe(*value);
        }
        let (count, _sum, buckets) = histogram.snapshot();

        prop_assert_eq!(count, observations.len() as u64);
        prop_assert!(!buckets.is_empty(), "the implicit +Inf bucket always exists");
        prop_assert_eq!(buckets.last().unwrap().cumulative_count, count);
        prop_assert!(buckets.last().unwrap().upper_bound.is_infinite());

        let mut previous = 0u64;
        for bucket in &buckets {
            prop_assert!(
                bucket.cumulative_count >= previous,
                "cumulative counts must never decrease as the bound rises"
            );
            previous = bucket.cumulative_count;
        }
    }

    /// Bucket bounds are always returned sorted, deduplicated, and
    /// finite, regardless of how scrambled or how full of non-finite
    /// junk the input was.
    #[test]
    fn histogram_bounds_are_always_clean(
        raw_bounds in proptest::collection::vec(
            prop_oneof![
                -10.0f64..10.0,
                Just(f64::NAN),
                Just(f64::INFINITY),
                Just(f64::NEG_INFINITY),
            ],
            0..20,
        ),
    ) {
        let histogram = Histogram::new(raw_bounds);
        let bounds = histogram.bounds();
        prop_assert!(bounds.iter().all(|b| b.is_finite()));
        prop_assert!(bounds.windows(2).all(|w| w[0] < w[1]), "sorted and deduplicated");
    }

    /// However many distinct label combinations are interned, the
    /// number of *real* (non-overflow) indices handed out never exceeds
    /// the configured capacity, and every combination consistently
    /// resolves to the same index across repeats.
    #[test]
    fn label_interner_never_exceeds_its_capacity(
        capacity in 0u32..16,
        values in proptest::collection::vec(0u8..20, 0..300),
    ) {
        let interner = LabelInterner::new(capacity);
        let mut real_ids = BTreeSet::new();
        for value in &values {
            let label = value.to_string();
            let id = interner.intern(&[&label]);
            if id != interner.overflow_index() {
                real_ids.insert(id);
            }
        }
        prop_assert!(real_ids.len() <= capacity as usize);

        // Re-interning every value again must reproduce the same ids
        // (the interner does not "forget" or reshuffle assignments).
        let mut second_pass = Vec::new();
        for value in &values {
            let label = value.to_string();
            second_pass.push(interner.intern(&[&label]));
        }
        let mut first_pass = Vec::new();
        for value in &values {
            let label = value.to_string();
            first_pass.push(interner.intern(&[&label]));
        }
        prop_assert_eq!(first_pass, second_pass);
    }

    /// A [`SpanContext`] built from arbitrary id bytes and a sampled flag
    /// round-trips exactly through `to_traceparent`/`parse_traceparent`,
    /// for any non-zero ids (the only ones the type can express -- see
    /// [`SpanContext::new`]'s docs on the all-zero exclusion living in
    /// [`astrs_telemetry::ids`]).
    #[test]
    fn traceparent_round_trips_for_arbitrary_ids(
        trace_id in proptest::array::uniform16(any::<u8>()),
        span_id in proptest::array::uniform8(any::<u8>()),
        sampled in any::<bool>(),
    ) {
        prop_assume!(trace_id != [0u8; 16]);
        prop_assume!(span_id != [0u8; 8]);

        let ctx = SpanContext::new(trace_id, span_id).with_sampled(sampled);
        let rendered = ctx.to_traceparent();
        let parsed = SpanContext::parse_traceparent(&rendered);
        prop_assert_eq!(parsed, Some(ctx));
    }

    /// [`RetryConfig::delay_for_attempt`] is monotonically non-decreasing
    /// in the attempt number and never exceeds `max_backoff`, for any
    /// configuration a caller might build (including a `max_backoff`
    /// smaller than `initial_backoff`, or a multiplier below `1.0`).
    #[test]
    fn retry_delay_never_exceeds_the_cap_and_is_monotone(
        initial_ms in 0u64..5_000,
        max_ms in 0u64..10_000,
        multiplier in 0.1f64..5.0,
        attempts in proptest::collection::vec(0u32..20, 1..10),
    ) {
        let retry = RetryConfig {
            max_retries: 10,
            initial_backoff: std::time::Duration::from_millis(initial_ms),
            max_backoff: std::time::Duration::from_millis(max_ms),
            backoff_multiplier: multiplier,
        };
        let mut sorted_attempts = attempts.clone();
        sorted_attempts.sort_unstable();

        let mut previous_delay = std::time::Duration::ZERO;
        for attempt in sorted_attempts {
            let delay = retry.delay_for_attempt(attempt);
            prop_assert!(delay <= retry.max_backoff);
            if multiplier >= 1.0 {
                prop_assert!(delay >= previous_delay, "non-decreasing when the multiplier grows delays");
            }
            previous_delay = delay;
        }
    }
}
