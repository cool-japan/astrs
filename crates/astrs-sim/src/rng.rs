//! [`SplitMix64`] — a small, dependency-free, deterministic pseudo-random
//! generator, and [`SplitMix64::next_gaussian`] built on top of it.
//!
//! # Why this crate hand-rolls a PRNG instead of depending on `rand`
//!
//! Nothing in the workspace's root `Cargo.toml`
//! (`[workspace.dependencies]`) registers `rand`, `rand_chacha`, or any
//! other RNG crate (this wave's setup pass audited exactly what this
//! crate needs and did not add one), and this crate's own contract cannot
//! add a new one without editing the root manifest, which is out of scope
//! here. That constraint turns out to be
//! the *better* engineering choice anyway, not merely the compliant one:
//! this crate's whole determinism story
//! (`identical seed + inputs -> bit-identical trajectory`, see
//! [`crate::World`]) depends on the generator producing the *exact same*
//! sequence forever, on every machine, under every future compiler. A
//! third-party crate's generator algorithm is an implementation detail that
//! crate is free to change on any semver-compatible release (`rand`'s own
//! default generator has changed major algorithms across its history) —
//! exactly the kind of silent drift [`crate::World`]'s reproducibility
//! promise cannot tolerate. A small, frozen-forever algorithm this crate
//! owns outright has no such risk.
//!
//! # The algorithm
//!
//! [`SplitMix64`] is Sebastiano Vigna's `splitmix64` generator (2015,
//! released into the public domain / CC0 —
//! <https://prng.di.unimi.it/splitmix64.c>), reproduced here verbatim: a
//! 64-bit state advanced by a fixed odd increment (the golden-ratio-derived
//! constant `0x9E37_79B9_7F4A_7C15`), run through a three-round bit-mixing
//! finalizer. It is not cryptographically secure and is not intended to
//! be — this crate needs *reproducible* randomness for sensor and odometry
//! noise, not *unpredictable* randomness, and `splitmix64` is a
//! well-studied, extensively-tested choice for exactly that job (it is also
//! what `xoshiro`/`xoroshiro`'s own reference implementations use to expand
//! a single seed into their larger state, which is a reasonable proof by
//! itself that its output passes standard statistical batteries).

/// A `splitmix64` generator: 64 bits of state, advanced deterministically.
///
/// # Examples
///
/// ```
/// use astrs_sim::rng::SplitMix64;
///
/// let mut a = SplitMix64::new(42);
/// let mut b = SplitMix64::new(42);
/// // The same seed always produces the same sequence.
/// assert_eq!(a.next_u64(), b.next_u64());
/// assert_eq!(a.next_u64(), b.next_u64());
///
/// // A different seed (overwhelmingly likely) produces a different one.
/// let mut c = SplitMix64::new(43);
/// assert_ne!(SplitMix64::new(42).next_u64(), c.next_u64());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitMix64(u64);

/// The golden-ratio-derived odd increment `splitmix64` advances its state
/// by on every call — `2^64 / φ`, rounded to the nearest odd integer.
const GOLDEN_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

impl SplitMix64 {
    /// Seeds a generator. Every `u64` seed (including `0`) is valid and
    /// produces a well-defined, distinct sequence — `splitmix64` has no
    /// "weak seed" the way some smaller/older generators do.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The seed this generator would need to be constructed with to
    /// reproduce its *current* position in the sequence — i.e. its raw
    /// internal state, exposed for a caller that wants to snapshot and
    /// later resume a run bit-for-bit (e.g. [`crate::World`]'s own state
    /// carries one of these per noise source).
    #[must_use]
    pub const fn state(self) -> u64 {
        self.0
    }

    /// Restores a generator from a previously observed [`SplitMix64::state`].
    #[must_use]
    pub const fn from_state(state: u64) -> Self {
        Self(state)
    }

    /// Draws the next 64-bit value and advances the generator.
    ///
    /// Every call first advances the state by the fixed odd
    /// golden-ratio-derived increment documented at the top of this file
    /// (odd, so repeated addition cycles through all `2^64` states before
    /// repeating), then runs it through a three-round xor/multiply
    /// finalizer that scrambles the low-quality low bits of a simple
    /// additive sequence into a high-quality output.
    #[must_use]
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(GOLDEN_GAMMA);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform draw in `[0, 1)`.
    ///
    /// Takes the top 53 bits of [`SplitMix64::next_u64`] — `f64`'s mantissa
    /// width — so every representable output is equally likely and the
    /// result is always strictly less than `1.0`.
    #[must_use]
    pub fn next_f64(&mut self) -> f64 {
        const MANTISSA_SCALE: f64 = 1.0 / (1_u64 << 53) as f64;
        (self.next_u64() >> 11) as f64 * MANTISSA_SCALE
    }

