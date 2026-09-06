//! [`Deadline`] — a monotonic-clock deadline with checked arithmetic.

use std::time::{Duration, Instant};

/// A point in monotonic time by which something must happen.
///
/// `Deadline` is deliberately built on [`std::time::Instant`], not
/// [`crate::HlcTimestamp`]: a deadline is a *local* scheduling concern
/// (timeouts, retry budgets, watchdog windows) that must never move
/// backward, whereas an `HlcTimestamp` is a *causal* concern meant to
/// travel across the wire. Mixing the two would let a wall-clock or
/// cross-host clock event perturb a purely local timeout.
///
/// Every query method takes an explicit `now: Instant` alongside a
/// `_now`-suffixed convenience that supplies [`Instant::now`] — the
/// explicit form lets callers drive a `Deadline` from a
/// [`crate::ManualClock`] (via [`crate::Clock::now_instant`]) for
/// deterministic tests, without ever needing to sleep in real time.
///
/// `Deadline` intentionally does **not** implement `serde`/`oxicode`
/// (de)serialization: an `Instant` is only meaningful within the process
/// that created it (it has no fixed epoch and cannot be reconstructed after
/// a restart or on another host), so there is no sound wire or storage
/// representation for one. Timeout *configuration* should instead be
/// carried as a [`Duration`] (see [`crate::parse_duration`]) and turned
/// into a `Deadline` locally, once, via [`Deadline::after`].
///
/// # Examples
///
/// ```
/// use astrs_time::Deadline;
/// use std::time::{Duration, Instant};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let now = Instant::now();
/// let deadline = Deadline::after(now, Duration::from_secs(10)).ok_or("overflow")?;
/// assert!(!deadline.is_expired(now));
/// assert_eq!(deadline.remaining(now), Duration::from_secs(10));
///
/// let later = now + Duration::from_secs(15);
/// assert!(deadline.is_expired(later));
/// assert_eq!(deadline.remaining(later), Duration::ZERO);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Deadline {
    target: Instant,
}

impl Deadline {
    /// Wraps an already-computed target instant.
    #[must_use]
    pub const fn new(target: Instant) -> Self {
        Self { target }
    }

    /// Builds a deadline `timeout` after `start`.
    ///
    /// Returns `None` only if `start + timeout` would overflow the
    /// platform's `Instant` representation — unreachable for any realistic
    /// timeout, but checked rather than left to panic (the behavior of
    /// `Instant`'s `Add` operator on overflow) since `timeout` may
    /// originate from user-supplied configuration (a manifest field, a CLI
    /// flag) that this crate cannot bound in advance.
    #[must_use]
    pub fn after(start: Instant, timeout: Duration) -> Option<Self> {
        start.checked_add(timeout).map(Self::new)
    }

    /// Builds a deadline `timeout` after the real current instant
    /// ([`Instant::now`]). See [`Deadline::after`] for the overflow case.
    #[must_use]
    pub fn from_now(timeout: Duration) -> Option<Self> {
        Self::after(Instant::now(), timeout)
    }

    /// The instant this deadline targets.
    #[must_use]
    pub const fn target(&self) -> Instant {
        self.target
    }

    /// Time remaining until this deadline, as measured from `now`.
    ///
    /// Returns [`Duration::ZERO`] if `now` is at or past the target,
    /// rather than underflowing or panicking.
    #[must_use]
    pub fn remaining(&self, now: Instant) -> Duration {
        self.target
            .checked_duration_since(now)
            .unwrap_or(Duration::ZERO)
    }

    /// [`Deadline::remaining`] measured from the real current instant.
    #[must_use]
    pub fn remaining_now(&self) -> Duration {
        self.remaining(Instant::now())
    }

    /// Whether `now` is at or past this deadline's target.
    #[must_use]
    pub fn is_expired(&self, now: Instant) -> bool {
        now >= self.target
    }

    /// [`Deadline::is_expired`] measured against the real current instant.
    #[must_use]
    pub fn is_expired_now(&self) -> bool {
        self.is_expired(Instant::now())
    }

    /// Returns a new deadline `extra` further out than this one.
    ///
    /// Returns `None` on the same (unreachable in practice) overflow
    /// condition as [`Deadline::after`], rather than panicking.
    #[must_use]
    pub fn checked_extend(&self, extra: Duration) -> Option<Self> {
        self.target.checked_add(extra).map(Self::new)
    }

    /// Returns a new deadline `less` closer than this one.
    ///
    /// Returns `None` if `less` would move the target before the earliest
    /// instant the platform can represent, rather than panicking.
    #[must_use]
    pub fn checked_shorten(&self, less: Duration) -> Option<Self> {
        self.target.checked_sub(less).map(Self::new)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn after_computes_target() {
        let now = Instant::now();
        let deadline = Deadline::after(now, Duration::from_secs(5)).unwrap();
        assert_eq!(deadline.target(), now + Duration::from_secs(5));
    }

    #[test]
    fn remaining_counts_down_and_floors_at_zero() {
        let now = Instant::now();
        let deadline = Deadline::after(now, Duration::from_secs(10)).unwrap();
        assert_eq!(deadline.remaining(now), Duration::from_secs(10));
        assert_eq!(
            deadline.remaining(now + Duration::from_secs(3)),
            Duration::from_secs(7)
        );
        assert_eq!(
            deadline.remaining(now + Duration::from_secs(10)),
            Duration::ZERO
        );
        assert_eq!(
            deadline.remaining(now + Duration::from_secs(999)),
            Duration::ZERO
        );
    }

    #[test]
    fn is_expired_is_inclusive_of_the_exact_target() {
        let now = Instant::now();
        let deadline = Deadline::after(now, Duration::from_secs(1)).unwrap();
        assert!(!deadline.is_expired(now));
        assert!(deadline.is_expired(now + Duration::from_secs(1)));
        assert!(deadline.is_expired(now + Duration::from_secs(2)));
    }

    #[test]
    fn checked_extend_moves_target_forward() {
        let now = Instant::now();
        let deadline = Deadline::after(now, Duration::from_secs(5)).unwrap();
        let extended = deadline.checked_extend(Duration::from_secs(5)).unwrap();
        assert_eq!(extended.target(), now + Duration::from_secs(10));
    }

    #[test]
    fn checked_shorten_moves_target_backward() {
        let now = Instant::now();
        let deadline = Deadline::after(now, Duration::from_secs(10)).unwrap();
        let shortened = deadline.checked_shorten(Duration::from_secs(4)).unwrap();
        assert_eq!(shortened.target(), now + Duration::from_secs(6));
    }

    #[test]
    fn from_now_targets_the_future() {
        let before = Instant::now();
        let deadline = Deadline::from_now(Duration::from_secs(1)).unwrap();
        assert!(deadline.target() > before);
        assert!(!deadline.is_expired_now());
    }

    #[test]
    fn ordering_matches_target_ordering() {
        let now = Instant::now();
        let sooner = Deadline::new(now + Duration::from_secs(1));
        let later = Deadline::new(now + Duration::from_secs(2));
        assert!(sooner < later);
    }

    #[test]
    fn new_wraps_an_arbitrary_instant() {
        let now = Instant::now();
        let deadline = Deadline::new(now);
        assert_eq!(deadline.target(), now);
        assert!(deadline.is_expired(now));
    }
}
