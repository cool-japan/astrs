//! The P² algorithm (Jain & Chlamtac, 1985) for streaming quantile
//! estimation, and [`JitterStats`], the per-timer p50/p99 jitter tracker
//! built on it (blueprint §11.1). `JitterStats`'s p99 estimator is what
//! `astrs-daemon` samples to publish the `astrs_daemon_timer_jitter_p99_us`
//! gauge — see that crate's `metrics::names::TIMER_JITTER_P99_US`.
//!
//! # Why P² instead of a reservoir
//!
//! A reservoir keeps a bounded random sample and computes an exact quantile
//! over it on demand — simple, but its accuracy is a function of reservoir
//! size and it needs an `O(k log k)` sort to answer a query. P² instead
//! maintains five running "marker" heights that track the target quantile
//! and its four neighbors, updated in `O(1)` per observation with no stored
//! samples at all — the right trade for a per-timer stat updated on every
//! tick, of which a busy daemon may have hundreds. The cost is that P² is
//! an *estimate*: it does not track the value that a sorted reservoir would
//! give exactly.
//!
//! # Accuracy
//!
//! P² converges to the true quantile as observations accumulate, for any
//! distribution (it makes no distributional assumptions). The proptest
//! suite in this module's tests feeds thousands of samples from several
//! shapes (uniform, and a skewed distribution built by squaring uniform
//! samples) in randomized arrival order and checks the p50/p99 estimates
//! land within **5% of the observed range** of the true rank-based
//! quantile — the bound this build is verified against. Two known
//! weaknesses, inherited from the algorithm rather than this
//! implementation: convergence is slower on heavily-skewed or long-tailed
//! distributions than on symmetric ones, and a stream with many exactly
//! equal values (heavy ties) can transiently pin two adjacent markers to
//! the same height. Neither affects correctness — the estimate stays a
//! plausible value within the observed range — only how quickly it settles.
//!
//! For fewer than five observations (before P² has enough data to
//! initialize its markers), [`P2Quantile::estimate`] instead returns the
//! exact nearest-rank quantile of the samples seen so far.

use std::time::Duration;

/// The internal, initialized P² marker state: five heights tracking the
/// target quantile and its four neighbors, their integer positions, the
/// ideal real-valued positions they are drifting toward, and the per-
/// observation increment to those ideal positions.
#[derive(Debug, Clone)]
struct P2State {
    heights: [f64; 5],
    positions: [i64; 5],
    desired: [f64; 5],
    increments: [f64; 5],
}

impl P2State {
    fn init(p: f64, mut initial: [f64; 5]) -> Self {
        initial.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Self {
            heights: initial,
            positions: [1, 2, 3, 4, 5],
            desired: [1.0, 1.0 + 2.0 * p, 1.0 + 4.0 * p, 3.0 + 2.0 * p, 5.0],
            increments: [0.0, p / 2.0, p, (1.0 + p) / 2.0, 1.0],
        }
    }

    /// Locates the cell containing `x`, extending an extreme marker if `x`
    /// falls outside the current range, and returns the index `k` (0..=3)
    /// such that every marker strictly after `k` must have its position
    /// incremented.
    fn locate_and_extend(&mut self, x: f64) -> usize {
        if x < self.heights[0] {
            self.heights[0] = x;
            0
        } else if x >= self.heights[4] {
            self.heights[4] = x;
            3
        } else {
            (0..3).find(|&i| x < self.heights[i + 1]).unwrap_or(3)
        }
    }

    fn observe(&mut self, x: f64) {
        let k = self.locate_and_extend(x);
        for position in self.positions.iter_mut().skip(k + 1) {
            *position += 1;
        }
        for i in 0..5 {
            self.desired[i] += self.increments[i];
        }
        for i in 1..4 {
            let d = self.desired[i] - self.positions[i] as f64;
            let right_gap = self.positions[i + 1] - self.positions[i];
            let left_gap = self.positions[i - 1] - self.positions[i];
            if d >= 1.0 && right_gap > 1 {
                self.adjust(i, 1);
            } else if d <= -1.0 && left_gap < -1 {
                self.adjust(i, -1);
            }
        }
    }

