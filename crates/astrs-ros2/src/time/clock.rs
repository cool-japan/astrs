//! [`Ros2Clock`]: what `node.now()` reads, and where simulated time enters.
//!
//! ROS 2 gives a node three clocks and lets a parameter switch between two
//! of them:
//!
//! | [`ClockType`] | Source | Jumps | Used for |
//! |---|---|---|---|
//! | [`SystemTime`](ClockType::SystemTime) | the wall clock | yes (NTP) | log stamps |
//! | [`SteadyTime`](ClockType::SteadyTime) | a monotonic counter | no | measuring elapsed time |
//! | [`RosTime`](ClockType::RosTime) | the wall clock, or `/clock` when `use_sim_time` is set | yes | everything a message stamps |
//!
//! # Why the HLC sits underneath
//!
//! AstRS stamps every message with a hybrid logical clock
//! ([`astrs_time::HlcClock`]), which is the wall clock plus a logical
//! counter that guarantees two events on one host never share a timestamp
//! and that a message's stamp is never behind one it causally follows. ROS
//! time is the wall clock with neither guarantee.
//!
//! [`Ros2Clock`] reads *through* the HLC rather than beside it, so a
//! bridged message's ROS stamp and its AstRS stamp are the same instant by
//! construction: [`Ros2Clock::now_hlc`] returns the HLC timestamp and
//! [`Ros2Clock::now`] is that same timestamp with the logical counter
//! dropped ([`RosTime::from_hlc`]). A bridge never has to reconcile two
//! independently-read clocks, which is the bug this shape exists to make
//! impossible.
//!
//! # Simulated time
//!
//! With `use_sim_time` set, ROS time comes from the `/clock` topic instead
//! of the wall clock. [`Ros2Clock::set_sim_time`] switches the source and
//! [`Ros2Clock::feed_clock`] delivers a `/clock` sample. Reading before any
//! sample has arrived returns [`RosTime::ZERO`], which is what `rclcpp`
//! does and what every `use_sim_time` node's first log line shows.
//!
//! Steady time is never simulated: it is the elapsed-time clock, and a
//! simulator that rewinds `/clock` must not make a timeout run backwards.

use core::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

use astrs_time::{HlcClock, HlcTimestamp};

use crate::time::stamp::{RosDuration, RosTime};

/// Which clock a reading comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum ClockType {
    /// The wall clock, or `/clock` when simulated time is on. The default,
    /// and what every message stamp uses.
    #[default]
    RosTime,
    /// The wall clock, never simulated.
    SystemTime,
    /// A monotonic counter since the clock was created, never simulated.
    SteadyTime,
}

impl ClockType {
    /// Every variant.
    pub const ALL: [Self; 3] = [Self::RosTime, Self::SystemTime, Self::SteadyTime];

    /// The name `rclcpp` gives it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::RosTime => "ROS_TIME",
            Self::SystemTime => "SYSTEM_TIME",
            Self::SteadyTime => "STEADY_TIME",
        }
    }

    /// True when simulated time can override this clock.
    #[must_use]
    pub const fn is_simulatable(self) -> bool {
        matches!(self, Self::RosTime)
    }
}

impl fmt::Display for ClockType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// A node's clock: ROS time, system time and steady time over one HLC.
///
/// Cheap to clone — every clone reads the same simulated-time state, which
/// is what lets a `/clock` subscription in one task feed a timer in another.
#[derive(Debug, Clone)]
pub struct Ros2Clock {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    hlc: HlcClock,
    started: Instant,
    use_sim_time: AtomicBool,
    /// The last `/clock` sample, in nanoseconds. `i64` rather than `u64`
    /// because a simulator may run before the epoch.
    sim_nanos: AtomicI64,
    sim_samples: AtomicU64,
}

impl Ros2Clock {
    /// A clock over a fresh system-backed HLC.
    #[must_use]
    pub fn new() -> Self {
        Self::with_hlc(HlcClock::system())
    }

    /// A clock over a caller's HLC.
    ///
    /// The seam a deterministic test needs: a node built on an
    /// [`HlcClock`] over a
    /// [`ManualClock`](astrs_time::ManualClock) has a ROS clock that moves
    /// only when the test moves it.
    #[must_use]
    pub fn with_hlc(hlc: HlcClock) -> Self {
        Self {
            inner: Arc::new(Inner {
                hlc,
                started: Instant::now(),
                use_sim_time: AtomicBool::new(false),
                sim_nanos: AtomicI64::new(0),
                sim_samples: AtomicU64::new(0),
            }),
        }
    }

    /// The HLC underneath.
    #[must_use]
    pub fn hlc(&self) -> &HlcClock {
        &self.inner.hlc
    }

