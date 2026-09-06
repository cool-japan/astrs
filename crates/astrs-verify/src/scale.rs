//! Exact integer scales: how continuous rates and durations become linear
//! *integer* arithmetic without ever rounding a rate.
//!
//! # Why integers
//!
//! Every obligation in this crate is discharged in linear **integer**
//! arithmetic (LIA). That is a deliberate choice, not a limitation:
//!
//! - Integer models are exactly comparable, so two runs of the same proof
//!   produce byte-identical counterexamples — the determinism this crate
//!   promises (see [`crate::prove`]) falls out structurally instead of
//!   being defended by convention.
//! - Every quantity a manifest actually declares is rational by
//!   construction: `astrs/timer/hz/50` is exactly 50/1 Hz,
//!   `astrs/timer/millis/40` is exactly 25/1 Hz. Floating point would
//!   introduce error into inputs that have none.
//!
//! # The two scales
//!
//! **Time** is nanoseconds ([`Nanos`]), a signed 128-bit count. Durations
//! reaching this crate come from `serde`-parsed `f64` seconds
//! ([`astrs_manifest::DurationSecs`]) and are converted once, at the
//! model boundary, with an explicit rejection for anything not
//! representable — see [`Nanos::from_secs_f64`].
//!
//! **Rates** are never converted to a period. Instead the model picks one
//! **analysis window** ([`Window`]) whose length in seconds is an exact
//! multiple of every declared trigger period, and counts *events per
//! window*. For a set of trigger rates `pᵢ/qᵢ` events per second, a window
//! of `L = lcm(qᵢ)` seconds makes every count `pᵢ · L / qᵢ` an exact
//! integer. Flow constraints then read "over one window, this node fires
//! *n* times", with no division anywhere.
//!
//! Relating the two scales also stays exact, because the encodings
//! multiply rather than divide: "node `n` can service its arrivals" is
//! written `fire[n] · wcet_ns ≤ window_ns`, never `fire[n] ≤ window_ns /
//! wcet_ns`.

use std::fmt;

use astrs_manifest::{DurationSecs, VirtualSource};

use crate::error::{DurationRejection, VerifyError};

/// The largest analysis window this crate will build, in seconds.
///
/// A window is the least common multiple of every declared trigger
/// period's denominator (see this module's docs). Realistic manifests land
/// in the single digits: 50 Hz and 30 Hz together need a window of one
/// second; `timer/secs/3` and `timer/secs/5` need fifteen. A manifest that
/// needs more than a day's worth of window has declared periods with no
/// useful common structure, and the honest answer is
/// [`VerifyError::ScaleOverflow`] rather than a silently rounded model.
pub const MAX_WINDOW_SECONDS: u128 = 86_400;

/// Nanoseconds per second, as used by every conversion here.
pub const NANOS_PER_SEC: i128 = 1_000_000_000;

/// A duration on the crate-wide integer nanosecond scale.
///
/// Deliberately a distinct type from a bare `i128`: the encodings mix
/// event counts and nanosecond quantities in the same linear terms, and
/// the two are only ever combined through the multiplications documented
/// in this module's header.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct Nanos(i128);

impl Nanos {
    /// Zero nanoseconds.
    pub const ZERO: Self = Self(0);

    /// Build a duration from a raw nanosecond count.
    #[must_use]
    pub const fn new(nanos: i128) -> Self {
        Self(nanos)
    }

    /// This duration's raw nanosecond count.
    #[must_use]
    pub const fn get(self) -> i128 {
        self.0
    }

    /// Convert a floating-point second count to exact nanoseconds,
    /// rounding to the nearest nanosecond.
    ///
    /// Rounding here is not a precision compromise in the model: it is the
    /// one place a value that was *already* an `f64` (because the manifest
    /// schema stores durations that way) is pinned to the integer grid,
    /// once, at the boundary. Everything downstream is exact.
    ///
    /// # Errors
    ///
    /// Returns a [`DurationRejection`] if `secs` is negative, not finite,
    /// or beyond the representable nanosecond range.
    pub fn from_secs_f64(secs: f64) -> Result<Self, DurationRejection> {
        if !secs.is_finite() {
            return Err(DurationRejection::NotFinite);
        }
        if secs < 0.0 {
            return Err(DurationRejection::Negative);
        }
        let nanos = secs * (NANOS_PER_SEC as f64);
        if nanos > (i64::MAX as f64) {
            return Err(DurationRejection::TooLarge);
        }
        Ok(Self(nanos.round() as i128))
    }

