//! [`TimerInterval`] — drift-free periodic scheduling for `astrs/timer/*`
//! virtual sources (blueprint §8.4).

use std::time::{Duration, Instant};

/// Converts a nanosecond count computed as a `u128` (as every tick offset
/// in this module is, to avoid overflow while multiplying a tick index by a
/// period) into a [`Duration`], saturating at [`Duration::MAX`] rather than
/// panicking.
///
/// This deliberately goes through [`Duration::new`] (seconds + subsec
/// nanos) rather than [`Duration::from_nanos`] (a bare `u64` nanosecond
/// count): `Duration::from_nanos`'s `u64` parameter tops out around 584
/// years, far short of `Duration`'s own ~584-*billion*-year range, so
/// converting through it would silently truncate any offset beyond ~584
/// years even though both the source `u128` and the destination `Duration`
/// can represent it exactly.
fn duration_from_nanos_u128(nanos: u128) -> Duration {
    let secs_u128 = nanos / 1_000_000_000;
    // `nanos % 1_000_000_000` is always in `0..1_000_000_000`, which fits a `u32`.
    let subsec_nanos = (nanos % 1_000_000_000) as u32;
    match u64::try_from(secs_u128) {
        Ok(secs) => Duration::new(secs, subsec_nanos),
        Err(_) => Duration::MAX,
    }
}

/// Error building a [`TimerInterval`].
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum TimerIntervalError {
    /// The requested period was zero. A zero-length repeating interval is
    /// not meaningful (it would tick infinitely often), so it is rejected
    /// at construction rather than producing a timer that divides by zero
    /// the first time it is scheduled.
    #[error("timer interval period must be non-zero")]
    ZeroPeriod,
    /// The requested rate was not finite and positive, or its reciprocal
    /// could not be represented as a [`Duration`].
    #[error("invalid timer rate {0} Hz (must be finite and positive)")]
    InvalidHz(f64),
}

/// Error parsing an `astrs/timer/*` virtual source path (blueprint §8.4:
/// `astrs/timer/millis/N`, `astrs/timer/secs/N`, `astrs/timer/hz/N`).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TimerSourcePathError {
    /// The path did not start with `astrs/timer/`.
    #[error("timer virtual source path {0:?} does not start with \"astrs/timer/\"")]
    NotATimerPath(String),
    /// The unit segment (the part after `astrs/timer/`) was not `millis`,
    /// `secs`, or `hz`.
    #[error(
        "timer virtual source path {0:?} has an unrecognized unit segment {1:?} (expected millis, secs, or hz)"
    )]
    UnknownUnit(String, String),
    /// The path was missing its `N` segment (or the unit segment itself).
    #[error("timer virtual source path {0:?} is missing its unit or N segment")]
    MissingValue(String),
    /// The path had extra `/`-separated segments after `N`.
    #[error("timer virtual source path {0:?} has trailing segments after N")]
    TrailingSegments(String),
    /// The `N` segment was not a valid number for its unit.
    #[error("timer virtual source path {0:?} has an invalid value {1:?}: {2}")]
    InvalidValue(String, String, String),
    /// `N` parsed, but was rejected by [`TimerInterval`]'s own validation
    /// (e.g. `astrs/timer/millis/0`).
    #[error(transparent)]
    Interval(#[from] TimerIntervalError),
}

