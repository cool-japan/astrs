//! [`HlcClock`] — a hybrid logical clock that issues [`HlcTimestamp`]s which
//! are monotone even when the underlying wall clock is not.

use std::sync::Mutex;
use std::time::Duration;

use crate::clock::{Clock, SystemClock};
use crate::stamped::Stamped;
use crate::timestamp::HlcTimestamp;

/// The default maximum acceptable clock drift between peers for
/// [`HlcClock::update_with`]: 500 milliseconds.
///
/// This bounds *clock skew between peers*, a different concept from the
/// scheduling drift [`crate::TimerInterval`] avoids — see that type's docs
/// for the distinction.
pub const DEFAULT_MAX_CLOCK_DRIFT: Duration = Duration::from_millis(500);

/// Error returned by [`HlcClock::update_with`] when a remote timestamp's
/// physical-time component is further ahead of the local wall clock than
/// the configured maximum drift allows.
///
/// This bound guards only against a remote clock that is too far in the
/// **future**: a remote timestamp that is behind the local wall clock is
/// always accepted, because merging in an old value can only leave the
/// local clock unchanged (via the `max` in the receive rule), never move it
/// backward. A remote clock that is far in the future, left unchecked,
/// could otherwise drag the entire cluster's logical time forward on the
/// word of one misbehaving or unsynchronized peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "remote clock drift of {drift:?} exceeds the configured maximum {max_drift:?} \
     (local wall clock {local_wall_ns} ns since epoch, remote physical time {remote_physical_ns} ns since epoch)"
)]
pub struct HlcDriftError {
    /// The local wall-clock reading, in nanoseconds since the UNIX epoch,
    /// at the moment the remote timestamp was checked.
    pub local_wall_ns: u64,
    /// The rejected remote timestamp's physical-time component, in
    /// nanoseconds since the UNIX epoch.
    pub remote_physical_ns: u64,
    /// How far the remote timestamp is ahead of the local wall clock.
    pub drift: Duration,
    /// The configured maximum acceptable drift that was exceeded.
    pub max_drift: Duration,
}

/// Errors raised by [`HlcClock`].
///
/// This wraps a single variant today; it stays an enum (rather than a bare
/// struct) so future receive-rule failure modes can be added without an
/// API break, matching the append-only evolution the blueprint (§3.4)
/// requires of every AstRS wire and error surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HlcError {
    /// A remote timestamp was rejected for exceeding the configured
    /// maximum clock drift. See [`HlcDriftError`].
    #[error(transparent)]
    DriftExceeded(#[from] HlcDriftError),
}

/// Returns the timestamp immediately following `(physical, logical)`:
/// `(physical, logical + 1)`, or `(physical + 1, 0)` if the logical counter
/// would overflow.
///
/// Overflowing `u32::MAX` ties on a single physical nanosecond is not
/// reachable by any real workload (it would require four billion `now()` or
/// `update_with()` calls landing on the same nanosecond), but handling it by
/// "borrowing" a nanosecond from the future — rather than erroring or
/// panicking — keeps [`HlcClock::now`] total and infallible, and remains
/// self-consistent: later calls compare the real wall clock against this
/// synthetically-advanced value and keep deriving further logical bumps
/// from it until real time catches up.
///
/// At the true ceiling of the type — [`HlcTimestamp::MAX`], where *both*
/// components are already exhausted — there is no larger `HlcTimestamp` to
/// borrow into. Returning `(physical + 1, 0)` there would wrap `physical`
/// back down via `saturating_add` while resetting `logical` to zero,
/// producing a result *smaller* than the input and breaking monotonicity.
/// Pinning at [`HlcTimestamp::MAX`] instead is the only sound behavior once
/// the 96-bit space is exhausted: the clock stops advancing, but it never
/// moves backward.
fn successor(physical: u64, logical: u32) -> HlcTimestamp {
    match logical.checked_add(1) {
        Some(l) => HlcTimestamp::new(physical, l),
        None => match physical.checked_add(1) {
            Some(p) => HlcTimestamp::new(p, 0),
            None => HlcTimestamp::MAX,
        },
    }
}

