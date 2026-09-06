//! Jittered beacon send intervals (blueprint §6.4: "jittered send intervals
//! (default 1s ± 20%)").
//!
//! Jitter exists so that a cluster-wide event (a power cycle, a coordinated
//! deploy) that restarts every daemon at once does not leave them all
//! beaconing on the exact same schedule forever after — a "beacon storm"
//! that defeats the point of spreading announcements over time. See
//! [`crate::defaults::DEFAULT_BEACON_INTERVAL`] and
//! [`crate::defaults::DEFAULT_JITTER_RATIO`] for the cluster-wide defaults.

use std::hash::{Hash, Hasher};
use std::time::Duration;

use astrs_wire::DaemonId;

/// Draws one jittered interval from `[base * (1 - ratio), base * (1 +
/// ratio)]`.
///
/// `ratio` is clamped to `[0.0, 1.0]` so a caller-supplied out-of-range
/// value can never produce a negative or unbounded interval.
///
/// # Entropy source and infallible fallback
///
/// The primary path draws from [`oxicrypto::random_range`], the platform
/// CSPRNG already used elsewhere in this crate (`auth_tag` signing keys
/// ultimately trace back to the same source via [`astrs_wire::AuthToken`]).
/// A CSPRNG can, in principle, fail (entropy source unavailable); jitter
/// exists purely to *desynchronize* senders, so on that failure this
/// function falls back to a cheap, always-available pseudo-random value
/// derived from `machine_id` (unique per process — see
/// [`crate::beacon::Beacon`]'s "Identity and dedup" docs) mixed with `tick`
/// (the caller's own send-loop counter) through [`std::hash::DefaultHasher`].
/// The fallback is deliberately *not* "no jitter at all": falling back to
/// the unjittered midpoint on every sender simultaneously would resynchronize
/// exactly the storm jitter exists to prevent, and every sender's `tick`
/// counter reaching the same failure at (roughly) the same wall-clock moment
/// is exactly the correlated-failure scenario ("every daemon restarted by
/// the same event") jitter is meant to survive.
///
/// # Examples
///
/// ```
/// use astrs_discovery::jitter::jittered_interval;
/// use astrs_wire::DaemonId;
/// use std::time::Duration;
///
/// let id = DaemonId::generate(None);
/// let interval = jittered_interval(Duration::from_secs(1), 0.20, &id, 0);
/// assert!(interval >= Duration::from_millis(800));
/// assert!(interval <= Duration::from_millis(1_200));
/// ```
#[must_use]
pub fn jittered_interval(base: Duration, ratio: f64, machine_id: &DaemonId, tick: u64) -> Duration {
    let ratio = if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let base_ms = u64::try_from(base.as_millis()).unwrap_or(u64::MAX);
    // Both casts are safe *for this computation*, and neither loss is a bug:
    // `base_ms as f64` is exact for every beacon interval a human would
    // configure (f64 is exact below 2^53 ms ≈ 285,000 years), and the
    // truncating `as u64` on the way back cannot overflow because `ratio` is
    // clamped to `0.0..=1.0`, so the product never exceeds `base_ms`. A
    // sub-millisecond rounding difference in a jitter *spread* is by
    // definition immaterial — the value is about to be randomized anyway.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let spread_ms = (base_ms as f64 * ratio) as u64;
    let lo_ms = base_ms.saturating_sub(spread_ms);
    let hi_ms = base_ms.saturating_add(spread_ms);

    if lo_ms >= hi_ms {
        // Zero-width range (ratio == 0, or `base` too small to have a
        // millisecond-granular spread): nothing to draw, return the base
        // exactly rather than asking `random_range` for an empty range
        // (which it rejects as `BadInput`).
        return Duration::from_millis(base_ms);
    }

    // `random_range` is `[min, max)`; add one so `hi_ms` itself is reachable.
    match oxicrypto::random_range(lo_ms, hi_ms.saturating_add(1)) {
        Ok(ms) => Duration::from_millis(ms),
        Err(_) => Duration::from_millis(lo_ms) + fallback_offset(machine_id, tick, hi_ms - lo_ms),
    }
}

/// The infallible fallback jitter source: mixes `machine_id` and `tick`
/// through [`std::hash::DefaultHasher`] (SipHash) rather than a hand-rolled
/// generator, since std already provides a well-distributed, dependency-free
/// hash and this crate's dependency policy forbids adding a `rand`-family
/// crate.
fn fallback_offset(machine_id: &DaemonId, tick: u64, spread_ms: u64) -> Duration {
    if spread_ms == 0 {
        return Duration::ZERO;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    machine_id.hash(&mut hasher);
    tick.hash(&mut hasher);
    Duration::from_millis(hasher.finish() % spread_ms)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn id() -> DaemonId {
        "fixture-00000000-0000-0000-0000-0000000000cd"
            .parse()
            .unwrap()
    }

    #[test]
    fn stays_within_the_configured_ratio() {
        let base = Duration::from_secs(1);
        for tick in 0..500u64 {
            let interval = jittered_interval(base, 0.20, &id(), tick);
            assert!(interval >= Duration::from_millis(800), "{interval:?}");
            assert!(interval <= Duration::from_millis(1_200), "{interval:?}");
        }
    }

    #[test]
    fn draws_vary_across_calls() {
        let base = Duration::from_secs(1);
        let samples: std::collections::BTreeSet<Duration> = (0..50u64)
            .map(|tick| jittered_interval(base, 0.20, &id(), tick))
            .collect();
        assert!(
            samples.len() > 1,
            "successive draws must not all collapse to one value"
        );
    }

    #[test]
    fn zero_ratio_returns_the_base_exactly() {
        let base = Duration::from_millis(250);
        for tick in 0..10u64 {
            assert_eq!(jittered_interval(base, 0.0, &id(), tick), base);
        }
    }

    #[test]
    fn out_of_range_ratio_is_clamped_not_propagated() {
        let base = Duration::from_secs(1);
        // Never panics, and never exceeds the fully-clamped [0%, 100%] band.
        let low = jittered_interval(base, -5.0, &id(), 0);
        let high = jittered_interval(base, 5.0, &id(), 0);
        assert!(low <= base);
        assert!(high <= base * 2);
        let nan = jittered_interval(base, f64::NAN, &id(), 0);
        assert_eq!(nan, base);
    }

    #[test]
    fn fallback_offset_stays_within_the_spread_and_varies() {
        let spread = 400u64;
        let offsets: std::collections::BTreeSet<u64> = (0..50u64)
            .map(|tick| {
                let d = fallback_offset(&id(), tick, spread);
                assert!(d <= Duration::from_millis(spread));
                d.as_millis() as u64
            })
            .collect();
        assert!(offsets.len() > 1);
    }

    #[test]
    fn fallback_offset_differs_across_machine_ids() {
        let a: DaemonId = "a-00000000-0000-0000-0000-0000000000aa".parse().unwrap();
        let b: DaemonId = "b-00000000-0000-0000-0000-0000000000bb".parse().unwrap();
        // Not a strict inequality guarantee (hash collisions exist), but
        // with a 400ms spread and two very different identities colliding
        // on every one of a hundred ticks would be an actual hash defect.
        let differing = (0..100u64)
            .filter(|&tick| fallback_offset(&a, tick, 400) != fallback_offset(&b, tick, 400))
            .count();
        assert!(differing > 50, "fallback jitter barely varies by identity");
    }

    #[test]
    fn fallback_offset_of_zero_spread_is_zero() {
        assert_eq!(fallback_offset(&id(), 42, 0), Duration::ZERO);
    }
}