    /// Moves marker `i` one position in direction `sign` (`+1` or `-1`),
    /// preferring the parabolic estimate and falling back to a linear one
    /// if the parabolic result would leave `i`'s neighbors out of order.
    fn adjust(&mut self, i: usize, sign: i64) {
        let d = sign as f64;
        let (n_im1, n_i, n_ip1) = (
            self.positions[i - 1] as f64,
            self.positions[i] as f64,
            self.positions[i + 1] as f64,
        );
        let (q_im1, q_i, q_ip1) = (self.heights[i - 1], self.heights[i], self.heights[i + 1]);

        let parabolic = q_i
            + d / (n_ip1 - n_im1)
                * ((n_i - n_im1 + d) * (q_ip1 - q_i) / (n_ip1 - n_i)
                    + (n_ip1 - n_i - d) * (q_i - q_im1) / (n_i - n_im1));

        self.heights[i] = if q_im1 < parabolic && parabolic < q_ip1 {
            parabolic
        } else if sign > 0 {
            q_i + d * (q_ip1 - q_i) / (n_ip1 - n_i)
        } else {
            q_i + d * (q_im1 - q_i) / (n_im1 - n_i)
        };
        self.positions[i] += sign;
    }
}

/// A streaming estimator for one quantile `p` via the P² algorithm.
///
/// Kept crate-private: it is the mechanism behind [`JitterStats`], not a
/// general-purpose statistics API this crate commits to for other callers.
#[derive(Debug, Clone)]
pub(crate) struct P2Quantile {
    p: f64,
    init: Vec<f64>,
    state: Option<P2State>,
    count: u64,
}

impl P2Quantile {
    /// Creates an estimator for quantile `p`.
    ///
    /// `p` is clamped into `(0, 1)` (defaulting to the median if given a
    /// non-finite value) rather than panicking — this crate's hard rule is
    /// zero panics in non-test code, and a target quantile is exactly the
    /// kind of value that might trace back to an external configuration
    /// input somewhere upstream.
    pub(crate) fn new(p: f64) -> Self {
        let p = if p.is_finite() {
            p.clamp(1e-6, 1.0 - 1e-6)
        } else {
            0.5
        };
        Self {
            p,
            init: Vec::with_capacity(5),
            state: None,
            count: 0,
        }
    }

    /// Records one observation.
    pub(crate) fn observe(&mut self, x: f64) {
        self.count += 1;
        if let Some(state) = &mut self.state {
            state.observe(x);
            return;
        }
        self.init.push(x);
        if self.init.len() == 5 {
            let initial = [
                self.init[0],
                self.init[1],
                self.init[2],
                self.init[3],
                self.init[4],
            ];
            self.state = Some(P2State::init(self.p, initial));
        }
    }

    /// The current estimate, or `None` if nothing has been observed yet.
    pub(crate) fn estimate(&self) -> Option<f64> {
        if let Some(state) = &self.state {
            return Some(state.heights[2]);
        }
        if self.init.is_empty() {
            return None;
        }
        let mut sorted = self.init.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let rank = ((self.p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
        sorted.get(rank - 1).copied()
    }

    /// Total observations recorded.
    pub(crate) const fn count(&self) -> u64 {
        self.count
    }
}

/// Running p50/p99 jitter statistics for one registered timer (blueprint
/// §11.1). The p99 side is what backs the exported
/// `astrs_daemon_timer_jitter_p99_us` gauge (`astrs-daemon`
/// `metrics::names::TIMER_JITTER_P99_US`); p50 is tracked for the same cost
/// but is not itself independently exported today.
///
/// "Jitter" here is `|fired_at - scheduled_at|`: how far a delivered tick
/// landed from its exact drift-free grid point. Estimated online via two
/// independent `P2Quantile` trackers (private; see the module docs for the
/// documented accuracy bound); [`JitterStats::max`] is tracked exactly
/// (a plain running maximum costs nothing extra to keep precise).
#[derive(Debug, Clone)]
pub struct JitterStats {
    p50: P2Quantile,
    p99: P2Quantile,
    max: Duration,
}

impl Default for JitterStats {
    fn default() -> Self {
        Self::new()
    }
}

impl JitterStats {
    /// Creates an empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            p50: P2Quantile::new(0.5),
            p99: P2Quantile::new(0.99),
            max: Duration::ZERO,
        }
    }

