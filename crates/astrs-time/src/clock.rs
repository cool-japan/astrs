//! Monotonic/wall clock abstraction: the [`Clock`] trait plus a real
//! ([`SystemClock`]) and a fully controllable ([`ManualClock`]) source.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A source of both wall-clock and monotonic time.
///
/// [`crate::HlcClock`] reads [`Clock::now_wall_ns`] to derive HLC
/// timestamps; [`crate::Deadline`] and [`crate::TimerInterval`] are built
/// directly from [`std::time::Instant`] values, which callers typically
/// obtain from [`Clock::now_instant`] so the same clock source drives both
/// halves of a scheduler.
///
/// Implementors must be cheap to call — both methods are expected to sit on
/// the event-loop-rate hot path (once per message send / tick), not a
/// per-byte path.
pub trait Clock: Send + Sync + fmt::Debug {
    /// The current wall-clock reading, as nanoseconds since the UNIX epoch.
    ///
    /// Unlike [`Instant`], this is permitted to jump — forward (an NTP
    /// slew or step) or backward (a clock correction, leap-second
    /// handling, or a user changing the system clock). [`crate::HlcClock`]
    /// exists specifically to give a monotone output even when this input
    /// does not behave monotonically.
    fn now_wall_ns(&self) -> u64;

    /// The current monotonic instant.
    ///
    /// Unlike [`Clock::now_wall_ns`], this must never move backward between
    /// two calls on the same clock.
    fn now_instant(&self) -> Instant;
}

// `HlcClock<C>` is generic over `C: Clock` rather than holding `Arc<dyn
// Clock>` internally, so it never pays for dynamic dispatch on its own hot
// path (see `HlcClock`'s docs). But that means a caller who *does* want to
// share one clock instance across several `HlcClock`s (or other
// clock-driven components) — the common case in a daemon holding one
// `SystemClock` for the whole process — needs `Arc<C>` (or `Box<C>`) to
// itself implement `Clock`, forwarding through the pointer. Both blanket
// impls below make that work, including the `Arc<dyn Clock>` / `Box<dyn
// Clock>` trait-object case via `?Sized`.
impl<C: Clock + ?Sized> Clock for std::sync::Arc<C> {
    fn now_wall_ns(&self) -> u64 {
        (**self).now_wall_ns()
    }

    fn now_instant(&self) -> Instant {
        (**self).now_instant()
    }
}

impl<C: Clock + ?Sized> Clock for Box<C> {
    fn now_wall_ns(&self) -> u64 {
        (**self).now_wall_ns()
    }

    fn now_instant(&self) -> Instant {
        (**self).now_instant()
    }
}

/// A [`Clock`] backed by the real operating-system clocks:
/// [`SystemTime::now`] for wall time, [`Instant::now`] for monotonic time.
///
/// This is a zero-sized type; constructing one is free.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_wall_ns(&self) -> u64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            // Saturate rather than panic if the host clock is (absurdly)
            // set beyond the year ~2554 that u64 nanoseconds can express.
            Ok(elapsed) => u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX),
            // The host clock reads before the UNIX epoch. Treat that as
            // "as early as representable" rather than propagating an
            // error through every `Clock` caller for a misconfigured
            // system clock.
            Err(_) => 0,
        }
    }

    fn now_instant(&self) -> Instant {
        Instant::now()
    }
}

/// A fully controllable [`Clock`] for deterministic tests and for AstRS's
/// `--deterministic` replay mode (§14 of the blueprint), where the timer
/// wheel is driven from a recording's HLC stream instead of real time.
///
/// `ManualClock` tracks wall time and monotonic time as independent
/// counters, so a test can advance both together (the common case — time
/// simply passing) or step the wall-clock reading backward on its own via
/// [`ManualClock::rewind_wall`] while monotonic time keeps moving forward,
/// reproducing the NTP-step scenario [`crate::HlcClock`] is designed to
/// survive.
///
/// # Examples
///
/// ```
/// use astrs_time::{Clock, ManualClock};
/// use std::time::Duration;
///
/// let clock = ManualClock::new(1_000_000_000);
/// assert_eq!(clock.now_wall_ns(), 1_000_000_000);
///
/// clock.advance(Duration::from_secs(1));
/// assert_eq!(clock.now_wall_ns(), 2_000_000_000);
///
/// // Simulate an NTP step: wall time jumps backward, monotonic time does not.
/// let before = clock.now_instant();
/// clock.rewind_wall(Duration::from_secs(5)); // 2_000_000_000ns saturates to 0
/// assert_eq!(clock.now_wall_ns(), 0);
/// assert_eq!(clock.now_instant(), before);
/// ```
#[derive(Debug)]
pub struct ManualClock {
    wall_ns: AtomicU64,
    base: Instant,
    offset_ns: AtomicU64,
}