    /// Convert an [`astrs_manifest::DurationSecs`] to exact nanoseconds,
    /// naming the offending field if it is not representable.
    ///
    /// # Errors
    ///
    /// Returns [`VerifyError::BadDuration`] carrying `what` when the value
    /// is out of range — see [`Nanos::from_secs_f64`].
    pub fn from_manifest(what: impl Into<String>, value: DurationSecs) -> crate::Result<Self> {
        Self::from_secs_f64(value.as_secs_f64()).map_err(|reason| VerifyError::BadDuration {
            what: what.into(),
            reason,
        })
    }

    /// This duration rendered for a human report, choosing the largest
    /// unit that keeps the number readable.
    ///
    /// Deterministic and lossless for every value the model carries: the
    /// unit is chosen from the magnitude alone and the mantissa is printed
    /// with the exact digits the integer holds, so two runs of the same
    /// proof render identical text.
    #[must_use]
    pub fn render(self) -> String {
        let n = self.0;
        if n == 0 {
            return "0".to_string();
        }
        let (sign, magnitude) = if n < 0 { ("-", -n) } else { ("", n) };
        let (unit, divisor) = if magnitude >= NANOS_PER_SEC {
            ("s", NANOS_PER_SEC)
        } else if magnitude >= 1_000_000 {
            ("ms", 1_000_000)
        } else if magnitude >= 1_000 {
            ("us", 1_000)
        } else {
            ("ns", 1)
        };
        let whole = magnitude / divisor;
        let fraction = magnitude % divisor;
        if fraction == 0 {
            format!("{sign}{whole}{unit}")
        } else {
            let width = match divisor {
                NANOS_PER_SEC => 9,
                1_000_000 => 6,
                _ => 3,
            };
            let digits = format!("{fraction:0width$}", width = width);
            format!("{sign}{whole}.{}{unit}", digits.trim_end_matches('0'))
        }
    }
}

impl fmt::Display for Nanos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// An exact non-negative rational rate in events per second.
///
/// Always stored in lowest terms with a positive denominator, so equality
/// is structural and the [`Window`] computation can take denominators at
/// face value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Rate {
    /// Events, in lowest terms.
    numerator: u64,
    /// Seconds, in lowest terms; never zero.
    denominator: u64,
}

impl Rate {
    /// A rate of zero events per second.
    pub const ZERO: Self = Self {
        numerator: 0,
        denominator: 1,
    };