/// A hybrid logical clock (HLC): a clock that combines a wall-clock reading
/// with a logical counter so that every timestamp it issues is (a) close to
/// wall-clock time and (b) strictly increasing, even across a wall-clock
/// step backward or a burst of calls landing on the same physical
/// nanosecond.
///
/// # Algorithm
///
/// [`HlcClock::now`] implements the standard HLC "send" rule: if the wall
/// clock has advanced past the last issued timestamp, physical time wins
/// and the logical counter resets to zero; otherwise the physical time is
/// held at its previous value and the logical counter is bumped.
/// [`HlcClock::update_with`] implements the "receive" rule (Kulkarni et
/// al., *Logical Physical Clocks*): the new physical time is
/// `max(wall clock, local, remote)`, and the logical counter is bumped from
/// whichever of local/remote/neither tied for that maximum.
///
/// # Concurrency
///
/// Internally, `HlcClock` guards its state with a [`Mutex`] rather than a
/// lock-free atomic. This is a deliberate choice, not an oversight: the
/// 96-bit `(physical_ns, logical)` pair does not fit in any atomic type
/// stable Rust provides (there is no `AtomicU128`), and the read-max-write
/// sequence in both `now` and `update_with` must be atomic as a whole to
/// stay correct. A short critical section around an event-loop-rate
/// operation (once per message send, not a per-byte path — §3 of the
/// blueprint) is not a bottleneck, so correctness was chosen over a
/// lock-free fast path (a genuinely faster design is possible but was
/// judged not worth the added risk here — see [`crate::HlcTimestamp`]'s
/// docs for the layout tradeoff this follows from). A poisoned lock (a
/// panic while holding it, which none of this crate's own code can cause)
/// is recovered via [`std::sync::PoisonError::into_inner`] rather than
/// propagated, since a stale-but-valid `HlcTimestamp` is always safe to
/// keep using — every subsequent operation only ever moves it forward.
///
/// # Examples
///
/// ```
/// use astrs_time::{HlcClock, ManualClock};
///
/// let clock = HlcClock::new(ManualClock::new(1_000));
/// let a = clock.now();
/// let b = clock.now();
/// assert!(b > a, "now() is strictly increasing even with a frozen wall clock");
/// ```
#[derive(Debug)]
pub struct HlcClock<C: Clock = SystemClock> {
    clock: C,
    state: Mutex<HlcTimestamp>,
    max_drift: Duration,
}

impl HlcClock<SystemClock> {
    /// Builds a clock backed by the real system clock
    /// ([`SystemClock`]), with the default 500ms maximum drift
    /// ([`DEFAULT_MAX_CLOCK_DRIFT`]).
    #[must_use]
    pub fn system() -> Self {
        Self::new(SystemClock)
    }
}

impl<C: Clock> HlcClock<C> {
    /// Builds a clock over the given time source, with the default 500ms
    /// maximum drift ([`DEFAULT_MAX_CLOCK_DRIFT`]).
    #[must_use]
    pub fn new(clock: C) -> Self {
        Self::with_max_drift(clock, DEFAULT_MAX_CLOCK_DRIFT)
    }

    /// Builds a clock over the given time source with an explicit maximum
    /// acceptable drift for [`HlcClock::update_with`].
    #[must_use]
    pub fn with_max_drift(clock: C, max_drift: Duration) -> Self {
        Self {
            clock,
            state: Mutex::new(HlcTimestamp::EPOCH),
            max_drift,
        }
    }

    /// The configured maximum acceptable clock drift.
    #[must_use]
    pub fn max_drift(&self) -> Duration {
        self.max_drift
    }

    /// Replaces the configured maximum acceptable clock drift.
    pub fn set_max_drift(&mut self, max_drift: Duration) {
        self.max_drift = max_drift;
    }

