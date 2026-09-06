//! The bounded spin → yield → sleep schedule.
//!
//! # Where this is, and is not, allowed
//!
//! Blueprint §6.2's "never sleep-retry" is a rule about **pool exhaustion**:
//! a producer that cannot get a slot must fail over to the reliable daemon
//! path immediately, not sleep hoping a consumer catches up. That rule is
//! enforced structurally — [`crate::Producer::try_allocate`] contains no
//! wait of any kind.
//!
//! A *consumer* waiting for data is the opposite situation, and blocking is
//! exactly right there. Its primary mechanism is the doorbell
//! ([`crate::Doorbell`]), which costs nothing while idle. This schedule is
//! the fallback for the one case a doorbell cannot cover: a consumer whose
//! producer lives in another process that has not yet been handed the
//! doorbell descriptor — the window before the daemon wires the route (§6.3),
//! or a deployment running without a broker at all.
//!
//! Making that fallback explicit, bounded and named is the point. An
//! unbounded spin would burn a core on a robot; an unbounded sleep would hide
//! a mis-wired route behind latency nobody attributes to it.
//!
//! # Examples
//!
//! ```
//! use astrs_shm::Backoff;
//!
//! let mut backoff = Backoff::new();
//! assert!(!backoff.is_sleeping());
//! for _ in 0..Backoff::SPINS + Backoff::YIELDS {
//!     backoff.step();
//! }
//! assert!(backoff.is_sleeping());
//!
//! // Any progress restarts the schedule, so a busy stream never sleeps.
//! backoff.reset();
//! assert!(!backoff.is_sleeping());
//! ```

use std::time::Duration;

/// A bounded spin → yield → sleep schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    spins: u32,
    yields: u32,
    sleep: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Backoff {
    /// How many `spin_loop` hints come first.
    ///
    /// Sized to cover the producer's commit window — the handful of stores
    /// between "the slot is mine" and "the gate is open" — so a consumer that
    /// arrives mid-commit almost never gets as far as yielding.
    pub const SPINS: u32 = 64;

    /// How many scheduler yields follow the spins.
    pub const YIELDS: u32 = 8;

    /// The first sleep, once spinning and yielding are exhausted.
    pub const MIN_SLEEP: Duration = Duration::from_micros(100);

    /// The longest sleep the schedule grows to.
    ///
    /// The worst-case added latency for a route whose doorbell has not been
    /// wired: two milliseconds, against a 5 s heartbeat and a 200 Hz control
    /// loop, is a visible-in-telemetry degradation rather than a stall.
    pub const MAX_SLEEP: Duration = Duration::from_millis(2);

    /// A fresh schedule.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            spins: 0,
            yields: 0,
            sleep: Self::MIN_SLEEP,
        }
    }

    /// Take one step.
    pub fn step(&mut self) {
        if self.spins < Self::SPINS {
            self.spins += 1;
            std::hint::spin_loop();
            return;
        }
        if self.yields < Self::YIELDS {
            self.yields += 1;
            std::thread::yield_now();
            return;
        }
        std::thread::sleep(self.sleep);
        self.sleep = double_capped(self.sleep, Self::MAX_SLEEP);
    }

    /// The duration the *next* sleeping step would take.
    ///
    /// Exposed so an async caller can drive the same schedule with its own
    /// timer instead of blocking a runtime thread.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::Backoff;
    ///
    /// let backoff = Backoff::new();
    /// assert_eq!(backoff.next_sleep(), Backoff::MIN_SLEEP);
    /// ```
    #[must_use]
    pub const fn next_sleep(&self) -> Duration {
        self.sleep
    }

    /// Advance the schedule without sleeping, returning how long a caller
    /// should wait.
    ///
    /// `None` while the schedule is still in its spin or yield phase — the
    /// step has already been taken by the time this returns.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::Backoff;
    ///
    /// let mut backoff = Backoff::new();
    /// assert_eq!(backoff.step_without_sleeping(), None);
    /// for _ in 0..Backoff::SPINS + Backoff::YIELDS {
    ///     backoff.step_without_sleeping();
    /// }
    /// assert!(backoff.step_without_sleeping().is_some());
    /// ```
    pub fn step_without_sleeping(&mut self) -> Option<Duration> {
        if self.spins < Self::SPINS {
            self.spins += 1;
            std::hint::spin_loop();
            return None;
        }
        if self.yields < Self::YIELDS {
            self.yields += 1;
            std::thread::yield_now();
            return None;
        }
        let sleep = self.sleep;
        self.sleep = double_capped(self.sleep, Self::MAX_SLEEP);
        Some(sleep)
    }

    /// Whether the schedule has reached its sleeping phase.
    #[must_use]
    pub const fn is_sleeping(&self) -> bool {
        self.spins >= Self::SPINS && self.yields >= Self::YIELDS
    }

    /// How many steps have been taken.
    #[must_use]
    pub const fn steps(&self) -> u32 {
        self.spins + self.yields
    }

    /// Restart the schedule — called whenever progress is made.
    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