/// A drift-free periodic schedule: `astrs/timer/millis/N`,
/// `astrs/timer/secs/N`, and `astrs/timer/hz/N` virtual sources (blueprint
/// §8.4) all resolve to one of these.
///
/// # Drift-free absolute scheduling
///
/// A naive repeating timer computed as `sleep(period)` in a loop
/// accumulates **scheduling drift**: every cycle's processing time (however
/// small) delays the next `sleep` call, so the *n*-th tick lands later and
/// later relative to where it "should" be. `TimerInterval` avoids this by
/// scheduling against a fixed absolute grid — `anchor`, `anchor + period`,
/// `anchor + 2*period`, ... — instead of relative to the last tick, so a
/// single late wakeup never compounds into the next one.
///
/// This is a different concept from the *clock drift* [`crate::HlcClock`]
/// bounds ([`crate::HlcClock::max_drift`]): that is skew between two
/// different clocks (peers' wall clocks disagreeing); this is skew between
/// a single clock's intended and actual schedule.
///
/// # The anchor lifecycle
///
/// [`TimerInterval::from_millis`]/[`from_secs`](Self::from_secs)/
/// [`from_hz`](Self::from_hz)/[`from_virtual_source_path`](Self::from_virtual_source_path)
/// all anchor the tick grid at their own call time — a reasonable default
/// for direct, ad-hoc use. But a manifest's `astrs/timer/*` source is
/// typically parsed once at `astrs validate` time, well before the
/// dataflow actually starts running; anchoring there would make the first
/// computed tick already be in the past by the time the graph starts. The
/// intended production flow is: parse the manifest path early (to validate
/// it and read `period()`), then call [`TimerInterval::rebase`] with the
/// instant the dataflow/node actually starts, before scheduling any ticks.
///
/// # Examples
///
/// ```
/// use astrs_time::TimerInterval;
/// use std::time::{Duration, Instant};
/// # fn main() -> Result<(), astrs_time::TimerIntervalError> {
/// let anchor = Instant::now();
/// let timer = TimerInterval::with_anchor(Duration::from_millis(100), anchor)?;
///
/// let t1 = timer.next_tick(anchor);
/// assert_eq!(t1, anchor + Duration::from_millis(100));
///
/// // A caller that only notices 250ms after the anchor still gets the
/// // *next grid point* (300ms), not `late + period` (350ms) — a late
/// // wakeup never pushes later ticks further out.
/// let late = anchor + Duration::from_millis(250);
/// let t2 = timer.next_tick(late);
/// assert_eq!(t2, anchor + Duration::from_millis(300));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerInterval {
    period: Duration,
    anchor: Instant,
}

impl TimerInterval {
    /// Builds a timer with the given period, anchored at `anchor`.
    ///
    /// # Errors
    ///
    /// Returns [`TimerIntervalError::ZeroPeriod`] if `period` is zero.
    pub fn with_anchor(period: Duration, anchor: Instant) -> Result<Self, TimerIntervalError> {
        if period.is_zero() {
            return Err(TimerIntervalError::ZeroPeriod);
        }
        Ok(Self { period, anchor })
    }

    /// Builds a timer with the given period, anchored at the current
    /// instant ([`Instant::now`]). See the type docs' "anchor lifecycle"
    /// section for why production schedulers should [`rebase`](Self::rebase)
    /// before use.
    ///
    /// # Errors
    ///
    /// Returns [`TimerIntervalError::ZeroPeriod`] if `period` is zero.
    pub fn from_period(period: Duration) -> Result<Self, TimerIntervalError> {
        Self::with_anchor(period, Instant::now())
    }

    /// Builds a timer with a `ms`-millisecond period.
    ///
    /// # Errors
    ///
    /// Returns [`TimerIntervalError::ZeroPeriod`] if `ms` is zero.
    pub fn from_millis(ms: u64) -> Result<Self, TimerIntervalError> {
        Self::from_period(Duration::from_millis(ms))
    }

    /// Builds a timer with a `secs`-second period.
    ///
    /// # Errors
    ///
    /// Returns [`TimerIntervalError::ZeroPeriod`] if `secs` is zero.
    pub fn from_secs(secs: u64) -> Result<Self, TimerIntervalError> {
        Self::from_period(Duration::from_secs(secs))
    }

    /// Builds a timer that ticks `hz` times per second.
    ///
    /// # Errors
    ///
    /// Returns [`TimerIntervalError::InvalidHz`] if `hz` is not finite and
    /// positive, or if `1.0 / hz` cannot be represented as a [`Duration`]
    /// (e.g. `hz` too close to zero). This never reaches the panicking
    /// path of [`Duration::from_secs_f64`]: the reciprocal is built with
    /// [`Duration::try_from_secs_f64`], which validates instead of
    /// panicking.
    pub fn from_hz(hz: f64) -> Result<Self, TimerIntervalError> {
        if !hz.is_finite() || hz <= 0.0 {
            return Err(TimerIntervalError::InvalidHz(hz));
        }
        let period =
            Duration::try_from_secs_f64(1.0 / hz).map_err(|_| TimerIntervalError::InvalidHz(hz))?;
        Self::from_period(period)
    }

