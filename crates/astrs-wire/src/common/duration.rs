//! [`DurationMs`] — the wire representation of a duration.
//!
//! Every timeout, delay and window in the control plane travels as a
//! millisecond count. Milliseconds are the granularity the manifest speaks in
//! (`health_check_interval: 5.0`, `restart_delay`, `timeout`) and the
//! granularity supervision actually operates at; nanoseconds would cost more
//! varint bytes on every heartbeat for precision nothing uses.
//!
//! Sub-millisecond timing lives in [`astrs_time::HlcTimestamp`], which is
//! nanosecond-resolution and is what timestamps events.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::DurationMs;
//! use std::time::Duration;
//!
//! let timeout = DurationMs::from_secs(5);
//! assert_eq!(timeout.as_millis(), 5_000);
//! assert_eq!(timeout.to_duration(), Duration::from_secs(5));
//! assert_eq!(DurationMs::from_duration(Duration::from_micros(1_500)), DurationMs::new(1));
//! ```

use core::fmt;
use core::time::Duration;

use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// A duration in whole milliseconds.
///
/// # Examples
///
/// ```
/// use astrs_wire::DurationMs;
///
/// let delay = DurationMs::new(250);
/// assert_eq!(delay.to_string(), "250ms");
/// assert_eq!(delay.checked_mul(2), Some(DurationMs::new(500)));
/// assert_eq!(DurationMs::MAX.checked_mul(2), None);
/// ```
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Encode,
    Decode,
)]
#[serde(transparent)]
pub struct DurationMs(u64);

impl DurationMs {
    /// Zero milliseconds.
    pub const ZERO: Self = Self(0);

    /// The largest representable duration.
    pub const MAX: Self = Self(u64::MAX);

    /// The default heartbeat interval: 5 s (blueprint §24.2).
    pub const HEARTBEAT: Self = Self(5_000);

    /// The default metrics sampling interval: 2 s (blueprint §24.2).
    pub const METRICS: Self = Self(2_000);

    /// The default health-check interval: 5 s (blueprint §24.2).
    pub const HEALTH_CHECK: Self = Self(5_000);

    /// Wraps a millisecond count.
    #[must_use]
    pub const fn new(millis: u64) -> Self {
        Self(millis)
    }

    /// A duration of `secs` seconds, saturating at [`DurationMs::MAX`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::DurationMs;
    ///
    /// assert_eq!(DurationMs::from_secs(2).as_millis(), 2_000);
    /// assert_eq!(DurationMs::from_secs(u64::MAX), DurationMs::MAX);
    /// ```
    #[must_use]
    pub const fn from_secs(secs: u64) -> Self {
        Self(secs.saturating_mul(1_000))
    }

    /// The millisecond count.
    #[must_use]
    pub const fn as_millis(self) -> u64 {
        self.0
    }

    /// Whether this is zero.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// As a [`std::time::Duration`].
    #[must_use]
    pub const fn to_duration(self) -> Duration {
        Duration::from_millis(self.0)
    }

