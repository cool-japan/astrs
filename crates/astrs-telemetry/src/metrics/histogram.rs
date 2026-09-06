//! [`Histogram`] — a fixed-bucket histogram metric handle.

use std::sync::atomic::{AtomicU64, Ordering};

use astrs_wire::HistogramBucket;

/// Latency-shaped default bucket bounds, in seconds, spanning 100 µs to
/// 10 s. A reasonable starting point for "how long did this take"
/// histograms; callers with a different natural range should pass their
/// own bounds to [`Histogram::new`] instead.
pub const DEFAULT_LATENCY_BUCKETS_SECONDS: &[f64] = &[
    0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
    5.0, 10.0,
];

/// A fixed-bucket histogram: a count, a running sum, and a fixed set of
/// bucket counters, all sized once at construction.
///
/// # Allocation-free record path
///
/// [`Histogram::observe`] does a binary search over the (already-boxed)
/// bound list and two atomic updates — no allocation, no lock. Buckets
/// hold **per-bucket** (not cumulative) counts internally; the cumulative
/// view the wire format ([`astrs_wire::HistogramBucket`]) expects is
/// computed once, in [`Histogram::snapshot`], which is called at the
/// export cadence (every few seconds), not per observation.
///
/// # Bucketing
///
/// `bounds` are the finite upper bounds, sorted ascending and deduplicated
/// at construction (a non-finite value in the input, e.g. `NaN` or
/// `f64::INFINITY`, is dropped — the `+Inf` bucket is implicit and always
/// present). An observed value goes into the first bucket whose bound is
/// `>= value`; a value larger than every finite bound, or `NaN`, goes into
/// the implicit `+Inf` bucket (see [`Histogram::observe`]'s docs for why
/// `NaN` is grouped there rather than the first bucket).
///
/// # Examples
///
/// ```
/// use astrs_telemetry::metrics::Histogram;
///
/// let histogram = Histogram::new(vec![1.0, 5.0, 10.0]);
/// histogram.observe(0.5);
/// histogram.observe(3.0);
/// histogram.observe(100.0);
///
/// let (count, sum, buckets) = histogram.snapshot();
/// assert_eq!(count, 3);
/// assert_eq!(sum, 103.5);
/// // Cumulative: <=1.0 -> 1, <=5.0 -> 2, <=10.0 -> 2, <=+Inf -> 3.
/// assert_eq!(buckets.len(), 4);
/// assert_eq!(buckets[3].cumulative_count, 3);
/// assert!(buckets[3].upper_bound.is_infinite());
/// ```
#[derive(Debug)]
pub struct Histogram {
    /// Finite upper bounds, ascending, deduplicated.
    bounds: Box<[f64]>,
    /// Per-bucket (non-cumulative) counts. Length `bounds.len() + 1`; the
    /// last slot is the implicit `+Inf` bucket.
    counts: Box<[AtomicU64]>,
    sum_bits: AtomicU64,
    count: AtomicU64,
}

impl Histogram {
    /// Builds a histogram with the given finite upper bounds.
    ///
    /// Non-finite entries (`NaN`, `+Inf`, `-Inf`) are dropped — the `+Inf`
    /// bucket always exists implicitly and does not need to be named —
    /// and the remainder is sorted ascending and deduplicated, so
    /// [`Histogram::bounds`] always returns a clean, well-formed bound
    /// list regardless of what was passed in. This keeps construction
    /// infallible: a caller building bounds from, say, a manifest field
    /// gets a usable histogram rather than a `Result` to unwrap.
    #[must_use]
    pub fn new(bounds: Vec<f64>) -> Self {
        let mut cleaned: Vec<f64> = bounds.into_iter().filter(|b| b.is_finite()).collect();
        cleaned.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        cleaned.dedup();
        let counts = (0..=cleaned.len()).map(|_| AtomicU64::new(0)).collect();
        Self {
            bounds: cleaned.into_boxed_slice(),
            counts,
            sum_bits: AtomicU64::new(0.0f64.to_bits()),
            count: AtomicU64::new(0),
        }
    }

    /// A histogram using [`DEFAULT_LATENCY_BUCKETS_SECONDS`].
    #[must_use]
    pub fn with_default_latency_buckets() -> Self {
        Self::new(DEFAULT_LATENCY_BUCKETS_SECONDS.to_vec())
    }

    /// The finite upper bounds this histogram was built with, ascending
    /// and deduplicated.
    #[must_use]
    pub fn bounds(&self) -> &[f64] {
        &self.bounds
    }