    /// Build `numerator / denominator` events per second, reduced.
    ///
    /// Returns `None` when `denominator` is zero.
    #[must_use]
    pub fn new(numerator: u64, denominator: u64) -> Option<Self> {
        if denominator == 0 {
            return None;
        }
        let divisor = gcd(numerator, denominator).max(1);
        Some(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }

    /// The rate implied by a manifest virtual source, if it is periodic.
    ///
    /// Returns `None` for the event-driven virtual sources
    /// (`astrs/logs…`, `astrs/status`), which have no declared rate —
    /// see [`astrs_manifest::VirtualSource::period`], whose `Option` this
    /// mirrors.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_manifest::recognize_virtual_source;
    /// use astrs_verify::Rate;
    ///
    /// let source = recognize_virtual_source("astrs/timer/millis/40")
    ///     .expect("astrs/ prefix")
    ///     .expect("well formed");
    /// // 40 ms is exactly 25 Hz — no rounding anywhere.
    /// assert_eq!(Rate::from_virtual_source(&source), Rate::new(25, 1));
    /// ```
    #[must_use]
    pub fn from_virtual_source(source: &VirtualSource) -> Option<Self> {
        match source {
            VirtualSource::TimerHz(n) => Self::new(*n, 1),
            VirtualSource::TimerSecs(n) => Self::new(1, *n),
            VirtualSource::TimerMillis(n) => Self::new(1_000, *n),
            VirtualSource::Logs { .. } | VirtualSource::Status => None,
        }
    }

    /// Build a rate from a floating-point events-per-second figure,
    /// recovering its exact rational form.
    ///
    /// Only values with a short exact decimal expansion are accepted (up
    /// to [`Self::MAX_DECIMAL_PLACES`] fractional digits): a profile that
    /// says `rate: 7.5` means exactly 15/2 Hz, and a profile that says
    /// `rate: 0.3333333333` means the author should have written `1/3`.
    /// Returning `None` rather than a nearby rational keeps the analysis
    /// window honest.
    #[must_use]
    pub fn from_f64(rate: f64) -> Option<Self> {
        if !rate.is_finite() || rate <= 0.0 {
            return None;
        }
        let mut denominator: u64 = 1;
        for _ in 0..=Self::MAX_DECIMAL_PLACES {
            let scaled = rate * (denominator as f64);
            if (scaled - scaled.round()).abs() < f64::EPSILON * scaled.max(1.0) {
                let numerator = scaled.round();
                if numerator > (u64::MAX as f64) {
                    return None;
                }
                return Self::new(numerator as u64, denominator);
            }
            denominator = denominator.checked_mul(10)?;
        }
        None
    }

    /// How many fractional decimal digits [`Rate::from_f64`] will accept
    /// before declaring a figure irrational for its purposes.
    pub const MAX_DECIMAL_PLACES: u32 = 6;

    /// This rate's numerator, in lowest terms.
    #[must_use]
    pub const fn numerator(self) -> u64 {
        self.numerator
    }

    /// This rate's denominator, in lowest terms; never zero.
    #[must_use]
    pub const fn denominator(self) -> u64 {
        self.denominator
    }

    /// Whether this rate is zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.numerator == 0
    }

    /// The exact number of events this rate produces over `window`.
    ///
    /// Exact by construction: [`Window::covering`] built the window from
    /// the least common multiple of every participating denominator, so
    /// `window.seconds() / denominator` divides evenly.
    ///
    /// Returns `None` only on integer overflow, which
    /// [`Window::covering`]'s own bound makes unreachable for rates that
    /// were part of the covering set.
    #[must_use]
    pub fn events_per_window(self, window: Window) -> Option<i128> {
        let seconds = window.seconds();
        let multiplier = seconds.checked_div(u128::from(self.denominator))?;
        let events = u128::from(self.numerator).checked_mul(multiplier)?;
        i128::try_from(events).ok()
    }

    /// The exact sum of two rates, or `None` on overflow.
    ///
    /// Rate *addition* is how an event-driven node's activation rate is
    /// derived: a node fires once per delivered event, so its firing rate
    /// is the sum of its inputs' arrival rates (blueprint §9.1's merged
    /// event loop). Exact rational addition keeps that derivation free of
    /// rounding all the way down a pipeline.
    #[must_use]
    pub fn checked_add(self, other: Self) -> Option<Self> {
        let numerator = u128::from(self.numerator)
            .checked_mul(u128::from(other.denominator))?
            .checked_add(u128::from(other.numerator).checked_mul(u128::from(self.denominator))?)?;
        let denominator =
            u128::from(self.denominator).checked_mul(u128::from(other.denominator))?;
        Self::new(
            u64::try_from(numerator).ok()?,
            u64::try_from(denominator).ok()?,
        )
    }

    /// This rate multiplied by a whole number of events, or `None` on
    /// overflow.
    #[must_use]
    pub fn checked_mul_u64(self, factor: u64) -> Option<Self> {
        Self::new(self.numerator.checked_mul(factor)?, self.denominator)
    }

    /// Compare two rates exactly, without converting either to a float.
    ///
    /// `Rate` cannot derive `Ord`: `1/3` and `2/6` reduce to the same
    /// value, but `50/1` and `1/50` order by neither field alone. The
    /// cross-multiplied comparison below is exact for every rate this
    /// crate builds (numerators and denominators are `u64`, so the
    /// products fit `u128`).
    #[must_use]
    pub fn compare(self, other: Self) -> std::cmp::Ordering {
        let left = u128::from(self.numerator) * u128::from(other.denominator);
        let right = u128::from(other.numerator) * u128::from(self.denominator);
        left.cmp(&right)
    }

