//! Exponential backoff with jitter (blueprint §12).
//!
//! The schedule is `base × multiplier^attempt`, capped, then reduced by a
//! random factor in `[1 − jitter, 1]`:
//!
//! ```text
//! attempt │ 0      1      2      3      4      5      6 …
//! ────────┼────────────────────────────────────────────────
//! plain   │ 250ms  500ms  1s     2s     4s     8s     16s
//! ±25%    │ 188…   375…   750…   1.5s…  3s…    6s…    12s…
//!         │ 250ms  500ms  1s     2s     4s     8s     16s
//! ```
//!
//! # Why jitter is not optional
//!
//! Twenty daemons in a rack lose the same coordinator at the same instant. With
//! a deterministic schedule they retry at the same instant too — and again 500
//! ms later, and again after a second — so the coordinator comes back up into a
//! synchronised thundering herd and falls over again. Jitter is what breaks the
//! phase lock, and it costs nothing.
//!
//! The randomness is a small xorshift generator seeded from the wall clock, not
//! a cryptographic source: a backoff delay is not a secret, and taking a
//! dependency for it would be absurd. [`Backoff::with_seed`] makes it fully
//! deterministic for tests.
//!
//! # Examples
//!
//! ```
//! use astrs_transport::{BackoffConfig, Backoff};
//! use std::time::Duration;
//!
//! let mut backoff = Backoff::new(BackoffConfig::new()).with_seed(7);
//! let first = backoff.next_delay().expect("a first attempt");
//! let second = backoff.next_delay().expect("a second attempt");
//!
//! assert!(first <= Duration::from_millis(250));
//! assert!(second <= Duration::from_millis(500));
//! assert_eq!(backoff.attempts(), 2);
//!
//! backoff.reset();
//! assert_eq!(backoff.attempts(), 0);
//! ```

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::BackoffConfig;

/// The exponential-backoff schedule for one reconnect loop.
#[derive(Debug, Clone)]
pub struct Backoff {
    /// The policy.
    config: BackoffConfig,
    /// How many delays have been handed out since the last reset.
    attempts: u32,
    /// The jitter generator's state.
    rng: u64,
}

impl Backoff {
    /// A schedule following `config`, seeded from the wall clock.
    #[must_use]
    pub fn new(config: BackoffConfig) -> Self {
        Self {
            config,
            attempts: 0,
            rng: seed_from_clock(),
        }
    }

    /// Replaces the jitter seed, making the schedule reproducible.
    ///
    /// A seed of zero would leave the xorshift generator stuck at zero
    /// forever, so it is replaced with a fixed non-zero constant.
    #[must_use]
    pub const fn with_seed(mut self, seed: u64) -> Self {
        self.rng = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        self
    }

    /// The policy this schedule follows.
    #[must_use]
    pub const fn config(&self) -> &BackoffConfig {
        &self.config
    }

    /// How many delays have been handed out since the last reset.
    #[must_use]
    pub const fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Whether the attempt ceiling has been reached.
    #[must_use]
    pub const fn is_exhausted(&self) -> bool {
        match self.config.max_attempts {
            Some(max) => self.attempts >= max,
            None => false,
        }
    }

    /// The next delay, or [`None`] once the attempt ceiling is reached.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::{Backoff, BackoffConfig};
    ///
    /// let mut backoff = Backoff::new(BackoffConfig::immediate().with_max_attempts(Some(2)));
    /// assert!(backoff.next_delay().is_some());
    /// assert!(backoff.next_delay().is_some());
    /// assert!(backoff.next_delay().is_none());
    /// ```
    pub fn next_delay(&mut self) -> Option<Duration> {
        if self.is_exhausted() {
            return None;
        }
        let plain = self.plain_delay(self.attempts);
        self.attempts = self.attempts.saturating_add(1);
        Some(self.apply_jitter(plain))
    }

    /// The delay for `attempt`, before jitter.
    ///
    /// Exposed because a supervisor's log ("retrying in ~4 s") wants the
    /// nominal figure, not the jittered one.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_transport::{Backoff, BackoffConfig};
    /// use std::time::Duration;
    ///
    /// let backoff = Backoff::new(BackoffConfig::new());
    /// assert_eq!(backoff.plain_delay(0), Duration::from_millis(250));
    /// assert_eq!(backoff.plain_delay(1), Duration::from_millis(500));
    /// assert_eq!(backoff.plain_delay(30), Duration::from_secs(30));
    /// ```
    #[must_use]
    pub fn plain_delay(&self, attempt: u32) -> Duration {
        let base = self.config.base.as_nanos();
        if base == 0 {
            return Duration::ZERO;
        }
        let cap = self.config.cap.as_nanos();
        let multiplier = u128::from(self.config.multiplier_percent);

        let mut delay = base;
        for _ in 0..attempt {
            delay = delay.saturating_mul(multiplier) / 100;
            if delay >= cap {
                return self.config.cap;
            }
        }
        duration_from_nanos(delay.min(cap))
    }