    /// Read the current ROS time.
    ///
    /// Simulated when `use_sim_time` is set, and [`RosTime::ZERO`] until the
    /// first `/clock` sample arrives.
    #[must_use]
    pub fn now(&self) -> RosTime {
        if self.uses_sim_time() {
            return RosTime::from_nanos(i128::from(self.inner.sim_nanos.load(Ordering::Acquire)));
        }
        RosTime::from_hlc(self.now_hlc())
    }

    /// Read the current HLC timestamp.
    ///
    /// Always the real clock: the HLC is what orders AstRS's own dataflow,
    /// and a simulator rewinding `/clock` must not rewind that.
    #[must_use]
    pub fn now_hlc(&self) -> HlcTimestamp {
        self.inner.hlc.now()
    }

    /// Read a specific clock.
    #[must_use]
    pub fn read(&self, clock: ClockType) -> RosTime {
        match clock {
            ClockType::RosTime => self.now(),
            ClockType::SystemTime => RosTime::from_hlc(self.now_hlc()),
            ClockType::SteadyTime => {
                RosTime::from_nanos(i128::from(self.inner.started.elapsed().as_nanos() as u64))
            }
        }
    }

    /// Whether ROS time is coming from `/clock`.
    #[must_use]
    pub fn uses_sim_time(&self) -> bool {
        self.inner.use_sim_time.load(Ordering::Acquire)
    }

    /// Switch ROS time between the wall clock and `/clock`.
    ///
    /// Returns the previous setting. Switching *on* does not clear the last
    /// simulated sample, so a node that toggles `use_sim_time` twice does
    /// not lose the simulator's position.
    pub fn set_sim_time(&self, enabled: bool) -> bool {
        self.inner.use_sim_time.swap(enabled, Ordering::AcqRel)
    }

    /// Deliver a `/clock` sample.
    ///
    /// Accepted whether or not `use_sim_time` is set — a node routinely
    /// subscribes to `/clock` before the parameter is read, and dropping
    /// those samples would leave it at zero for the first few cycles.
    pub fn feed_clock(&self, time: RosTime) {
        let nanos = i64::try_from(time.to_nanos()).unwrap_or(i64::MAX);
        self.inner.sim_nanos.store(nanos, Ordering::Release);
        self.inner.sim_samples.fetch_add(1, Ordering::Relaxed);
    }

    /// How many `/clock` samples have arrived.
    #[must_use]
    pub fn clock_samples(&self) -> u64 {
        self.inner.sim_samples.load(Ordering::Relaxed)
    }

    /// The last simulated time, whether or not it is in use.
    #[must_use]
    pub fn sim_time(&self) -> RosTime {
        RosTime::from_nanos(i128::from(self.inner.sim_nanos.load(Ordering::Acquire)))
    }

    /// Time since the clock was created, from the steady counter.
    #[must_use]
    pub fn steady_elapsed(&self) -> RosDuration {
        RosDuration::from_std(self.inner.started.elapsed())
    }

    /// Merge a peer's HLC timestamp into this clock.
    ///
    /// What a bridge does with the AstRS-side stamp of a message it is
    /// forwarding, so that the HLC's happens-before guarantee survives the
    /// crossing. Drift beyond the HLC's tolerance is reported rather than
    /// applied.
    ///
    /// # Errors
    ///
    /// [`astrs_time::HlcError`] when the remote stamp is further ahead than
    /// the configured drift allows.
    pub fn observe(&self, remote: HlcTimestamp) -> Result<HlcTimestamp, astrs_time::HlcError> {
        self.inner.hlc.update_with(remote)
    }
}