    /// A standard normal (mean `0`, standard deviation `1`) draw, via the
    /// Box–Muller transform.
    ///
    /// # Why the polar form is not used instead
    ///
    /// The textbook rejection-sampling ("polar") variant of Box–Muller
    /// avoids the trig calls this one makes, at the cost of a rejection
    /// loop whose iteration count is not fixed — a poor fit for a generator
    /// whose whole purpose (see [module docs](self)) is that its state
    /// consumption is *exactly* predictable from the number of calls made.
    /// The basic transform used here always consumes exactly one
    /// [`SplitMix64::next_u64`]-pair worth of state (two draws) per call,
    /// which is what lets [`crate::odometry::OdometryModel`] advance its
    /// generator by a fixed, sigma-independent amount every tick (see that
    /// type's own docs).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_sim::rng::SplitMix64;
    ///
    /// let mut rng = SplitMix64::new(7);
    /// let samples: Vec<f64> = (0..10_000).map(|_| rng.next_gaussian()).collect();
    /// let mean: f64 = samples.iter().sum::<f64>() / samples.len() as f64;
    /// // Not exactly zero, but a 10k-sample mean of a unit normal is small.
    /// assert!(mean.abs() < 0.05, "mean was {mean}");
    /// assert!(samples.iter().all(|s| s.is_finite()));
    /// ```
    #[must_use]
    pub fn next_gaussian(&mut self) -> f64 {
        // `u1` is drawn as `1.0 - next_f64()` rather than `next_f64()`
        // directly so it ranges over `(0, 1]` instead of `[0, 1)` — `ln(0)`
        // is `-inf`, which would make an unlucky draw of exactly `0.0`
        // (rare, but `next_f64` can produce it) poison the result with a
        // NaN/infinite sample. `u1 == 1.0` is harmless: `ln(1.0) == 0.0`.
        let u1 = 1.0 - self.next_f64();
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn the_same_seed_reproduces_the_same_sequence() {
        let mut a = SplitMix64::new(1234);
        let mut b = SplitMix64::new(1234);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let mut a = SplitMix64::new(1);
        let mut b = SplitMix64::new(2);
        // At least one of the first handful of draws must differ — a
        // stronger, still-deterministic stand-in for "these are different
        // sequences" that does not merely hope no early collision happens.
        let diverged = (0..8).any(|_| a.next_u64() != b.next_u64());
        assert!(diverged);
    }

    #[test]
    fn a_zero_seed_is_not_a_degenerate_all_zero_sequence() {
        let mut rng = SplitMix64::new(0);
        let first = rng.next_u64();
        let second = rng.next_u64();
        assert_ne!(first, 0);
        assert_ne!(first, second);
    }

    #[test]
    fn state_and_from_state_round_trip_a_resumed_sequence() {
        let mut original = SplitMix64::new(99);
        let _ = original.next_u64();
        let _ = original.next_u64();
        let checkpoint = original.state();

        let expected_next = {
            let mut continued = original;
            continued.next_u64()
        };

        let mut resumed = SplitMix64::from_state(checkpoint);
        assert_eq!(resumed.next_u64(), expected_next);
    }

    #[test]
    fn next_f64_always_lands_in_the_half_open_unit_interval() {
        let mut rng = SplitMix64::new(2024);
        for _ in 0..50_000 {
            let value = rng.next_f64();
            assert!((0.0..1.0).contains(&value), "{value} out of range");
        }
    }

    #[test]
    fn next_f64_covers_more_than_a_narrow_band_of_the_unit_interval() {
        // Not a rigorous uniformity test — just a guard against an
        // implementation bug that collapses the output to (say) always
        // `< 0.01`, which "stays in `[0, 1)`" alone would not catch.
        let mut rng = SplitMix64::new(11);
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for _ in 0..10_000 {
            let value = rng.next_f64();
            min = min.min(value);
            max = max.max(value);
        }
        assert!(min < 0.05, "min was {min}");
        assert!(max > 0.95, "max was {max}");
    }

    #[test]
    fn next_gaussian_never_produces_nan_or_infinity() {
        let mut rng = SplitMix64::new(555);
        for _ in 0..100_000 {
            assert!(rng.next_gaussian().is_finite());
        }
    }

    #[test]
    fn next_gaussian_is_centered_near_zero_with_unit_scale() {
        let mut rng = SplitMix64::new(9001);
        let n = 200_000;
        let samples: Vec<f64> = (0..n).map(|_| rng.next_gaussian()).collect();
        let mean: f64 = samples.iter().sum::<f64>() / f64::from(n);
        let variance: f64 = samples.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / f64::from(n);
        assert!(mean.abs() < 0.02, "mean was {mean}");
        assert!((variance - 1.0).abs() < 0.05, "variance was {variance}");
    }

    #[test]
    fn next_gaussian_consumes_exactly_two_u64_draws_worth_of_state() {
        // See this method's own docs: a fixed, predictable state
        // consumption per call is a load-bearing property for
        // `OdometryModel`, not an incidental implementation detail.
        let mut via_gaussian = SplitMix64::new(4242);
        let _ = via_gaussian.next_gaussian();
        let after_gaussian = via_gaussian.state();

        let mut via_raw = SplitMix64::new(4242);
        let _ = via_raw.next_u64();
        let _ = via_raw.next_u64();
        assert_eq!(via_raw.state(), after_gaussian);
    }
}