    /// This rate's exact period, in nanoseconds, or `None` for a zero rate
    /// (which has no period) or when the period is not a whole number of
    /// nanoseconds.
    ///
    /// Used only for *rendering* a counterexample — no encoding divides.
    #[must_use]
    pub fn exact_period_nanos(self) -> Option<Nanos> {
        if self.numerator == 0 {
            return None;
        }
        let total = NANOS_PER_SEC.checked_mul(i128::from(self.denominator))?;
        let numerator = i128::from(self.numerator);
        (total % numerator == 0).then(|| Nanos::new(total / numerator))
    }
}

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.denominator == 1 {
            write!(f, "{} Hz", self.numerator)
        } else {
            write!(f, "{}/{} Hz", self.numerator, self.denominator)
        }
    }
}

/// The analysis window every flow constraint counts events over.
///
/// See this module's header for why a window exists at all. Construct one
/// with [`Window::covering`], which picks the shortest window that makes
/// every supplied rate an exact whole number of events.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct Window {
    /// The window length in whole seconds; never zero.
    seconds: u128,
}

impl Window {
    /// The one-second window, which covers every whole-Hz rate.
    pub const ONE_SECOND: Self = Self { seconds: 1 };

    /// The shortest window over which every rate in `rates` produces a
    /// whole number of events.
    ///
    /// The result is the least common multiple of the rates' denominators
    /// (see this module's header). Rates are consumed in iteration order,
    /// but the least common multiple is order-independent, so the window
    /// is a pure function of the *set* of rates — one of the things that
    /// makes a proof run reproducible.
    ///
    /// # Errors
    ///
    /// Returns [`VerifyError::ScaleOverflow`] if the required window
    /// exceeds [`MAX_WINDOW_SECONDS`].
    pub fn covering(rates: impl IntoIterator<Item = Rate>) -> crate::Result<Self> {
        let mut seconds: u128 = 1;
        for rate in rates {
            if rate.is_zero() {
                continue;
            }
            let denominator = u128::from(rate.denominator());
            seconds = lcm_u128(seconds, denominator).ok_or(VerifyError::ScaleOverflow {
                needed: u128::MAX,
                limit: MAX_WINDOW_SECONDS,
            })?;
            if seconds > MAX_WINDOW_SECONDS {
                return Err(VerifyError::ScaleOverflow {
                    needed: seconds,
                    limit: MAX_WINDOW_SECONDS,
                });
            }
        }
        Ok(Self { seconds })
    }

    /// This window's length in whole seconds.
    #[must_use]
    pub const fn seconds(self) -> u128 {
        self.seconds
    }

    /// This window's length in nanoseconds.
    ///
    /// Bounded by [`MAX_WINDOW_SECONDS`] · 10⁹, comfortably inside
    /// `i128`, so the conversion never saturates for a window
    /// [`Window::covering`] produced.
    #[must_use]
    pub fn nanos(self) -> Nanos {
        Nanos::new(
            i128::try_from(self.seconds)
                .unwrap_or(i128::MAX / NANOS_PER_SEC)
                .saturating_mul(NANOS_PER_SEC),
        )
    }
}

impl fmt::Display for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.seconds == 1 {
            f.write_str("1s")
        } else {
            write!(f, "{}s", self.seconds)
        }
    }
}