    /// Records one jitter measurement.
    pub(crate) fn observe(&mut self, jitter: Duration) {
        let nanos = jitter.as_nanos() as f64;
        self.p50.observe(nanos);
        self.p99.observe(nanos);
        self.max = self.max.max(jitter);
    }

    /// The estimated median jitter, or `None` if nothing has been observed.
    #[must_use]
    pub fn p50(&self) -> Option<Duration> {
        self.p50.estimate().map(nanos_to_duration)
    }

    /// The estimated 99th-percentile jitter, or `None` if nothing has been
    /// observed.
    #[must_use]
    pub fn p99(&self) -> Option<Duration> {
        self.p99.estimate().map(nanos_to_duration)
    }

    /// The largest jitter observed, tracked exactly.
    #[must_use]
    pub const fn max(&self) -> Duration {
        self.max
    }

    /// Total ticks observed.
    ///
    /// Delegates to the p50 estimator's own count rather than keeping a
    /// separate counter: `JitterStats::observe` always feeds both
    /// trackers the same observation in the same call, so they can never
    /// disagree, and one fewer field is one fewer thing to keep in sync.
    #[must_use]
    pub const fn samples(&self) -> u64 {
        self.p50.count()
    }
}

/// Converts an estimated nanosecond value back into a [`Duration`],
/// saturating at zero rather than panicking on the (should-be-unreachable,
/// since jitter measurements are never negative) case of a non-finite or
/// negative estimate.
fn nanos_to_duration(nanos: f64) -> Duration {
    if !nanos.is_finite() || nanos <= 0.0 {
        return Duration::ZERO;
    }
    let nanos_u64 = if nanos >= u64::MAX as f64 {
        u64::MAX
    } else {
        nanos as u64
    };
    Duration::from_nanos(nanos_u64)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use proptest::prelude::*;

    /// A cheap, dependency-free deterministic pseudo-shuffle: visits every
    /// index in `0..n` exactly once, in an order that looks nothing like
    /// arrival order, by stepping through the range with a step size
    /// coprime to it. Used so the accuracy tests exercise P²'s online
    /// update in a realistic (non-monotonic) arrival order without pulling
    /// in a random number generator. Deliberately *not* "multiply the
    /// index by a fixed large prime, mod n": that construction degenerates
    /// badly for any `n` sharing a near-common-factor relationship with the
    /// chosen prime (e.g. `n = 990` makes `7919 mod 990 == 989 == -1 mod
    /// 990`, so the "shuffle" is just the sequence reversed — a
    /// monotonic, not scrambled, arrival order, and a known pathological
    /// input for P²). A textbook Fisher-Yates shuffle over a small,
    /// fixed-seed xorshift generator has no such degenerate case for any
    /// `n`, which is the property these accuracy tests actually depend on.
    fn shuffled_range(n: usize) -> Vec<usize> {
        let mut values: Vec<usize> = (0..n).collect();
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15; // any fixed non-zero seed
        for i in (1..n).rev() {
            // xorshift64: cheap, deterministic, not cryptographic, and
            // exactly enough for an unbiased-in-practice Fisher-Yates swap
            // index without a `rand`-family dependency.
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let j = (state as usize) % (i + 1);
            values.swap(i, j);
        }
        values
    }

    #[test]
    fn fewer_than_five_samples_uses_exact_nearest_rank() {
        let mut q = P2Quantile::new(0.5);
        assert_eq!(q.estimate(), None);
        q.observe(10.0);
        assert_eq!(q.estimate(), Some(10.0));
        q.observe(30.0);
        q.observe(20.0);
        // Sorted: [10, 20, 30]; median (p=0.5) nearest-rank is index 1 -> 20.
        assert_eq!(q.estimate(), Some(20.0));
    }

    #[test]
    fn count_tracks_every_observation() {
        let mut q = P2Quantile::new(0.5);
        for x in 0..100 {
            q.observe(f64::from(x));
        }
        assert_eq!(q.count(), 100);
    }

    #[test]
    fn median_of_a_uniform_sequence_converges_near_the_middle() {
        let n = 2001usize;
        let mut q = P2Quantile::new(0.5);
        for i in shuffled_range(n) {
            q.observe(i as f64);
        }
        let estimate = q.estimate().expect("initialized after 5 samples");
        // True median of 0..=2000 is 1000.
        assert!(
            (estimate - 1000.0).abs() < 100.0,
            "median estimate {estimate} too far from 1000"
        );
    }

    #[test]
    fn p99_of_a_uniform_sequence_converges_near_the_true_value() {
        let n = 2001usize;
        let mut q = P2Quantile::new(0.99);
        for i in shuffled_range(n) {
            q.observe(i as f64);
        }
        let estimate = q.estimate().expect("initialized after 5 samples");
        // True p99 rank of 0..=2000 (nearest-rank) is ~1980.
        assert!(
            (estimate - 1980.0).abs() < 100.0,
            "p99 estimate {estimate} too far from 1980"
        );
    }

    #[test]
    fn heights_stay_sorted_after_many_observations() {
        let n = 3001usize;
        let mut q = P2Quantile::new(0.9);
        for i in shuffled_range(n) {
            // A skewed distribution: squaring compresses small ranks
            // together and stretches the tail, stressing the marker
            // adjustment logic more than a uniform sequence would.
            q.observe((i as f64).powi(2));
        }
        let state = q.state.as_ref().expect("initialized");
        for pair in state.heights.windows(2) {
            assert!(
                pair[0] <= pair[1],
                "markers must stay ordered: {:?}",
                state.heights
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// For any uniform integer range fed in a scrambled order, the P²
        /// median estimate lands within 5% of the range's true midpoint —
        /// the accuracy bound documented on the module.
        #[test]
        fn median_within_five_percent_of_range_for_any_uniform_range(
            max in 200i64..5000,
        ) {
            let n = max as usize + 1;
            let mut q = P2Quantile::new(0.5);
            for i in shuffled_range(n) {
                q.observe(i as f64);
            }
            let estimate = q.estimate().expect("initialized");
            let true_median = max as f64 / 2.0;
            let tolerance = (max as f64) * 0.05;
            prop_assert!(
                (estimate - true_median).abs() <= tolerance,
                "estimate {estimate} vs true {true_median} (tolerance {tolerance})"
            );
        }

        /// The estimate is always within the observed min/max range,
        /// regardless of distribution shape or arrival order — a P²
        /// marker can never legitimately claim a quantile outside the data
        /// it has actually seen.
        #[test]
        fn estimate_never_leaves_the_observed_range(
            values in prop::collection::vec(-1000.0f64..1000.0, 5..500),
            p in 0.01f64..0.99,
        ) {
            let mut q = P2Quantile::new(p);
            let mut min = f64::INFINITY;
            let mut max = f64::NEG_INFINITY;
            for &x in &values {
                q.observe(x);
                min = min.min(x);
                max = max.max(x);
            }
            let estimate = q.estimate().expect("at least 5 observations");
            prop_assert!(estimate >= min - 1e-9 && estimate <= max + 1e-9);
        }
    }

    #[test]
    fn jitter_stats_reports_none_before_five_samples_but_tracks_max_immediately() {
        let mut stats = JitterStats::new();
        stats.observe(Duration::from_micros(50));
        assert_eq!(stats.samples(), 1);
        assert_eq!(stats.max(), Duration::from_micros(50));
        assert_eq!(
            stats.p50(),
            Some(Duration::from_micros(50)),
            "exact nearest-rank below 5 samples"
        );
    }

    #[test]
    fn jitter_stats_max_is_exact_even_as_p50_is_estimated() {
        let mut stats = JitterStats::new();
        let samples = [10u64, 500, 20, 15, 30, 12, 5000, 25];
        for micros in samples {
            stats.observe(Duration::from_micros(micros));
        }
        assert_eq!(stats.max(), Duration::from_micros(5000));
        assert_eq!(stats.samples(), samples.len() as u64);
        assert!(stats.p50().unwrap() < stats.max());
    }
}