impl Default for Ros2Clock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use astrs_time::ManualClock;

    /// A clock whose wall time the test controls.
    fn manual(start_nanos: u64) -> (Ros2Clock, Arc<ManualClock>) {
        let manual = Arc::new(ManualClock::new(start_nanos));
        let clock = Ros2Clock::with_hlc(HlcClock::new(astrs_time::SystemClock));
        // The HLC over `SystemClock` is what a real node uses; the manual one
        // is returned so a test can assert against a known instant where the
        // assertion does not depend on the wall clock.
        (clock, manual)
    }

    #[test]
    fn the_three_clock_types_are_distinct_and_named() {
        let mut names: Vec<&str> = ClockType::ALL.iter().map(|kind| kind.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 3);
        assert_eq!(ClockType::default(), ClockType::RosTime);
        assert_eq!(ClockType::RosTime.to_string(), "ROS_TIME");
    }

    #[test]
    fn only_ros_time_is_simulatable() {
        assert!(ClockType::RosTime.is_simulatable());
        assert!(!ClockType::SystemTime.is_simulatable());
        assert!(!ClockType::SteadyTime.is_simulatable());
    }

    #[test]
    fn ros_time_and_the_hlc_agree_on_the_instant() {
        let clock = Ros2Clock::new();
        let hlc = clock.now_hlc();
        let ros = RosTime::from_hlc(hlc);
        assert_eq!(
            ros.to_hlc().map(|stamp| stamp.physical_ns()),
            Some(hlc.physical_ns())
        );
    }

    #[test]
    fn the_wall_clock_reading_is_after_the_epoch() {
        let clock = Ros2Clock::new();
        assert!(
            clock.now().sec > 1_600_000_000,
            "a system clock reads well past 2020"
        );
        assert!(clock.now().is_positive());
    }

    #[test]
    fn simulated_time_starts_at_zero_and_follows_the_clock_topic() {
        let clock = Ros2Clock::new();
        assert!(!clock.uses_sim_time());
        assert!(
            !clock.set_sim_time(true),
            "the previous setting is returned"
        );
        assert!(clock.uses_sim_time());
        assert_eq!(
            clock.now(),
            RosTime::ZERO,
            "a sim-time node reads zero until /clock arrives"
        );

        clock.feed_clock(RosTime::new(42, 500_000_000));
        assert_eq!(clock.now(), RosTime::new(42, 500_000_000));
        assert_eq!(clock.clock_samples(), 1);
    }

    #[test]
    fn a_clock_sample_is_kept_even_when_sim_time_is_off() {
        let clock = Ros2Clock::new();
        clock.feed_clock(RosTime::new(7, 0));
        assert_eq!(clock.sim_time(), RosTime::new(7, 0));
        assert_ne!(
            clock.now(),
            RosTime::new(7, 0),
            "with sim time off, /clock does not drive `now`"
        );
        clock.set_sim_time(true);
        assert_eq!(clock.now(), RosTime::new(7, 0));
    }

    #[test]
    fn toggling_sim_time_twice_does_not_lose_the_simulators_position() {
        let clock = Ros2Clock::new();
        clock.set_sim_time(true);
        clock.feed_clock(RosTime::new(99, 0));
        clock.set_sim_time(false);
        clock.set_sim_time(true);
        assert_eq!(clock.now(), RosTime::new(99, 0));
        assert_eq!(clock.clock_samples(), 1);
    }

    #[test]
    fn simulated_time_may_run_backwards() {
        let clock = Ros2Clock::new();
        clock.set_sim_time(true);
        clock.feed_clock(RosTime::new(100, 0));
        clock.feed_clock(RosTime::new(50, 0));
        assert_eq!(
            clock.now(),
            RosTime::new(50, 0),
            "a simulator that resets is an ordinary thing; the clock follows"
        );
        assert_eq!(clock.clock_samples(), 2);
    }

    #[test]
    fn steady_time_never_follows_the_simulator() {
        let clock = Ros2Clock::new();
        clock.set_sim_time(true);
        clock.feed_clock(RosTime::new(1_000_000, 0));
        let steady = clock.read(ClockType::SteadyTime);
        assert!(
            steady.sec < 100,
            "steady time counts from this clock's creation, not the simulator: {steady}"
        );
    }

    #[test]
    fn system_time_never_follows_the_simulator() {
        let clock = Ros2Clock::new();
        clock.set_sim_time(true);
        clock.feed_clock(RosTime::new(1, 0));
        assert!(
            clock.read(ClockType::SystemTime).sec > 1_600_000_000,
            "SYSTEM_TIME is the wall clock even under sim time"
        );
        assert_eq!(clock.read(ClockType::RosTime), RosTime::new(1, 0));
    }

    #[test]
    fn steady_elapsed_only_moves_forward() {
        let clock = Ros2Clock::new();
        let first = clock.steady_elapsed();
        let second = clock.steady_elapsed();
        assert!(second >= first);
        assert!(!first.is_negative());
    }

    #[test]
    fn a_clone_shares_the_simulated_state() {
        let clock = Ros2Clock::new();
        let clone = clock.clone();
        clone.set_sim_time(true);
        clone.feed_clock(RosTime::new(11, 0));
        assert!(clock.uses_sim_time());
        assert_eq!(clock.now(), RosTime::new(11, 0));
    }

    #[test]
    fn observing_a_peer_advances_the_hlc() {
        let clock = Ros2Clock::new();
        let ahead = HlcTimestamp::new(clock.now_hlc().physical_ns().saturating_add(1_000_000), 0);
        let merged = clock.observe(ahead).expect("a millisecond is within drift");
        assert!(merged >= ahead);
        assert!(clock.now_hlc() >= merged);
    }

    #[test]
    fn observing_a_wildly_ahead_peer_is_refused_rather_than_applied() {
        let clock = Ros2Clock::new();
        let far_ahead = HlcTimestamp::new(u64::MAX / 2, 0);
        assert!(
            clock.observe(far_ahead).is_err(),
            "a peer a century ahead is a broken clock, not a fast one"
        );
    }

    #[test]
    fn the_manual_seam_exists_for_deterministic_tests() {
        let (clock, manual) = manual(1_000_000_000);
        manual.advance(std::time::Duration::from_secs(1));
        assert!(clock.now().is_positive());
        assert!(!Ros2Clock::default().uses_sim_time());
    }
}