    /// Borrows the underlying time source.
    #[must_use]
    pub fn clock(&self) -> &C {
        &self.clock
    }

    /// The highest timestamp this clock has issued or merged so far,
    /// without advancing it.
    ///
    /// Useful for read-only inspection (metrics export, the TUI timeline)
    /// without perturbing the clock the way [`HlcClock::now`] would.
    #[must_use]
    pub fn last(&self) -> HlcTimestamp {
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Issues a new timestamp: the HLC "send" rule.
    ///
    /// Always strictly greater than every timestamp previously returned by
    /// `now` or accepted by `update_with` on this clock, regardless of how
    /// the underlying wall clock behaves — including a wall clock that
    /// stands still, runs backward, or repeats a reading.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_time::{Clock, HlcClock, ManualClock};
    ///
    /// let wall = ManualClock::new(1_000);
    /// let clock = HlcClock::new(wall);
    /// let a = clock.now();
    ///
    /// // Wall clock steps backward (e.g. an NTP correction) ...
    /// clock.clock().rewind_wall(std::time::Duration::from_secs(1));
    /// // ... yet the HLC output still strictly increases.
    /// let b = clock.now();
    /// assert!(b > a);
    /// ```
    pub fn now(&self) -> HlcTimestamp {
        let wall_ns = self.clock.now_wall_ns();
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let new = if wall_ns > guard.physical_ns() {
            HlcTimestamp::new(wall_ns, 0)
        } else {
            successor(guard.physical_ns(), guard.logical())
        };
        *guard = new;
        new
    }

    /// Merges a timestamp received from a peer: the HLC "receive" rule.
    ///
    /// Returns the merged timestamp — strictly greater than both the local
    /// clock's previous state and `remote` — and advances the local clock
    /// to it. Returns [`HlcError::DriftExceeded`] without changing any
    /// local state if `remote`'s physical-time component is further ahead
    /// of the local wall clock than [`HlcClock::max_drift`] allows.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_time::{HlcClock, HlcTimestamp, ManualClock};
    /// # fn main() -> Result<(), astrs_time::HlcError> {
    /// let clock = HlcClock::new(ManualClock::new(1_000));
    /// let remote = HlcTimestamp::new(1_000, 41);
    /// let merged = clock.update_with(remote)?;
    /// assert!(merged > remote);
    /// # Ok(())
    /// # }
    /// ```
    pub fn update_with(&self, remote: HlcTimestamp) -> Result<HlcTimestamp, HlcError> {
        let wall_ns = self.clock.now_wall_ns();
        let drift_ns = remote.physical_ns().saturating_sub(wall_ns);
        let max_drift_ns = u64::try_from(self.max_drift.as_nanos()).unwrap_or(u64::MAX);
        if drift_ns > max_drift_ns {
            return Err(HlcDriftError {
                local_wall_ns: wall_ns,
                remote_physical_ns: remote.physical_ns(),
                drift: Duration::from_nanos(drift_ns),
                max_drift: self.max_drift,
            }
            .into());
        }

        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let local = *guard;
        let l_new = wall_ns.max(local.physical_ns()).max(remote.physical_ns());
        let new = if l_new == local.physical_ns() && l_new == remote.physical_ns() {
            successor(l_new, local.logical().max(remote.logical()))
        } else if l_new == local.physical_ns() {
            successor(l_new, local.logical())
        } else if l_new == remote.physical_ns() {
            successor(l_new, remote.logical())
        } else {
            HlcTimestamp::new(l_new, 0)
        };
        *guard = new;
        Ok(new)
    }

    /// Stamps `inner` with a fresh timestamp from [`HlcClock::now`].
    ///
    /// A convenience for the pattern the blueprint's merged event loop
    /// (§4.3) uses throughout: every event is a [`Stamped<T>`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_time::HlcClock;
    ///
    /// let clock = HlcClock::system();
    /// let event = clock.stamp("hello");
    /// assert_eq!(event.inner, "hello");
    /// ```
    #[must_use]
    pub fn stamp<T>(&self, inner: T) -> Stamped<T> {
        Stamped::new(self.now(), inner)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;

    #[test]
    fn now_is_monotone_with_frozen_wall_clock() {
        let clock = HlcClock::new(ManualClock::new(1_000));
        let mut previous = clock.now();
        for _ in 0..1_000 {
            let current = clock.now();
            assert!(current > previous);
            previous = current;
        }
    }

    #[test]
    fn now_takes_wall_clock_when_it_advances() {
        let wall = ManualClock::new(1_000);
        let clock = HlcClock::new(wall);
        let a = clock.now();
        assert_eq!(a, HlcTimestamp::new(1_000, 0));
        clock.clock().advance(Duration::from_nanos(1));
        let b = clock.now();
        assert_eq!(b, HlcTimestamp::new(1_001, 0));
    }

    #[test]
    fn now_bumps_logical_when_wall_clock_stands_still() {
        let clock = HlcClock::new(ManualClock::new(1_000));
        let a = clock.now();
        let b = clock.now();
        assert_eq!(a, HlcTimestamp::new(1_000, 0));
        assert_eq!(b, HlcTimestamp::new(1_000, 1));
    }

    #[test]
    fn now_is_monotone_across_a_wall_clock_step_backward() {
        let wall = ManualClock::new(10_000);
        let clock = HlcClock::new(wall);
        let a = clock.now();
        clock.clock().rewind_wall(Duration::from_nanos(9_999));
        let b = clock.now();
        assert!(b > a);
        assert_eq!(b, HlcTimestamp::new(10_000, 1));
    }

    #[test]
    fn update_with_takes_max_of_local_remote_and_wall() {
        let clock = HlcClock::new(ManualClock::new(1_000));
        let remote = HlcTimestamp::new(5_000, 3);
        let merged = clock.update_with(remote).unwrap();
        assert_eq!(merged, HlcTimestamp::new(5_000, 4));
        assert_eq!(clock.last(), merged);
    }

    #[test]
    fn update_with_bumps_local_logical_when_local_dominates() {
        let clock = HlcClock::new(ManualClock::new(1_000));
        let _ = clock.now(); // local state becomes (1000, 0)
        let remote = HlcTimestamp::new(500, 99);
        let merged = clock.update_with(remote).unwrap();
        assert_eq!(merged, HlcTimestamp::new(1_000, 1));
    }

    #[test]
    fn update_with_ties_take_max_logical_plus_one() {
        let clock = HlcClock::new(ManualClock::new(1_000));
        let _ = clock.now(); // local state becomes (1000, 0)
        let remote = HlcTimestamp::new(1_000, 41);
        let merged = clock.update_with(remote).unwrap();
        assert_eq!(merged, HlcTimestamp::new(1_000, 42));
    }

    #[test]
    fn update_with_never_moves_backward() {
        let clock = HlcClock::new(ManualClock::new(1_000));
        let a = clock.now();
        let stale_remote = HlcTimestamp::new(1, 0);
        let merged = clock.update_with(stale_remote).unwrap();
        assert!(merged > a);
    }

    #[test]
    fn update_with_rejects_excessive_future_drift() {
        let clock = HlcClock::with_max_drift(ManualClock::new(1_000), Duration::from_millis(500));
        let far_future = HlcTimestamp::new(1_000 + 501_000_000, 0);
        let err = clock.update_with(far_future).unwrap_err();
        assert!(matches!(err, HlcError::DriftExceeded(_)));
    }

    #[test]
    fn update_with_accepts_drift_at_exactly_the_boundary() {
        let clock = HlcClock::with_max_drift(ManualClock::new(1_000), Duration::from_millis(500));
        let at_boundary = HlcTimestamp::new(1_000 + 500_000_000, 0);
        assert!(clock.update_with(at_boundary).is_ok());
    }

    #[test]
    fn update_with_does_not_mutate_state_on_rejection() {
        let clock = HlcClock::with_max_drift(ManualClock::new(1_000), Duration::from_millis(1));
        let before = clock.last();
        let far_future = HlcTimestamp::new(1_000 + 2_000_000, 0);
        assert!(clock.update_with(far_future).is_err());
        assert_eq!(clock.last(), before);
    }

    #[test]
    fn update_with_accepts_arbitrarily_stale_remotes() {
        let clock = HlcClock::with_max_drift(
            ManualClock::new(1_000_000_000_000),
            Duration::from_millis(1),
        );
        // A remote timestamp from the deep past is not "drift" in the
        // sense this bound guards against; it must always be accepted.
        let ancient = HlcTimestamp::new(1, 0);
        assert!(clock.update_with(ancient).is_ok());
    }

    #[test]
    fn default_max_drift_is_500ms() {
        let clock: HlcClock<ManualClock> = HlcClock::new(ManualClock::new(0));
        assert_eq!(clock.max_drift(), Duration::from_millis(500));
        assert_eq!(DEFAULT_MAX_CLOCK_DRIFT, Duration::from_millis(500));
    }

    #[test]
    fn set_max_drift_takes_effect() {
        let mut clock = HlcClock::new(ManualClock::new(1_000));
        clock.set_max_drift(Duration::from_secs(1));
        assert_eq!(clock.max_drift(), Duration::from_secs(1));
    }

    #[test]
    fn successor_borrows_a_nanosecond_on_logical_overflow() {
        assert_eq!(successor(10, u32::MAX), HlcTimestamp::new(11, 0));
        assert_eq!(successor(10, 5), HlcTimestamp::new(10, 6));
    }

    #[test]
    fn successor_pins_at_the_absolute_maximum_instead_of_wrapping_backward() {
        // Both components already exhausted: there is no larger value to
        // borrow into, so the only sound result is the ceiling itself.
        assert_eq!(successor(u64::MAX, u32::MAX), HlcTimestamp::MAX);
        // One step below the ceiling still borrows normally.
        assert_eq!(successor(u64::MAX, u32::MAX - 1), HlcTimestamp::MAX);
        assert_eq!(
            successor(u64::MAX - 1, u32::MAX),
            HlcTimestamp::new(u64::MAX, 0)
        );
    }

    /// End-to-end regression test for the same ceiling-pinning behavior,
    /// exercised through the public `HlcClock` API (not just the private
    /// `successor` helper directly): a clock whose wall time is pinned at
    /// `u64::MAX` and whose state is driven to `HlcTimestamp::MAX` via
    /// `update_with` must never subsequently move backward.
    #[test]
    fn clock_pins_at_absolute_maximum_rather_than_wrapping_backward() {
        let clock = HlcClock::with_max_drift(ManualClock::new(u64::MAX), Duration::MAX);

        let seeded = clock.now();
        assert_eq!(seeded, HlcTimestamp::new(u64::MAX, 0));

        // Merge in a remote timestamp that ties on `physical_ns` and is one
        // below the maximum logical value, driving the clock to the exact
        // ceiling in a single accepted merge.
        let remote = HlcTimestamp::new(u64::MAX, u32::MAX - 1);
        let merged = clock.update_with(remote).unwrap();
        assert_eq!(merged, HlcTimestamp::MAX);

        // Further calls must not go backward, even though there is no
        // larger value left to advance to.
        let next = clock.now();
        assert!(next >= merged);
        assert_eq!(next, HlcTimestamp::MAX);
    }

    #[test]
    fn stamp_uses_now() {
        let clock = HlcClock::new(ManualClock::new(1_000));
        let stamped = clock.stamp(7u32);
        assert_eq!(stamped.inner, 7);
        assert_eq!(stamped.ts, clock.last());
    }

    #[test]
    fn system_convenience_constructor_works() {
        let clock = HlcClock::system();
        let a = clock.now();
        let b = clock.now();
        assert!(b > a);
    }
}