    /// Parses one of the `astrs/timer/*` virtual source paths named in the
    /// blueprint (§8.4): `astrs/timer/millis/N`, `astrs/timer/secs/N`,
    /// `astrs/timer/hz/N`.
    ///
    /// This intentionally does not also accept a `dora/timer/...` form;
    /// mapping dora's timer paths onto AstRS's is `astrs-migrate`'s job
    /// (§8.6), not this crate's.
    ///
    /// # Errors
    ///
    /// Returns [`TimerSourcePathError`] if `path` does not start with
    /// `astrs/timer/`, names an unrecognized unit segment, is missing its
    /// `N` segment, has trailing segments after `N`, `N` fails to parse, or
    /// `N` fails [`TimerInterval`]'s own validation (e.g.
    /// `astrs/timer/millis/0`).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_time::TimerInterval;
    /// # fn main() -> Result<(), astrs_time::TimerSourcePathError> {
    /// let timer = TimerInterval::from_virtual_source_path("astrs/timer/millis/250")?;
    /// assert_eq!(timer.period(), std::time::Duration::from_millis(250));
    ///
    /// let by_rate = TimerInterval::from_virtual_source_path("astrs/timer/hz/10")?;
    /// assert_eq!(by_rate.period(), std::time::Duration::from_millis(100));
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_virtual_source_path(path: &str) -> Result<Self, TimerSourcePathError> {
        let rest = path
            .strip_prefix("astrs/timer/")
            .ok_or_else(|| TimerSourcePathError::NotATimerPath(path.to_owned()))?;
        let mut segments = rest.split('/');
        let unit = segments
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| TimerSourcePathError::MissingValue(path.to_owned()))?;
        let value = segments
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| TimerSourcePathError::MissingValue(path.to_owned()))?;
        if segments.next().is_some() {
            return Err(TimerSourcePathError::TrailingSegments(path.to_owned()));
        }

        match unit {
            "millis" => {
                let ms: u64 = value.parse().map_err(|e: std::num::ParseIntError| {
                    TimerSourcePathError::InvalidValue(
                        path.to_owned(),
                        value.to_owned(),
                        e.to_string(),
                    )
                })?;
                Ok(Self::from_millis(ms)?)
            }
            "secs" => {
                let secs: u64 = value.parse().map_err(|e: std::num::ParseIntError| {
                    TimerSourcePathError::InvalidValue(
                        path.to_owned(),
                        value.to_owned(),
                        e.to_string(),
                    )
                })?;
                Ok(Self::from_secs(secs)?)
            }
            "hz" => {
                let hz: f64 = value.parse().map_err(|e: std::num::ParseFloatError| {
                    TimerSourcePathError::InvalidValue(
                        path.to_owned(),
                        value.to_owned(),
                        e.to_string(),
                    )
                })?;
                Ok(Self::from_hz(hz)?)
            }
            other => Err(TimerSourcePathError::UnknownUnit(
                path.to_owned(),
                other.to_owned(),
            )),
        }
    }

    /// Re-anchors this timer's tick grid at `anchor`, keeping the same
    /// period. See the type docs' "anchor lifecycle" section.
    #[must_use]
    pub fn rebase(&self, anchor: Instant) -> Self {
        Self {
            period: self.period,
            anchor,
        }
    }

    /// The configured period.
    #[must_use]
    pub const fn period(&self) -> Duration {
        self.period
    }

    /// The instant the tick grid is anchored at.
    #[must_use]
    pub const fn anchor(&self) -> Instant {
        self.anchor
    }

    /// The configured rate, in ticks per second (the inverse of
    /// [`TimerInterval::period`]).
    #[must_use]
    pub fn hz(&self) -> f64 {
        1.0 / self.period.as_secs_f64()
    }

    /// The smallest scheduled grid point strictly greater than `after`.
    ///
    /// Grid points are `anchor + k * period` for `k = 0, 1, 2, ...`. Calling
    /// this repeatedly with each result — `let mut t = timer.next_tick(now);
    /// loop { sleep_until(t); t = timer.next_tick(t); }` — walks the
    /// schedule forward without drift (see the type docs).
    #[must_use]
    pub fn next_tick(&self, after: Instant) -> Instant {
        let elapsed = match after.checked_duration_since(self.anchor) {
            Some(d) => d,
            // `after` precedes the anchor: the first grid point (the
            // anchor itself) is already strictly after `after`.
            None => return self.anchor,
        };
        let period_ns = self.period.as_nanos().max(1);
        let ticks_elapsed = elapsed.as_nanos() / period_ns;
        let next_index = ticks_elapsed.saturating_add(1);
        let offset_ns = next_index.saturating_mul(period_ns);
        let offset = duration_from_nanos_u128(offset_ns);
        // `checked_add` overflowing here is unreachable in practice (see
        // `ManualClock::now_instant`'s docs for the same reasoning), but
        // the fallback chain stays forward-saturating rather than ever
        // returning an instant at or before `after`: try the very next
        // period boundary from `after` directly, and only as an absolute
        // last resort return `after` itself.
        self.anchor
            .checked_add(offset)
            .unwrap_or_else(|| after.checked_add(self.period).unwrap_or(after))
    }

    /// Number of complete periods elapsed between the anchor and `at`
    /// (`0` if `at` is at or before the anchor).
    #[must_use]
    pub fn ticks_elapsed(&self, at: Instant) -> u64 {
        match at.checked_duration_since(self.anchor) {
            Some(elapsed) => {
                let period_ns = self.period.as_nanos().max(1);
                u64::try_from(elapsed.as_nanos() / period_ns).unwrap_or(u64::MAX)
            }
            None => 0,
        }
    }

    /// Number of scheduled tick boundaries between `from` and `to`
    /// (`0` if `to <= from`).
    ///
    /// A scheduler that last acted at `from` and only gets to act again at
    /// a later `to` can use this to tell how many ticks were missed in
    /// between, and decide whether to catch up (fire once) or replay
    /// (fire `ticks_between` times) under its own policy.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_time::TimerInterval;
    /// use std::time::{Duration, Instant};
    /// # fn main() -> Result<(), astrs_time::TimerIntervalError> {
    /// let anchor = Instant::now();
    /// let timer = TimerInterval::with_anchor(Duration::from_millis(100), anchor)?;
    /// let from = anchor + Duration::from_millis(50);
    /// let to = anchor + Duration::from_millis(350);
    /// // Ticks at 100, 200, 300ms all fall in (50ms, 350ms]: 3 missed.
    /// assert_eq!(timer.ticks_between(from, to), 3);
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn ticks_between(&self, from: Instant, to: Instant) -> u64 {
        if to <= from {
            return 0;
        }
        self.ticks_elapsed(to)
            .saturating_sub(self.ticks_elapsed(from))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn anchor() -> Instant {
        Instant::now()
    }

    #[test]
    fn duration_from_nanos_u128_handles_the_full_u64_seconds_range() {
        assert_eq!(duration_from_nanos_u128(0), Duration::ZERO);
        assert_eq!(duration_from_nanos_u128(1), Duration::from_nanos(1));
        assert_eq!(
            duration_from_nanos_u128(1_500_000_000),
            Duration::from_millis(1_500)
        );
        // Beyond what `u64` nanoseconds (~584 years) can express, but well
        // within `Duration`'s true range (~584 billion years).
        let six_hundred_years_ns = 600u128 * 365 * 86_400 * 1_000_000_000;
        assert_eq!(
            duration_from_nanos_u128(six_hundred_years_ns),
            Duration::from_secs(600 * 365 * 86_400)
        );
        // Beyond even `Duration`'s range: saturates rather than panicking.
        assert_eq!(duration_from_nanos_u128(u128::MAX), Duration::MAX);
    }

    #[test]
    fn from_millis_secs_and_period_agree() {
        let anchor = anchor();
        assert_eq!(
            TimerInterval::with_anchor(Duration::from_millis(250), anchor)
                .unwrap()
                .period(),
            Duration::from_millis(250)
        );
        assert_eq!(
            TimerInterval::with_anchor(Duration::from_secs(5), anchor)
                .unwrap()
                .period(),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn from_hz_computes_reciprocal_period() {
        let timer = TimerInterval::from_hz(10.0).unwrap();
        assert_eq!(timer.period(), Duration::from_millis(100));
        assert!((timer.hz() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn from_hz_rejects_non_positive_and_non_finite() {
        assert!(matches!(
            TimerInterval::from_hz(0.0),
            Err(TimerIntervalError::InvalidHz(_))
        ));
        assert!(matches!(
            TimerInterval::from_hz(-1.0),
            Err(TimerIntervalError::InvalidHz(_))
        ));
        assert!(matches!(
            TimerInterval::from_hz(f64::NAN),
            Err(TimerIntervalError::InvalidHz(_))
        ));
        assert!(matches!(
            TimerInterval::from_hz(f64::INFINITY),
            Err(TimerIntervalError::InvalidHz(_))
        ));
    }

    #[test]
    fn from_hz_never_panics_on_extreme_values() {
        // The point of this test is that none of these panic (regardless
        // of whether each ends up `Ok` or `Err`) via the panicking
        // `Duration::from_secs_f64` path `from_hz` deliberately avoids.
        let _ = TimerInterval::from_hz(f64::MIN_POSITIVE);
        let _ = TimerInterval::from_hz(f64::MAX);
        let _ = TimerInterval::from_hz(-0.0);
        let _ = TimerInterval::from_hz(f64::NEG_INFINITY);
    }

    #[test]
    fn zero_period_is_rejected() {
        assert_eq!(
            TimerInterval::from_millis(0),
            Err(TimerIntervalError::ZeroPeriod)
        );
        assert_eq!(
            TimerInterval::from_secs(0),
            Err(TimerIntervalError::ZeroPeriod)
        );
        assert_eq!(
            TimerInterval::with_anchor(Duration::ZERO, anchor()),
            Err(TimerIntervalError::ZeroPeriod)
        );
    }

    #[test]
    fn next_tick_lands_on_the_fixed_grid() {
        let anchor = anchor();
        let timer = TimerInterval::with_anchor(Duration::from_millis(100), anchor).unwrap();
        assert_eq!(timer.next_tick(anchor), anchor + Duration::from_millis(100));
        assert_eq!(
            timer.next_tick(anchor + Duration::from_millis(1)),
            anchor + Duration::from_millis(100)
        );
        assert_eq!(
            timer.next_tick(anchor + Duration::from_millis(99)),
            anchor + Duration::from_millis(100)
        );
        assert_eq!(
            timer.next_tick(anchor + Duration::from_millis(100)),
            anchor + Duration::from_millis(200)
        );
        assert_eq!(
            timer.next_tick(anchor + Duration::from_millis(250)),
            anchor + Duration::from_millis(300)
        );
    }

    #[test]
    fn next_tick_is_always_strictly_after_its_input() {
        let anchor = anchor();
        let timer = TimerInterval::with_anchor(Duration::from_millis(30), anchor).unwrap();
        let mut t = anchor;
        for _ in 0..50 {
            let next = timer.next_tick(t);
            assert!(next > t);
            t = next;
        }
    }

    #[test]
    fn next_tick_before_anchor_returns_anchor() {
        let anchor = anchor();
        let timer = TimerInterval::with_anchor(Duration::from_millis(100), anchor).unwrap();
        let before = anchor - Duration::from_millis(1);
        assert_eq!(timer.next_tick(before), anchor);
    }

    #[test]
    fn next_tick_represents_periods_beyond_u64_nanoseconds_range_correctly() {
        // 600 years in nanoseconds exceeds `u64::MAX` (~584.5 years worth
        // of nanoseconds), so this specifically exercises
        // `duration_from_nanos_u128`'s `Duration::new`-based conversion
        // rather than a `u64`-nanosecond-limited one, which would silently
        // truncate the true 600-year period down to ~584.5 years.
        let anchor = anchor();
        let period = Duration::from_secs(600 * 365 * 86_400);
        let timer = TimerInterval::with_anchor(period, anchor).unwrap();
        assert_eq!(timer.next_tick(anchor), anchor + period);
    }

    #[test]
    fn next_tick_with_a_period_near_duration_max_never_panics_and_stays_forward() {
        // The period itself (hundreds of billions of years) will always
        // exceed any real platform's `Instant` representable range, so
        // this exercises the documented forward-saturating fallback chain
        // end to end rather than the primary "land exactly on the grid"
        // path — the point of this test is the absence of a panic and the
        // preserved `>= after` invariant, not an exact target instant.
        let anchor = anchor();
        let timer = TimerInterval::with_anchor(Duration::MAX, anchor).unwrap();
        let next = timer.next_tick(anchor);
        assert!(next >= anchor);

        let later = anchor + Duration::from_secs(3600);
        let next_from_later = timer.next_tick(later);
        assert!(next_from_later >= later);
    }

    #[test]
    fn repeated_next_tick_walks_the_schedule_without_drift() {
        let anchor = anchor();
        let timer = TimerInterval::with_anchor(Duration::from_millis(10), anchor).unwrap();
        let mut t = anchor;
        for k in 1..=20u32 {
            t = timer.next_tick(t);
            assert_eq!(t, anchor + Duration::from_millis(10) * k);
        }
    }

    #[test]
    fn rebase_changes_anchor_keeps_period() {
        let original_anchor = anchor();
        let timer = TimerInterval::with_anchor(Duration::from_millis(50), original_anchor).unwrap();
        let new_anchor = original_anchor + Duration::from_secs(10);
        let rebased = timer.rebase(new_anchor);
        assert_eq!(rebased.period(), Duration::from_millis(50));
        assert_eq!(rebased.anchor(), new_anchor);
        assert_eq!(
            rebased.next_tick(new_anchor),
            new_anchor + Duration::from_millis(50)
        );
    }

    #[test]
    fn ticks_elapsed_counts_complete_periods() {
        let anchor = anchor();
        let timer = TimerInterval::with_anchor(Duration::from_millis(100), anchor).unwrap();
        assert_eq!(timer.ticks_elapsed(anchor), 0);
        assert_eq!(timer.ticks_elapsed(anchor + Duration::from_millis(99)), 0);
        assert_eq!(timer.ticks_elapsed(anchor + Duration::from_millis(100)), 1);
        assert_eq!(timer.ticks_elapsed(anchor + Duration::from_millis(999)), 9);
        assert_eq!(timer.ticks_elapsed(anchor - Duration::from_millis(1)), 0);
    }

    #[test]
    fn ticks_between_counts_missed_ticks() {
        let anchor = anchor();
        let timer = TimerInterval::with_anchor(Duration::from_millis(100), anchor).unwrap();
        let from = anchor + Duration::from_millis(50);
        let to = anchor + Duration::from_millis(350);
        assert_eq!(timer.ticks_between(from, to), 3);
        assert_eq!(timer.ticks_between(to, from), 0);
        assert_eq!(timer.ticks_between(from, from), 0);
    }

    #[test]
    fn from_virtual_source_path_millis() {
        let timer = TimerInterval::from_virtual_source_path("astrs/timer/millis/250").unwrap();
        assert_eq!(timer.period(), Duration::from_millis(250));
    }

    #[test]
    fn from_virtual_source_path_secs() {
        let timer = TimerInterval::from_virtual_source_path("astrs/timer/secs/5").unwrap();
        assert_eq!(timer.period(), Duration::from_secs(5));
    }

    #[test]
    fn from_virtual_source_path_hz() {
        let timer = TimerInterval::from_virtual_source_path("astrs/timer/hz/10").unwrap();
        assert_eq!(timer.period(), Duration::from_millis(100));
    }

    #[test]
    fn from_virtual_source_path_rejects_non_timer_paths() {
        let err = TimerInterval::from_virtual_source_path("astrs/logs").unwrap_err();
        assert!(matches!(err, TimerSourcePathError::NotATimerPath(_)));
    }

    #[test]
    fn from_virtual_source_path_rejects_unknown_unit() {
        let err = TimerInterval::from_virtual_source_path("astrs/timer/fortnights/1").unwrap_err();
        assert!(matches!(err, TimerSourcePathError::UnknownUnit(_, _)));
    }

    #[test]
    fn from_virtual_source_path_rejects_missing_value() {
        let err = TimerInterval::from_virtual_source_path("astrs/timer/millis").unwrap_err();
        assert!(matches!(err, TimerSourcePathError::MissingValue(_)));
    }

    #[test]
    fn from_virtual_source_path_rejects_trailing_segments() {
        let err =
            TimerInterval::from_virtual_source_path("astrs/timer/millis/250/extra").unwrap_err();
        assert!(matches!(err, TimerSourcePathError::TrailingSegments(_)));
    }

    #[test]
    fn from_virtual_source_path_rejects_invalid_numeric_value() {
        let err = TimerInterval::from_virtual_source_path("astrs/timer/millis/abc").unwrap_err();
        assert!(matches!(err, TimerSourcePathError::InvalidValue(_, _, _)));
    }

    #[test]
    fn from_virtual_source_path_propagates_interval_errors() {
        let err = TimerInterval::from_virtual_source_path("astrs/timer/millis/0").unwrap_err();
        assert!(matches!(
            err,
            TimerSourcePathError::Interval(TimerIntervalError::ZeroPeriod)
        ));
    }

    #[test]
    fn does_not_special_case_dora_timer_paths() {
        // Mapping `dora/timer/...` onto `astrs/timer/...` is astrs-migrate's
        // job (blueprint §8.6), not this crate's.
        let err = TimerInterval::from_virtual_source_path("dora/timer/millis/250").unwrap_err();
        assert!(matches!(err, TimerSourcePathError::NotATimerPath(_)));
    }
}