    /// Reduces `delay` by a random factor in `[1 − jitter, 1]`.
    fn apply_jitter(&mut self, delay: Duration) -> Duration {
        let jitter = u128::from(self.config.jitter_percent);
        if jitter == 0 || delay.is_zero() {
            return delay;
        }
        let span = delay.as_nanos().saturating_mul(jitter) / 100;
        if span == 0 {
            return delay;
        }
        let shaved = u128::from(self.next_random()) % (span + 1);
        duration_from_nanos(delay.as_nanos().saturating_sub(shaved))
    }

    /// Restarts the schedule after a successful connection.
    pub const fn reset(&mut self) {
        self.attempts = 0;
    }

    /// One step of xorshift64*.
    ///
    /// Fast, deterministic from a seed, and statistically far better than
    /// anything a backoff needs.
    fn next_random(&mut self) -> u64 {
        let mut state = self.rng;
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.rng = state;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Sleeps for the next delay, returning what it waited.
    ///
    /// # Errors
    ///
    /// [`None`] once the attempt ceiling is reached, in which case nothing was
    /// waited for.
    pub async fn wait(&mut self) -> Option<Duration> {
        let delay = self.next_delay()?;
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        Some(delay)
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(BackoffConfig::default())
    }
}

/// A non-cryptographic seed from the wall clock.
fn seed_from_clock() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0x2545_F491_4F6C_DD1D, |elapsed| elapsed.as_nanos() as u64);
    // Mix in the thread id so two loops started in the same nanosecond still
    // diverge — which is exactly the herd this jitter exists to break up.
    let thread = {
        let id = std::thread::current().id();
        // `ThreadId` has no numeric accessor on stable; its `Debug` form is
        // stable enough for a seed and costs nothing on a once-per-loop path.
        let text = format!("{id:?}");
        text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
        })
    };
    let seed = nanos ^ thread;
    if seed == 0 {
        0x9E37_79B9_7F4A_7C15
    } else {
        seed
    }
}