/// Greatest common divisor, Euclid's algorithm on `u64`.
fn gcd(a: u64, b: u64) -> u64 {
    let (mut a, mut b) = (a, b);
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// Least common multiple on `u128`, returning `None` on overflow.
fn lcm_u128(a: u128, b: u128) -> Option<u128> {
    if a == 0 || b == 0 {
        return Some(0);
    }
    let mut x = a;
    let mut y = b;
    while y != 0 {
        let t = x % y;
        x = y;
        y = t;
    }
    a.checked_div(x)?.checked_mul(b)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn rate_reduces_to_lowest_terms() {
        let rate = Rate::new(1_000, 40).expect("positive denominator");
        assert_eq!(rate.numerator(), 25);
        assert_eq!(rate.denominator(), 1);
    }

    #[test]
    fn rate_rejects_zero_denominator() {
        assert_eq!(Rate::new(1, 0), None);
    }

    #[test]
    fn timer_sources_map_to_exact_rates() {
        let cases = [
            (VirtualSource::TimerHz(50), (50, 1)),
            (VirtualSource::TimerMillis(40), (25, 1)),
            (VirtualSource::TimerMillis(3), (1_000, 3)),
            (VirtualSource::TimerSecs(2), (1, 2)),
        ];
        for (source, (numerator, denominator)) in cases {
            let rate = Rate::from_virtual_source(&source).expect("periodic source");
            assert_eq!(
                (rate.numerator(), rate.denominator()),
                (numerator, denominator),
                "{source:?}"
            );
        }
    }

    #[test]
    fn event_driven_sources_have_no_rate() {
        assert_eq!(Rate::from_virtual_source(&VirtualSource::Status), None);
        assert_eq!(
            Rate::from_virtual_source(&VirtualSource::Logs {
                level: None,
                node: None
            }),
            None
        );
    }

    #[test]
    fn window_is_lcm_of_denominators() {
        let rates = [
            Rate::new(1, 3).expect("rate"),
            Rate::new(1, 5).expect("rate"),
            Rate::new(50, 1).expect("rate"),
        ];
        let window = Window::covering(rates).expect("small window");
        assert_eq!(window.seconds(), 15);
    }

    #[test]
    fn window_is_order_independent() {
        let a = Window::covering([
            Rate::new(1, 4).expect("rate"),
            Rate::new(1, 6).expect("rate"),
        ])
        .expect("window");
        let b = Window::covering([
            Rate::new(1, 6).expect("rate"),
            Rate::new(1, 4).expect("rate"),
        ])
        .expect("window");
        assert_eq!(a, b);
        assert_eq!(a.seconds(), 12);
    }

    #[test]
    fn window_rejects_pathological_period_sets() {
        // Six mutually prime periods whose product blows past a day.
        let rates: Vec<Rate> = [7_u64, 11, 13, 17, 19, 23, 29]
            .into_iter()
            .map(|d| Rate::new(1, d).expect("rate"))
            .collect();
        let err = Window::covering(rates).expect_err("must overflow");
        assert!(matches!(err, VerifyError::ScaleOverflow { .. }));
    }

    #[test]
    fn events_per_window_is_exact() {
        let window = Window::covering([
            Rate::new(1, 3).expect("rate"),
            Rate::new(50, 1).expect("rate"),
        ])
        .expect("window");
        assert_eq!(window.seconds(), 3);
        assert_eq!(
            Rate::new(1, 3)
                .expect("rate")
                .events_per_window(window)
                .expect("exact"),
            1
        );
        assert_eq!(
            Rate::new(50, 1)
                .expect("rate")
                .events_per_window(window)
                .expect("exact"),
            150
        );
    }

    #[test]
    fn zero_rate_does_not_widen_the_window() {
        let window =
            Window::covering([Rate::ZERO, Rate::new(1, 7).expect("rate")]).expect("window");
        assert_eq!(window.seconds(), 7);
    }

    #[test]
    fn window_nanos_matches_seconds() {
        let window = Window::covering([Rate::new(1, 2).expect("rate")]).expect("window");
        assert_eq!(window.nanos(), Nanos::new(2 * NANOS_PER_SEC));
    }

    #[test]
    fn nanos_from_secs_rejects_bad_values() {
        assert_eq!(Nanos::from_secs_f64(-1.0), Err(DurationRejection::Negative));
        assert_eq!(
            Nanos::from_secs_f64(f64::NAN),
            Err(DurationRejection::NotFinite)
        );
        assert_eq!(Nanos::from_secs_f64(1e12), Err(DurationRejection::TooLarge));
    }

    #[test]
    fn nanos_render_picks_readable_units() {
        assert_eq!(Nanos::new(0).render(), "0");
        assert_eq!(Nanos::new(750).render(), "750ns");
        assert_eq!(Nanos::new(1_500).render(), "1.5us");
        assert_eq!(Nanos::new(20_000_000).render(), "20ms");
        assert_eq!(Nanos::new(1_500_000_000).render(), "1.5s");
        assert_eq!(Nanos::new(-2_000_000_000).render(), "-2s");
    }

    #[test]
    fn nanos_render_is_stable_across_calls() {
        let value = Nanos::new(123_456_789);
        assert_eq!(value.render(), value.render());
        assert_eq!(value.render(), "123.456789ms");
    }

    #[test]
    fn rate_from_f64_recovers_exact_fractions() {
        assert_eq!(Rate::from_f64(7.5), Rate::new(15, 2));
        assert_eq!(Rate::from_f64(30.0), Rate::new(30, 1));
        assert_eq!(Rate::from_f64(0.5), Rate::new(1, 2));
    }

    #[test]
    fn rate_from_f64_rejects_unrepresentable_and_non_positive() {
        assert_eq!(Rate::from_f64(0.0), None);
        assert_eq!(Rate::from_f64(-3.0), None);
        assert_eq!(Rate::from_f64(f64::INFINITY), None);
        assert_eq!(Rate::from_f64(1.0 / 3.0), None);
    }

    #[test]
    fn rate_display_reads_naturally() {
        assert_eq!(Rate::new(50, 1).expect("rate").to_string(), "50 Hz");
        assert_eq!(Rate::new(1, 3).expect("rate").to_string(), "1/3 Hz");
    }

    #[test]
    fn exact_period_nanos_matches_the_rate() {
        assert_eq!(
            Rate::new(50, 1).expect("rate").exact_period_nanos(),
            Some(Nanos::new(20_000_000))
        );
        assert_eq!(Rate::ZERO.exact_period_nanos(), None);
        // 3 Hz has no whole-nanosecond period.
        assert_eq!(Rate::new(3, 1).expect("rate").exact_period_nanos(), None);
    }

    #[test]
    fn gcd_and_lcm_behave() {
        assert_eq!(gcd(12, 18), 6);
        assert_eq!(gcd(7, 1), 1);
        assert_eq!(lcm_u128(4, 6), Some(12));
        assert_eq!(lcm_u128(0, 6), Some(0));
        assert_eq!(lcm_u128(u128::MAX, u128::MAX - 1), None);
    }

    #[test]
    fn rates_add_exactly() {
        let a = Rate::new(1, 3).expect("rate");
        let b = Rate::new(1, 6).expect("rate");
        assert_eq!(a.checked_add(b), Rate::new(1, 2));
        assert_eq!(Rate::ZERO.checked_add(a), Some(a));
    }

    #[test]
    fn rate_addition_overflow_is_reported() {
        let big = Rate::new(u64::MAX, u64::MAX - 1).expect("rate");
        assert_eq!(big.checked_add(big), None);
    }

    #[test]
    fn rates_multiply_by_whole_events() {
        let rate = Rate::new(1, 4).expect("rate");
        assert_eq!(rate.checked_mul_u64(8), Rate::new(2, 1));
        assert_eq!(rate.checked_mul_u64(0), Some(Rate::ZERO));
        assert_eq!(
            Rate::new(u64::MAX, 1).expect("rate").checked_mul_u64(2),
            None
        );
    }

    #[test]
    fn rate_comparison_is_exact() {
        use std::cmp::Ordering;
        let slow = Rate::new(1, 2).expect("rate");
        let fast = Rate::new(50, 1).expect("rate");
        assert_eq!(slow.compare(fast), Ordering::Less);
        assert_eq!(fast.compare(slow), Ordering::Greater);
        assert_eq!(
            Rate::new(1, 3)
                .expect("rate")
                .compare(Rate::new(2, 6).expect("rate")),
            Ordering::Equal
        );
    }

    #[test]
    fn window_display_reads_naturally() {
        assert_eq!(Window::ONE_SECOND.to_string(), "1s");
        assert_eq!(
            Window::covering([Rate::new(1, 5).expect("rate")])
                .expect("window")
                .to_string(),
            "5s"
        );
    }
}