    /// From a [`std::time::Duration`], truncating toward zero.
    ///
    /// Truncation is the right rounding for a timeout: rounding *up* would
    /// silently make a "0.5 ms" deadline a whole millisecond, which is a
    /// bigger surprise than losing sub-millisecond precision that the control
    /// plane never had.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::DurationMs;
    /// use std::time::Duration;
    ///
    /// assert_eq!(DurationMs::from_duration(Duration::from_micros(999)), DurationMs::ZERO);
    /// assert_eq!(DurationMs::from_duration(Duration::from_millis(3)), DurationMs::new(3));
    /// ```
    #[must_use]
    pub fn from_duration(duration: Duration) -> Self {
        Self(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
    }

    /// Multiplies by `factor`, returning `None` on overflow.
    ///
    /// Used by the exponential restart backoff of blueprint §12
    /// (`restart_delay × 2^n`), where overflowing to a tiny delay would turn a
    /// backoff into a spin.
    #[must_use]
    pub const fn checked_mul(self, factor: u64) -> Option<Self> {
        match self.0.checked_mul(factor) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Adds two durations, saturating at [`DurationMs::MAX`].
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    /// The smaller of two durations.
    #[must_use]
    pub const fn min(self, other: Self) -> Self {
        if self.0 < other.0 { self } else { other }
    }

    /// The larger of two durations.
    #[must_use]
    pub const fn max(self, other: Self) -> Self {
        if self.0 > other.0 { self } else { other }
    }
}

impl fmt::Display for DurationMs {
    /// Renders in the manifest's human duration style: `250ms`, `5s`, `2m`.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::DurationMs;
    ///
    /// assert_eq!(DurationMs::new(0).to_string(), "0ms");
    /// assert_eq!(DurationMs::new(250).to_string(), "250ms");
    /// assert_eq!(DurationMs::new(5_000).to_string(), "5s");
    /// assert_eq!(DurationMs::new(5_500).to_string(), "5500ms");
    /// assert_eq!(DurationMs::new(120_000).to_string(), "2m");
    /// ```
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let millis = self.0;
        if millis != 0 && millis.is_multiple_of(60_000) {
            write!(f, "{}m", millis / 60_000)
        } else if millis != 0 && millis.is_multiple_of(1_000) {
            write!(f, "{}s", millis / 1_000)
        } else {
            write!(f, "{millis}ms")
        }
    }
}

impl From<u64> for DurationMs {
    fn from(millis: u64) -> Self {
        Self(millis)
    }
}

impl From<DurationMs> for u64 {
    fn from(duration: DurationMs) -> Self {
        duration.0
    }
}

impl From<Duration> for DurationMs {
    fn from(duration: Duration) -> Self {
        Self::from_duration(duration)
    }
}

impl From<DurationMs> for Duration {
    fn from(duration: DurationMs) -> Self {
        duration.to_duration()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::round_trip;

    #[test]
    fn constants_match_the_blueprint_defaults() {
        assert_eq!(DurationMs::HEARTBEAT.as_millis(), 5_000);
        assert_eq!(DurationMs::METRICS.as_millis(), 2_000);
        assert_eq!(DurationMs::HEALTH_CHECK.as_millis(), 5_000);
        assert!(DurationMs::ZERO.is_zero());
        assert_eq!(DurationMs::default(), DurationMs::ZERO);
    }

    #[test]
    fn conversions_round_trip_through_std_duration() {
        for millis in [0u64, 1, 999, 1_000, 60_000, 86_400_000] {
            let duration = DurationMs::new(millis);
            assert_eq!(DurationMs::from(duration.to_duration()), duration);
            assert_eq!(Duration::from(duration), Duration::from_millis(millis));
            assert_eq!(u64::from(duration), millis);
            assert_eq!(DurationMs::from(millis), duration);
        }
    }

    #[test]
    fn sub_millisecond_input_truncates() {
        assert_eq!(
            DurationMs::from_duration(Duration::from_nanos(1)),
            DurationMs::ZERO
        );
        assert_eq!(
            DurationMs::from_duration(Duration::from_micros(1_999)),
            DurationMs::new(1)
        );
    }

    #[test]
    fn huge_durations_saturate_instead_of_wrapping() {
        assert_eq!(DurationMs::from_secs(u64::MAX), DurationMs::MAX);
        assert_eq!(DurationMs::from_duration(Duration::MAX), DurationMs::MAX);
        assert_eq!(
            DurationMs::MAX.saturating_add(DurationMs::new(1)),
            DurationMs::MAX
        );
    }

    #[test]
    fn checked_mul_refuses_to_wrap() {
        assert_eq!(
            DurationMs::new(100).checked_mul(4),
            Some(DurationMs::new(400))
        );
        assert_eq!(
            DurationMs::new(0).checked_mul(u64::MAX),
            Some(DurationMs::ZERO)
        );
        assert_eq!(DurationMs::MAX.checked_mul(2), None);
    }

    #[test]
    fn exponential_backoff_stays_bounded() {
        // The §12 restart backoff: delay × 2^n capped by max_restart_delay.
        let base = DurationMs::new(100);
        let cap = DurationMs::from_secs(30);
        let mut delay = base;
        for _ in 0..64 {
            delay = delay.checked_mul(2).unwrap_or(cap).min(cap);
            assert!(delay <= cap);
        }
        assert_eq!(delay, cap);
    }

    #[test]
    fn min_and_max_pick_the_right_side() {
        let small = DurationMs::new(1);
        let large = DurationMs::new(2);
        assert_eq!(small.min(large), small);
        assert_eq!(small.max(large), large);
        assert_eq!(large.min(small), small);
        assert_eq!(large.max(small), large);
    }

    #[test]
    fn display_picks_the_largest_exact_unit() {
        assert_eq!(DurationMs::new(0).to_string(), "0ms");
        assert_eq!(DurationMs::new(1).to_string(), "1ms");
        assert_eq!(DurationMs::new(999).to_string(), "999ms");
        assert_eq!(DurationMs::new(1_000).to_string(), "1s");
        assert_eq!(DurationMs::new(1_500).to_string(), "1500ms");
        assert_eq!(DurationMs::new(59_000).to_string(), "59s");
        assert_eq!(DurationMs::new(60_000).to_string(), "1m");
        assert_eq!(DurationMs::new(90_000).to_string(), "90s");
    }

    #[test]
    fn codec_and_serde_round_trip() {
        for millis in [0u64, 1, 5_000, u64::MAX] {
            let duration = DurationMs::new(millis);
            assert_eq!(round_trip(&duration).unwrap(), duration);
            let json = serde_json::to_string(&duration).unwrap();
            assert_eq!(json, millis.to_string());
            assert_eq!(serde_json::from_str::<DurationMs>(&json).unwrap(), duration);
        }
    }

    #[test]
    fn small_values_are_cheap_on_the_wire() {
        use crate::codec::WireEncode;

        assert_eq!(DurationMs::new(1).encode_to_vec().unwrap().len(), 1);
    }
}