/// Builds a [`Duration`] from a nanosecond count that may not fit.
fn duration_from_nanos(nanos: u128) -> Duration {
    let clamped = nanos.min(u128::from(u64::MAX));
    Duration::from_nanos(clamped as u64)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_plain_schedule_doubles_and_then_caps() {
        let backoff = Backoff::new(BackoffConfig::new());
        assert_eq!(backoff.plain_delay(0), Duration::from_millis(250));
        assert_eq!(backoff.plain_delay(1), Duration::from_millis(500));
        assert_eq!(backoff.plain_delay(2), Duration::from_secs(1));
        assert_eq!(backoff.plain_delay(3), Duration::from_secs(2));
        assert_eq!(backoff.plain_delay(4), Duration::from_secs(4));
        assert_eq!(backoff.plain_delay(5), Duration::from_secs(8));
        assert_eq!(backoff.plain_delay(6), Duration::from_secs(16));
        // 32 s would exceed the 30 s cap.
        assert_eq!(backoff.plain_delay(7), Duration::from_secs(30));
        assert_eq!(backoff.plain_delay(1_000), Duration::from_secs(30));
    }

    #[test]
    fn a_growth_factor_other_than_doubling_is_honoured() {
        let config = BackoffConfig::new()
            .with_base(Duration::from_millis(100))
            .with_multiplier_percent(300);
        let backoff = Backoff::new(config);
        assert_eq!(backoff.plain_delay(0), Duration::from_millis(100));
        assert_eq!(backoff.plain_delay(1), Duration::from_millis(300));
        assert_eq!(backoff.plain_delay(2), Duration::from_millis(900));
    }

    #[test]
    fn a_flat_schedule_never_grows() {
        let config = BackoffConfig::new()
            .with_base(Duration::from_millis(50))
            .with_multiplier_percent(100)
            .with_jitter_percent(0);
        let backoff = Backoff::new(config);
        for attempt in 0..16 {
            assert_eq!(backoff.plain_delay(attempt), Duration::from_millis(50));
        }
    }

    #[test]
    fn jitter_only_ever_shortens_a_delay() {
        let config = BackoffConfig::new().with_jitter_percent(25);
        let mut backoff = Backoff::new(config).with_seed(0xdead_beef);
        for attempt in 0..12u32 {
            let plain = backoff.plain_delay(attempt);
            let jittered = backoff.next_delay().expect("a delay");
            assert!(jittered <= plain, "jitter must never lengthen a delay");
            let floor = plain.mul_f64(0.75);
            assert!(
                jittered >= floor.saturating_sub(Duration::from_nanos(1)),
                "attempt {attempt}: {jittered:?} is below the 25% floor {floor:?}"
            );
        }
    }

    #[test]
    fn full_jitter_can_reach_zero_but_not_below() {
        let config = BackoffConfig::new().with_jitter_percent(100);
        let mut backoff = Backoff::new(config).with_seed(1);
        for _ in 0..64 {
            let delay = backoff.next_delay().expect("a delay");
            assert!(delay <= Duration::from_secs(30));
        }
    }

    #[test]
    fn no_jitter_is_exactly_the_plain_schedule() {
        let config = BackoffConfig::new().with_jitter_percent(0);
        let mut backoff = Backoff::new(config);
        for attempt in 0..8u32 {
            let plain = backoff.plain_delay(attempt);
            assert_eq!(backoff.next_delay(), Some(plain));
        }
    }

    #[test]
    fn a_seeded_schedule_is_reproducible() {
        let config = BackoffConfig::new();
        let mut first = Backoff::new(config).with_seed(42);
        let mut second = Backoff::new(config).with_seed(42);
        for _ in 0..16 {
            assert_eq!(first.next_delay(), second.next_delay());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        let config = BackoffConfig::new();
        let mut first = Backoff::new(config).with_seed(1);
        let mut second = Backoff::new(config).with_seed(2);
        let firsts: Vec<_> = (0..16).filter_map(|_| first.next_delay()).collect();
        let seconds: Vec<_> = (0..16).filter_map(|_| second.next_delay()).collect();
        assert_ne!(
            firsts, seconds,
            "two seeds must not produce an identical herd"
        );
    }

    #[test]
    fn a_zero_seed_is_replaced_so_the_generator_still_runs() {
        let mut backoff = Backoff::new(BackoffConfig::new()).with_seed(0);
        let delays: Vec<_> = (0..8).filter_map(|_| backoff.next_delay()).collect();
        assert!(
            delays.iter().any(|delay| !delay.is_zero()),
            "a zero seed must not freeze the schedule"
        );
    }

    #[test]
    fn the_attempt_ceiling_ends_the_schedule() {
        let config = BackoffConfig::immediate().with_max_attempts(Some(3));
        let mut backoff = Backoff::new(config);
        assert!(!backoff.is_exhausted());
        assert!(backoff.next_delay().is_some());
        assert!(backoff.next_delay().is_some());
        assert!(backoff.next_delay().is_some());
        assert!(backoff.is_exhausted());
        assert!(backoff.next_delay().is_none());
        assert_eq!(backoff.attempts(), 3);

        backoff.reset();
        assert!(!backoff.is_exhausted());
        assert!(backoff.next_delay().is_some());
    }

    #[test]
    fn an_unbounded_schedule_never_exhausts() {
        let mut backoff = Backoff::new(BackoffConfig::immediate());
        for _ in 0..1_000 {
            assert!(backoff.next_delay().is_some());
        }
        assert!(!backoff.is_exhausted());
    }

    #[test]
    fn a_zero_base_is_always_zero() {
        let config = BackoffConfig::new()
            .with_base(Duration::ZERO)
            .with_cap(Duration::ZERO);
        let mut backoff = Backoff::new(config);
        for _ in 0..8 {
            assert_eq!(backoff.next_delay(), Some(Duration::ZERO));
        }
    }

    #[test]
    fn a_clock_seed_is_never_zero() {
        for _ in 0..8 {
            assert_ne!(seed_from_clock(), 0);
        }
    }

    #[test]
    fn the_default_follows_the_default_policy() {
        let backoff = Backoff::default();
        assert_eq!(backoff.config(), &BackoffConfig::default());
        assert_eq!(backoff.attempts(), 0);
    }

    #[tokio::test]
    async fn waiting_sleeps_for_the_delay_it_reports() {
        // Kept short: the point is that `wait` actually sleeps and reports the
        // delay it slept, not that the sleep is precise.
        let config = BackoffConfig::new()
            .with_base(Duration::from_millis(20))
            .with_cap(Duration::from_millis(20))
            .with_jitter_percent(0);
        let mut backoff = Backoff::new(config);
        let start = std::time::Instant::now();
        let waited = backoff.wait().await.expect("a delay");
        assert_eq!(waited, Duration::from_millis(20));
        assert!(start.elapsed() >= Duration::from_millis(15));
    }

    #[tokio::test]
    async fn waiting_past_the_ceiling_waits_for_nothing() {
        let mut backoff = Backoff::new(BackoffConfig::immediate().with_max_attempts(Some(1)));
        let start = std::time::Instant::now();
        assert!(backoff.wait().await.is_some());
        assert!(backoff.wait().await.is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_huge_multiplier_saturates_at_the_cap_rather_than_overflowing() {
        let config = BackoffConfig::new()
            .with_base(Duration::from_secs(1))
            .with_cap(Duration::from_secs(60))
            .with_multiplier_percent(u32::MAX);
        let backoff = Backoff::new(config);
        assert_eq!(backoff.plain_delay(1), Duration::from_secs(60));
        assert_eq!(backoff.plain_delay(64), Duration::from_secs(60));
    }
}