/// Double a duration, capped.
///
/// `Duration * 2` can overflow in principle; the cap makes that unreachable,
/// but the saturating form keeps the function total rather than relying on it.
const fn double_capped(value: Duration, cap: Duration) -> Duration {
    match value.checked_add(value) {
        Some(doubled) if doubled.as_nanos() < cap.as_nanos() => doubled,
        _ => cap,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_schedule_escalates_in_three_phases() {
        let mut backoff = Backoff::new();
        assert!(!backoff.is_sleeping());
        assert_eq!(backoff.steps(), 0);

        for _ in 0..Backoff::SPINS {
            assert_eq!(backoff.step_without_sleeping(), None, "spin phase");
        }
        assert!(!backoff.is_sleeping(), "still yielding");
        for _ in 0..Backoff::YIELDS {
            assert_eq!(backoff.step_without_sleeping(), None, "yield phase");
        }
        assert!(backoff.is_sleeping());
        assert_eq!(backoff.steps(), Backoff::SPINS + Backoff::YIELDS);
        assert_eq!(backoff.step_without_sleeping(), Some(Backoff::MIN_SLEEP));
    }

    #[test]
    fn sleeps_double_and_then_cap() {
        let mut backoff = Backoff::new();
        for _ in 0..Backoff::SPINS + Backoff::YIELDS {
            backoff.step_without_sleeping();
        }
        let mut previous = Duration::ZERO;
        for _ in 0..32 {
            let sleep = backoff
                .step_without_sleeping()
                .expect("the schedule is sleeping");
            assert!(sleep >= previous, "sleeps must not shrink");
            assert!(sleep <= Backoff::MAX_SLEEP, "sleeps must stay capped");
            previous = sleep;
        }
        assert_eq!(previous, Backoff::MAX_SLEEP);
        assert_eq!(backoff.next_sleep(), Backoff::MAX_SLEEP);
    }

    #[test]
    fn reset_restores_the_first_phase() {
        let mut backoff = Backoff::new();
        for _ in 0..200 {
            backoff.step_without_sleeping();
        }
        assert!(backoff.is_sleeping());
        backoff.reset();
        assert!(!backoff.is_sleeping());
        assert_eq!(backoff.steps(), 0);
        assert_eq!(backoff.next_sleep(), Backoff::MIN_SLEEP);
        assert_eq!(backoff, Backoff::default());
    }

    #[test]
    fn the_blocking_step_walks_the_same_schedule_as_the_non_blocking_one() {
        // The spin and yield phases must not sleep at all — a wall-clock
        // assertion would be flaky under a loaded test runner, so the
        // property is stated structurally instead: `step` and
        // `step_without_sleeping` traverse identical states, and only the
        // latter's `Some(_)` marks a phase that would have slept.
        let mut blocking = Backoff::new();
        let mut reporting = Backoff::new();
        for _ in 0..Backoff::SPINS + Backoff::YIELDS {
            blocking.step();
            assert_eq!(reporting.step_without_sleeping(), None, "no sleep yet");
            assert_eq!(blocking.steps(), reporting.steps());
            assert_eq!(blocking.is_sleeping(), reporting.is_sleeping());
        }
        assert!(blocking.is_sleeping());
        assert!(reporting.step_without_sleeping().is_some());
    }

    #[test]
    fn doubling_is_total() {
        assert_eq!(
            double_capped(Duration::from_micros(100), Duration::from_millis(2)),
            Duration::from_micros(200)
        );
        assert_eq!(
            double_capped(Duration::from_millis(4), Duration::from_millis(2)),
            Duration::from_millis(2)
        );
        assert_eq!(
            double_capped(Duration::MAX, Duration::from_millis(2)),
            Duration::from_millis(2)
        );
    }
}
