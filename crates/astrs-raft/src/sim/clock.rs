//! [`VirtualClock`]: time that only moves when the simulation says so.
//!
//! A consensus bug that needs a fifteen-second partition to show itself is
//! unreachable in a test suite that runs in real time. A virtual clock makes
//! that partition free: [`VirtualClock::advance`] is a `+= 1`, and a thousand
//! simulated seconds cost a few microseconds.
//!
//! Just as importantly, it makes runs **reproducible**. Real time is the
//! largest source of nondeterminism in a distributed test; removing it is what
//! lets a failing seed be replayed exactly instead of chased.
//!
//! # Examples
//!
//! ```
//! use astrs_raft::sim::VirtualClock;
//! use std::time::Duration;
//!
//! let mut clock = VirtualClock::new(Duration::from_millis(50));
//! assert_eq!(clock.tick(), 0);
//! clock.advance();
//! clock.advance();
//! assert_eq!(clock.tick(), 2);
//! assert_eq!(clock.elapsed(), Duration::from_millis(100));
//! ```

use std::time::Duration;

/// A tick counter with a wall-clock interpretation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualClock {
    /// Ticks elapsed since the simulation started.
    tick: u64,
    /// What one tick is worth in wall-clock terms.
    interval: Duration,
}

impl VirtualClock {
    /// A clock at tick zero whose ticks are `interval` long.
    #[must_use]
    pub const fn new(interval: Duration) -> Self {
        Self { tick: 0, interval }
    }

    /// The current tick.
    #[must_use]
    pub const fn tick(&self) -> u64 {
        self.tick
    }

    /// What one tick is worth.
    #[must_use]
    pub const fn interval(&self) -> Duration {
        self.interval
    }

    /// Moves time forward by one tick.
    pub const fn advance(&mut self) {
        self.tick = self.tick.saturating_add(1);
    }

    /// Moves time forward by `ticks`.
    pub const fn advance_by(&mut self, ticks: u64) {
        self.tick = self.tick.saturating_add(ticks);
    }

    /// How much wall-clock time the simulation has covered.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.interval
            .saturating_mul(u32::try_from(self.tick).unwrap_or(u32::MAX))
    }

    /// The tick `ticks` in the future, saturating.
    #[must_use]
    pub const fn tick_after(&self, ticks: u64) -> u64 {
        self.tick.saturating_add(ticks)
    }
}

impl Default for VirtualClock {
    /// A clock whose ticks are the crate's own default tick interval.
    fn default() -> Self {
        Self::new(crate::config::DEFAULT_TICK_INTERVAL)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn a_fresh_clock_starts_at_zero() {
        let clock = VirtualClock::new(Duration::from_millis(10));
        assert_eq!(clock.tick(), 0);
        assert_eq!(clock.elapsed(), Duration::ZERO);
        assert_eq!(clock.interval(), Duration::from_millis(10));
    }

    #[test]
    fn advancing_moves_both_the_tick_and_the_wall_clock_reading() {
        let mut clock = VirtualClock::new(Duration::from_millis(20));
        clock.advance_by(50);
        assert_eq!(clock.tick(), 50);
        assert_eq!(clock.elapsed(), Duration::from_secs(1));
    }

    #[test]
    fn a_simulated_hour_costs_nothing() {
        // The point of the whole type: 3600 s of simulated time in one call.
        let mut clock = VirtualClock::new(Duration::from_millis(50));
        clock.advance_by(72_000);
        assert_eq!(clock.elapsed(), Duration::from_secs(3600));
    }

    #[test]
    fn future_ticks_are_computed_without_wrapping() {
        let mut clock = VirtualClock::new(Duration::from_millis(1));
        clock.advance_by(u64::MAX - 1);
        clock.advance();
        clock.advance();
        assert_eq!(clock.tick(), u64::MAX);
        assert_eq!(clock.tick_after(10), u64::MAX);
    }

    #[test]
    fn the_default_matches_the_crates_own_tick() {
        assert_eq!(
            VirtualClock::default().interval(),
            crate::config::DEFAULT_TICK_INTERVAL
        );
    }
}