    /// Records one observation.
    ///
    /// `NaN` is grouped into the implicit `+Inf` overflow bucket rather
    /// than the first finite bucket: every finite comparison against
    /// `NaN` is false, so a naive "first bound `>= value`" search would
    /// place it in bucket `0`, silently misrepresenting a broken
    /// measurement as the *smallest* one observed. Routing it to the
    /// edge bucket instead keeps it visibly anomalous — the same
    /// "surface, don't hide" choice this crate makes for `NaN` gauges
    /// (see [`astrs_wire::MetricValue::bitwise_eq`]'s docs) — and it is
    /// still included in `sum`, so a `NaN` observation shows up as a
    /// `NaN` sum rather than being quietly dropped and leaving `count`
    /// and the bucket totals inconsistent with `sum`.
    pub fn observe(&self, value: f64) {
        let idx = if value.is_nan() {
            self.bounds.len()
        } else {
            self.bounds.partition_point(|&bound| bound < value)
        };
        self.counts[idx].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        let _ = self
            .sum_bits
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                Some((f64::from_bits(bits) + value).to_bits())
            });
    }

    /// The total number of observations.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// The running sum of every observed value.
    #[must_use]
    pub fn sum(&self) -> f64 {
        f64::from_bits(self.sum_bits.load(Ordering::Relaxed))
    }

    /// A consistent-enough snapshot: total count, running sum, and the
    /// **cumulative** bucket counts the wire format expects (ascending,
    /// always ending in an explicit `+Inf` bucket whose cumulative count
    /// equals the total).
    ///
    /// "Consistent-enough" because this reads `count`, `sum` and each
    /// bucket with separate atomic loads rather than one lock — under
    /// concurrent `observe` calls the individual numbers can be from
    /// slightly different instants, the same trade-off
    /// [`crate::metrics::Counter`] and [`crate::metrics::Gauge`] make for
    /// the same reason (an allocation-free, lock-free record path). A
    /// metrics snapshot sampled every few seconds does not need
    /// linearizability with the hot path.
    #[must_use]
    pub fn snapshot(&self) -> (u64, f64, Vec<HistogramBucket>) {
        let mut cumulative = 0u64;
        let mut buckets = Vec::with_capacity(self.bounds.len() + 1);
        for (bound, counter) in self.bounds.iter().zip(self.counts.iter()) {
            cumulative += counter.load(Ordering::Relaxed);
            buckets.push(HistogramBucket {
                upper_bound: *bound,
                cumulative_count: cumulative,
            });
        }
        cumulative += self.counts[self.bounds.len()].load(Ordering::Relaxed);
        buckets.push(HistogramBucket {
            upper_bound: f64::INFINITY,
            cumulative_count: cumulative,
        });
        (self.count(), self.sum(), buckets)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn empty_histogram_has_one_infinite_bucket_at_zero() {
        let histogram = Histogram::new(vec![1.0, 2.0]);
        let (count, sum, buckets) = histogram.snapshot();
        assert_eq!(count, 0);
        assert_eq!(sum, 0.0);
        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[2].upper_bound, f64::INFINITY);
        assert_eq!(buckets[2].cumulative_count, 0);
    }

    #[test]
    fn observations_land_in_the_first_bucket_at_or_above_value() {
        let histogram = Histogram::new(vec![1.0, 5.0, 10.0]);
        histogram.observe(0.5);
        histogram.observe(1.0); // exactly on the bound: inclusive.
        histogram.observe(3.0);
        histogram.observe(100.0);
        let (count, sum, buckets) = histogram.snapshot();
        assert_eq!(count, 4);
        assert_eq!(sum, 104.5);
        assert_eq!(
            buckets[0],
            HistogramBucket {
                upper_bound: 1.0,
                cumulative_count: 2
            }
        );
        assert_eq!(
            buckets[1],
            HistogramBucket {
                upper_bound: 5.0,
                cumulative_count: 3
            }
        );
        assert_eq!(
            buckets[2],
            HistogramBucket {
                upper_bound: 10.0,
                cumulative_count: 3
            }
        );
        assert_eq!(buckets[3].upper_bound, f64::INFINITY);
        assert_eq!(buckets[3].cumulative_count, 4);
    }

    #[test]
    fn bounds_are_sorted_deduplicated_and_stripped_of_non_finite_values() {
        let histogram = Histogram::new(vec![5.0, 1.0, 5.0, f64::NAN, f64::INFINITY, 2.0]);
        assert_eq!(histogram.bounds(), &[1.0, 2.0, 5.0]);
    }

    #[test]
    fn nan_observations_go_to_the_overflow_bucket_and_poison_the_sum() {
        let histogram = Histogram::new(vec![1.0, 2.0]);
        histogram.observe(0.5);
        histogram.observe(f64::NAN);
        let (count, sum, buckets) = histogram.snapshot();
        assert_eq!(count, 2);
        assert!(sum.is_nan());
        assert_eq!(
            buckets[0].cumulative_count, 1,
            "0.5 must not be miscounted as NaN's bucket"
        );
        assert_eq!(buckets.last().unwrap().cumulative_count, 2);
    }

    #[test]
    fn no_bounds_still_captures_everything_in_the_infinite_bucket() {
        let histogram = Histogram::new(vec![]);
        histogram.observe(42.0);
        let (count, _sum, buckets) = histogram.snapshot();
        assert_eq!(count, 1);
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].upper_bound, f64::INFINITY);
        assert_eq!(buckets[0].cumulative_count, 1);
    }

    #[test]
    fn default_latency_buckets_are_ascending() {
        let histogram = Histogram::with_default_latency_buckets();
        let bounds = histogram.bounds();
        assert!(bounds.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(bounds.len(), DEFAULT_LATENCY_BUCKETS_SECONDS.len());
    }

    #[test]
    fn concurrent_observations_total_correctly() {
        use std::sync::Arc;
        use std::thread;

        let histogram = Arc::new(Histogram::new(vec![1.0, 2.0, 3.0]));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let histogram = Arc::clone(&histogram);
                thread::spawn(move || {
                    for _ in 0..500 {
                        histogram.observe(1.5);
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let (count, sum, buckets) = histogram.snapshot();
        assert_eq!(count, 4_000);
        assert_eq!(sum, 6_000.0);
        assert_eq!(buckets[0].cumulative_count, 0, "<=1.0 must stay empty");
        assert_eq!(
            buckets[1].cumulative_count, 4_000,
            "<=2.0 catches every 1.5"
        );
    }
}