impl ManualClock {
    /// Creates a clock whose wall time starts at `start_wall_ns` nanoseconds
    /// since the UNIX epoch, and whose monotonic time starts at the instant
    /// of this call (offset zero).
    #[must_use]
    pub fn new(start_wall_ns: u64) -> Self {
        Self {
            wall_ns: AtomicU64::new(start_wall_ns),
            base: Instant::now(),
            offset_ns: AtomicU64::new(0),
        }
    }

    /// Sets the wall-clock reading to an absolute value, without touching
    /// monotonic time. Useful for jumping straight to a specific instant in
    /// a test rather than composing `advance`/`rewind_wall` calls.
    pub fn set_wall_ns(&self, wall_ns: u64) {
        self.wall_ns.store(wall_ns, Ordering::SeqCst);
    }

    /// Advances **both** wall time and monotonic time by `dur` — the
    /// ordinary case of simulated time simply passing. Saturates instead of
    /// wrapping if `dur` is large enough to overflow the internal counters.
    pub fn advance(&self, dur: Duration) {
        let ns = u64::try_from(dur.as_nanos()).unwrap_or(u64::MAX);
        let _ = self
            .wall_ns
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                Some(v.saturating_add(ns))
            });
        let _ = self
            .offset_ns
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                Some(v.saturating_add(ns))
            });
    }

    /// Steps the wall-clock reading backward by `dur` **without** moving
    /// monotonic time, reproducing a clock correction (NTP step, manual
    /// `date -s`, leap-second slew) that a well-behaved monotonic clock is
    /// immune to. Saturates at wall-time zero rather than underflowing.
    pub fn rewind_wall(&self, dur: Duration) {
        let ns = u64::try_from(dur.as_nanos()).unwrap_or(u64::MAX);
        let _ = self
            .wall_ns
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                Some(v.saturating_sub(ns))
            });
    }
}

impl Default for ManualClock {
    /// Starts the clock at the real current wall time (via [`SystemClock`]),
    /// with monotonic time zeroed at construction. Tests that need full,
    /// reproducible determinism should prefer [`ManualClock::new`] with an
    /// explicit starting value instead.
    fn default() -> Self {
        Self::new(SystemClock.now_wall_ns())
    }
}

impl Clock for ManualClock {
    fn now_wall_ns(&self) -> u64 {
        self.wall_ns.load(Ordering::SeqCst)
    }

    fn now_instant(&self) -> Instant {
        let offset = self.offset_ns.load(Ordering::SeqCst);
        // `checked_add` fails only if the offset is large enough to
        // overflow the platform's `Instant` representation — practically
        // unreachable (it would require simulating hundreds of years of
        // elapsed time), but falling back to `self.base` would return an
        // instant *earlier* than ones already handed out, breaking this
        // clock's own monotonicity contract. `Instant::now()` is always
        // `>= self.base` (real monotonic time only moves forward since
        // `self.base` was captured), so it is the safe forward-saturating
        // choice even though it under-represents the simulated offset in
        // this unreachable-in-practice branch.
        self.base
            .checked_add(Duration::from_nanos(offset))
            .unwrap_or_else(Instant::now)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_now_wall_ns_is_plausible() {
        // Sanity bound: any time after this crate's creation, before some
        // comfortably-future date, expressed in nanoseconds since epoch.
        let ns = SystemClock.now_wall_ns();
        let year_2020_ns: u64 = 1_577_836_800_000_000_000;
        let year_2100_ns: u64 = 4_102_444_800_000_000_000;
        assert!(ns > year_2020_ns);
        assert!(ns < year_2100_ns);
    }

    #[test]
    fn system_clock_now_instant_advances() {
        let a = SystemClock.now_instant();
        let b = SystemClock.now_instant();
        assert!(b >= a);
    }

    #[test]
    fn manual_clock_starts_at_configured_wall_time() {
        let clock = ManualClock::new(42);
        assert_eq!(clock.now_wall_ns(), 42);
    }

    #[test]
    fn manual_clock_advance_moves_both_clocks_forward() {
        let clock = ManualClock::new(1_000);
        let instant_before = clock.now_instant();
        clock.advance(Duration::from_millis(250));
        assert_eq!(clock.now_wall_ns(), 1_000 + 250_000_000);
        assert!(clock.now_instant() > instant_before);
        assert_eq!(
            clock.now_instant() - instant_before,
            Duration::from_millis(250)
        );
    }

    #[test]
    fn manual_clock_rewind_wall_does_not_move_monotonic_time() {
        let clock = ManualClock::new(10_000_000_000);
        let instant_before = clock.now_instant();
        clock.rewind_wall(Duration::from_secs(5));
        assert_eq!(clock.now_wall_ns(), 5_000_000_000);
        assert_eq!(clock.now_instant(), instant_before);
    }

    #[test]
    fn manual_clock_rewind_wall_saturates_at_zero() {
        let clock = ManualClock::new(1_000);
        clock.rewind_wall(Duration::from_secs(1));
        assert_eq!(clock.now_wall_ns(), 0);
    }

    #[test]
    fn manual_clock_set_wall_ns_is_absolute() {
        let clock = ManualClock::new(1_000);
        clock.advance(Duration::from_secs(1));
        clock.set_wall_ns(99);
        assert_eq!(clock.now_wall_ns(), 99);
    }

    #[test]
    fn manual_clock_default_seeds_from_real_wall_time() {
        let clock = ManualClock::default();
        let year_2020_ns: u64 = 1_577_836_800_000_000_000;
        assert!(clock.now_wall_ns() > year_2020_ns);
    }

    #[test]
    fn manual_clock_monotonic_time_never_goes_backward_across_calls() {
        let clock = ManualClock::new(0);
        let mut previous = clock.now_instant();
        for _ in 0..100 {
            clock.advance(Duration::from_nanos(1));
            let current = clock.now_instant();
            assert!(current >= previous);
            previous = current;
        }
    }

    #[test]
    fn clock_trait_is_object_safe() {
        let clocks: Vec<Box<dyn Clock>> =
            vec![Box::new(SystemClock), Box::new(ManualClock::new(0))];
        for c in &clocks {
            let _ = c.now_wall_ns();
            let _ = c.now_instant();
        }
    }

    #[test]
    fn arc_of_a_concrete_clock_implements_clock() {
        let clock: std::sync::Arc<ManualClock> = std::sync::Arc::new(ManualClock::new(1_000));
        assert_eq!(clock.now_wall_ns(), 1_000);
        // Sharing the same `Arc` across two independent readers observes
        // the same underlying clock (interior mutability through `Clock`
        // for `Arc<C>`, not a per-clone snapshot).
        let shared = std::sync::Arc::clone(&clock);
        clock.advance(Duration::from_secs(1));
        assert_eq!(shared.now_wall_ns(), 1_000 + 1_000_000_000);
    }

    #[test]
    fn arc_dyn_clock_implements_clock() {
        // The trait-object case specifically: `Arc<dyn Clock>`, which is
        // what a daemon holding one shared, type-erased clock source would
        // actually store.
        let clock: std::sync::Arc<dyn Clock> = std::sync::Arc::new(SystemClock);
        let _ = clock.now_wall_ns();
        let _ = clock.now_instant();
    }

    #[test]
    fn box_dyn_clock_implements_clock() {
        let clock: Box<dyn Clock> = Box::new(ManualClock::new(42));
        assert_eq!(clock.now_wall_ns(), 42);
    }

    #[test]
    fn hlc_clock_accepts_an_arc_wrapped_clock() {
        // The motivating use case: `HlcClock<C>` is generic, so it accepts
        // `Arc<SystemClock>` (or any `Arc<C: Clock>`) directly via the
        // blanket impl, letting a caller share one clock across several
        // `HlcClock`s (or other clock-driven components) without an extra
        // adapter type.
        let shared = std::sync::Arc::new(ManualClock::new(1_000));
        let hlc = crate::HlcClock::new(std::sync::Arc::clone(&shared));
        let a = hlc.now();
        shared.advance(Duration::from_nanos(1));
        let b = hlc.now();
        assert!(b > a);
    }
}
