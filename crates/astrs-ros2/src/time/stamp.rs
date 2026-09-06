//! [`RosTime`] and [`RosDuration`]: the two `builtin_interfaces` value types,
//! with every conversion the bridge needs.

use core::cmp::Ordering;
use core::fmt;
use std::time::Duration as StdDuration;

use astrs_rtps::structure::{DdsDuration, Time as RtpsTime};
use astrs_time::HlcTimestamp;

use crate::idl::generated::builtin_interfaces;

/// Nanoseconds in one second.
pub const NANOS_PER_SEC: u32 = 1_000_000_000;

/// Nanoseconds in one second, as the wider type arithmetic needs.
const NANOS_PER_SEC_I128: i128 = 1_000_000_000;

/// A ROS 2 point in time: `builtin_interfaces/msg/Time`.
///
/// Normalized on construction — `nanosec` is always in `[0, 1e9)` — so two
/// equal instants always compare equal, which a raw struct with a
/// billion-nanosecond field could not promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RosTime {
    /// Seconds since the epoch, positive or negative.
    pub sec: i32,
    /// Nanoseconds into the second, always in `[0, 1e9)`.
    pub nanosec: u32,
}

impl RosTime {
    /// The zero time: what an unset `header.stamp` carries.
    pub const ZERO: Self = Self { sec: 0, nanosec: 0 };

    /// The largest representable time.
    pub const MAX: Self = Self {
        sec: i32::MAX,
        nanosec: NANOS_PER_SEC - 1,
    };

    /// The smallest representable time.
    pub const MIN: Self = Self {
        sec: i32::MIN,
        nanosec: 0,
    };

    /// Build a time from components, normalizing an over-large `nanosec`.
    ///
    /// A caller that hands in `(1, 1_500_000_000)` means `(2, 500_000_000)`,
    /// and normalizing here means nothing downstream has to.
    #[must_use]
    pub const fn new(sec: i32, nanosec: u32) -> Self {
        let extra_secs = nanosec / NANOS_PER_SEC;
        let nanosec = nanosec % NANOS_PER_SEC;
        let sec = sec.saturating_add_unsigned(extra_secs);
        Self { sec, nanosec }
    }

    /// Build a time from a signed nanosecond count.
    #[must_use]
    pub const fn from_nanos(nanos: i128) -> Self {
        let mut sec = nanos.div_euclid(NANOS_PER_SEC_I128);
        let nanosec = nanos.rem_euclid(NANOS_PER_SEC_I128);
        if sec > i32::MAX as i128 {
            sec = i32::MAX as i128;
        } else if sec < i32::MIN as i128 {
            sec = i32::MIN as i128;
        }
        Self {
            sec: sec as i32,
            nanosec: nanosec as u32,
        }
    }

    /// The time as a signed nanosecond count.
    #[must_use]
    pub const fn to_nanos(self) -> i128 {
        (self.sec as i128)
            .wrapping_mul(NANOS_PER_SEC_I128)
            .wrapping_add(self.nanosec as i128)
    }

    /// True when the time is at or after the epoch.
    #[must_use]
    pub const fn is_positive(self) -> bool {
        self.sec >= 0
    }

    /// True when the time is exactly zero — an unset `header.stamp`.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.sec == 0 && self.nanosec == 0
    }

    /// Read a time from an HLC timestamp, **dropping the logical counter**.
    ///
    /// See the module docs: ROS time has no logical component, and
    /// manufacturing nanoseconds to carry one would make the stamp lie.
    #[must_use]
    pub const fn from_hlc(stamp: HlcTimestamp) -> Self {
        Self::from_nanos(stamp.physical_ns() as i128)
    }

    /// Read a time into an HLC timestamp with a zero logical counter.
    ///
    /// `None` for a time before the epoch, which an unsigned HLC cannot
    /// hold.
    #[must_use]
    pub const fn to_hlc(self) -> Option<HlcTimestamp> {
        let nanos = self.to_nanos();
        if nanos < 0 {
            return None;
        }
        if nanos > u64::MAX as i128 {
            return None;
        }
        Some(HlcTimestamp::new(nanos as u64, 0))
    }

    /// True when `stamp` survives a `hlc → ros → hlc` round trip unchanged.
    ///
    /// Exactly when its logical counter is zero and its physical component
    /// is inside `i32` seconds.
    #[must_use]
    pub fn round_trips_losslessly(stamp: HlcTimestamp) -> bool {
        stamp.logical() == 0 && Self::from_hlc(stamp).to_hlc() == Some(stamp)
    }

    /// Read a time from an RTPS source timestamp.
    #[must_use]
    pub const fn from_rtps(time: RtpsTime) -> Self {
        Self::from_nanos(time.to_nanos())
    }

    /// Render as an RTPS source timestamp.
    ///
    /// RTPS keeps the sub-second part as a fraction of 2⁻³², so a nanosecond
    /// value does not always survive the round trip exactly; the error is
    /// under a quarter of a nanosecond, which is below the resolution of
    /// every clock either side of this conversion.
    #[must_use]
    pub const fn to_rtps(self) -> RtpsTime {
        RtpsTime::from_unix_nanos(self.to_nanos())
    }

    /// Read the generated wire type.
    #[must_use]
    pub const fn from_message(message: &builtin_interfaces::Time) -> Self {
        Self::new(message.sec, message.nanosec)
    }

    /// Render as the generated wire type.
    #[must_use]
    pub const fn to_message(self) -> builtin_interfaces::Time {
        builtin_interfaces::Time {
            sec: self.sec,
            nanosec: self.nanosec,
        }
    }

    /// The signed interval from `earlier` to this time.
    #[must_use]
    pub const fn since(self, earlier: Self) -> RosDuration {
        RosDuration::from_nanos(self.to_nanos().wrapping_sub(earlier.to_nanos()))
    }

    /// This time advanced by `duration`, saturating at [`RosTime::MAX`].
    #[must_use]
    pub const fn saturating_add(self, duration: RosDuration) -> Self {
        Self::from_nanos(self.to_nanos().saturating_add(duration.to_nanos()))
    }

    /// This time moved back by `duration`, saturating at [`RosTime::MIN`].
    #[must_use]
    pub const fn saturating_sub(self, duration: RosDuration) -> Self {
        Self::from_nanos(self.to_nanos().saturating_sub(duration.to_nanos()))
    }
}

impl PartialOrd for RosTime {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RosTime {
    fn cmp(&self, other: &Self) -> Ordering {
        self.to_nanos().cmp(&other.to_nanos())
    }
}

impl fmt::Display for RosTime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{:09}", self.sec, self.nanosec)
    }
}

impl From<builtin_interfaces::Time> for RosTime {
    fn from(message: builtin_interfaces::Time) -> Self {
        Self::from_message(&message)
    }
}

impl From<RosTime> for builtin_interfaces::Time {
    fn from(time: RosTime) -> Self {
        time.to_message()
    }
}

impl From<HlcTimestamp> for RosTime {
    fn from(stamp: HlcTimestamp) -> Self {
        Self::from_hlc(stamp)
    }
}

/// A ROS 2 interval: `builtin_interfaces/msg/Duration`.
///
/// Signed, unlike [`std::time::Duration`], because a ROS duration is
/// routinely negative — `tf`'s time-travel lookups and a deadline that has
/// already passed are both ordinary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RosDuration {
    /// Whole seconds, positive or negative.
    pub sec: i32,
    /// Nanoseconds into the second, always in `[0, 1e9)`.
    pub nanosec: u32,
}

impl RosDuration {
    /// A zero-length interval.
    pub const ZERO: Self = Self { sec: 0, nanosec: 0 };

    /// The longest representable interval.
    pub const MAX: Self = Self {
        sec: i32::MAX,
        nanosec: NANOS_PER_SEC - 1,
    };

    /// Build an interval from components, normalizing an over-large
    /// `nanosec`.
    #[must_use]
    pub const fn new(sec: i32, nanosec: u32) -> Self {
        let extra_secs = nanosec / NANOS_PER_SEC;
        let nanosec = nanosec % NANOS_PER_SEC;
        Self {
            sec: sec.saturating_add_unsigned(extra_secs),
            nanosec,
        }
    }

    /// Build an interval from a signed nanosecond count.
    #[must_use]
    pub const fn from_nanos(nanos: i128) -> Self {
        let mut sec = nanos.div_euclid(NANOS_PER_SEC_I128);
        let nanosec = nanos.rem_euclid(NANOS_PER_SEC_I128);
        if sec > i32::MAX as i128 {
            sec = i32::MAX as i128;
        } else if sec < i32::MIN as i128 {
            sec = i32::MIN as i128;
        }
        Self {
            sec: sec as i32,
            nanosec: nanosec as u32,
        }
    }

    /// Build an interval from whole seconds.
    #[must_use]
    pub const fn from_secs(sec: i32) -> Self {
        Self { sec, nanosec: 0 }
    }

    /// Build an interval from milliseconds.
    #[must_use]
    pub const fn from_millis(millis: i64) -> Self {
        Self::from_nanos((millis as i128).wrapping_mul(1_000_000))
    }

    /// The interval as a signed nanosecond count.
    #[must_use]
    pub const fn to_nanos(self) -> i128 {
        (self.sec as i128)
            .wrapping_mul(NANOS_PER_SEC_I128)
            .wrapping_add(self.nanosec as i128)
    }

    /// True for a zero-length interval.
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.sec == 0 && self.nanosec == 0
    }

    /// True when the interval runs backwards.
    #[must_use]
    pub const fn is_negative(self) -> bool {
        self.to_nanos() < 0
    }

    /// The interval as an unsigned `std` duration, or `None` when negative.
    #[must_use]
    pub fn to_std(self) -> Option<StdDuration> {
        let nanos = self.to_nanos();
        if nanos < 0 {
            return None;
        }
        u64::try_from(nanos).ok().map(StdDuration::from_nanos)
    }

    /// Read an unsigned `std` duration.
    #[must_use]
    pub fn from_std(duration: StdDuration) -> Self {
        Self::from_nanos(i128::from(
            duration.as_nanos().min(u128::from(u64::MAX)) as u64
        ))
    }

    /// Read a DDS duration, with `INFINITE` mapping to [`RosDuration::MAX`].
    ///
    /// ROS has no infinite duration; `rmw` uses a very large one, and
    /// saturating is what every consumer already expects.
    #[must_use]
    pub const fn from_dds(duration: DdsDuration) -> Self {
        if duration.is_infinite() {
            return Self::MAX;
        }
        Self::new(duration.sec, duration.nanosec)
    }

    /// Render as a DDS duration.
    #[must_use]
    pub const fn to_dds(self) -> DdsDuration {
        DdsDuration::new(self.sec, self.nanosec)
    }

    /// Read the generated wire type.
    #[must_use]
    pub const fn from_message(message: &builtin_interfaces::Duration) -> Self {
        Self::new(message.sec, message.nanosec)
    }

    /// Render as the generated wire type.
    #[must_use]
    pub const fn to_message(self) -> builtin_interfaces::Duration {
        builtin_interfaces::Duration {
            sec: self.sec,
            nanosec: self.nanosec,
        }
    }

    /// Add two intervals, saturating.
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self::from_nanos(self.to_nanos().saturating_add(other.to_nanos()))
    }

    /// Subtract two intervals, saturating.
    #[must_use]
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self::from_nanos(self.to_nanos().saturating_sub(other.to_nanos()))
    }
}

impl PartialOrd for RosDuration {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RosDuration {
    fn cmp(&self, other: &Self) -> Ordering {
        self.to_nanos().cmp(&other.to_nanos())
    }
}

impl fmt::Display for RosDuration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{:09}s", self.sec, self.nanosec)
    }
}

impl From<builtin_interfaces::Duration> for RosDuration {
    fn from(message: builtin_interfaces::Duration) -> Self {
        Self::from_message(&message)
    }
}

impl From<RosDuration> for builtin_interfaces::Duration {
    fn from(duration: RosDuration) -> Self {
        duration.to_message()
    }
}

impl From<StdDuration> for RosDuration {
    fn from(duration: StdDuration) -> Self {
        Self::from_std(duration)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_time_normalizes_an_over_large_nanosecond_field() {
        let time = RosTime::new(1, 1_500_000_000);
        assert_eq!(time.sec, 2);
        assert_eq!(time.nanosec, 500_000_000);
    }

    #[test]
    fn nanoseconds_round_trip_in_both_directions() {
        for nanos in [
            0_i128,
            1,
            999_999_999,
            1_000_000_000,
            1_700_000_000_500_000_000,
        ] {
            assert_eq!(RosTime::from_nanos(nanos).to_nanos(), nanos);
        }
    }

    #[test]
    fn a_negative_time_stays_negative_and_keeps_a_positive_nanosec() {
        let time = RosTime::from_nanos(-1_500_000_000);
        assert_eq!(time.sec, -2);
        assert_eq!(
            time.nanosec, 500_000_000,
            "the nanosecond field is unsigned, so the seconds carry the sign"
        );
        assert_eq!(time.to_nanos(), -1_500_000_000);
        assert!(!time.is_positive());
    }

    #[test]
    fn a_negative_time_has_no_hlc_form() {
        assert_eq!(RosTime::from_nanos(-1).to_hlc(), None);
        assert!(RosTime::from_nanos(0).to_hlc().is_some());
    }

    #[test]
    fn the_hlc_round_trip_drops_only_the_logical_counter() {
        let physical = 1_700_000_000_500_000_000_u64;
        let plain = HlcTimestamp::new(physical, 0);
        let ticked = HlcTimestamp::new(physical, 7);

        assert_eq!(RosTime::from_hlc(plain), RosTime::from_hlc(ticked));
        assert_eq!(RosTime::from_hlc(plain).to_hlc(), Some(plain));
        assert_ne!(RosTime::from_hlc(ticked).to_hlc(), Some(ticked));

        assert!(RosTime::round_trips_losslessly(plain));
        assert!(!RosTime::round_trips_losslessly(ticked));
    }

    #[test]
    fn an_hlc_stamp_past_the_int32_seconds_ceiling_does_not_round_trip() {
        let far_future = HlcTimestamp::new(u64::MAX, 0);
        assert!(!RosTime::round_trips_losslessly(far_future));
        assert_eq!(
            RosTime::from_hlc(far_future).sec,
            i32::MAX,
            "saturating is visible rather than wrapping into the past"
        );
    }

    #[test]
    fn the_rtps_conversion_is_accurate_to_under_a_nanosecond() {
        for nanos in [0_i128, 1_000_000_000, 1_700_000_000_123_456_789] {
            let time = RosTime::from_nanos(nanos);
            let back = RosTime::from_rtps(time.to_rtps());
            let error = (back.to_nanos() - nanos).abs();
            assert!(error <= 1, "{nanos} drifted by {error}ns");
        }
    }

    #[test]
    fn the_generated_message_types_convert_both_ways() {
        let time = RosTime::new(17, 250_000_000);
        let message: builtin_interfaces::Time = time.into();
        assert_eq!(message.sec, 17);
        assert_eq!(message.nanosec, 250_000_000);
        assert_eq!(RosTime::from(message), time);

        let duration = RosDuration::from_millis(1_250);
        let message: builtin_interfaces::Duration = duration.into();
        assert_eq!(message.sec, 1);
        assert_eq!(message.nanosec, 250_000_000);
        assert_eq!(RosDuration::from(message), duration);
    }

    #[test]
    fn times_order_by_instant_not_by_field() {
        let early = RosTime::new(1, 999_999_999);
        let late = RosTime::new(2, 0);
        assert!(early < late);
        assert_eq!(early.max(late), late);
        assert_eq!(RosTime::MIN.cmp(&RosTime::MAX), Ordering::Less);
    }

    #[test]
    fn subtracting_two_times_gives_a_signed_duration() {
        let early = RosTime::new(10, 0);
        let late = RosTime::new(12, 500_000_000);
        assert_eq!(late.since(early), RosDuration::from_millis(2_500));
        assert!(early.since(late).is_negative());
        assert_eq!(early.since(late).to_nanos(), -2_500_000_000);
    }

    #[test]
    fn adding_and_subtracting_a_duration_are_inverses() {
        let time = RosTime::new(100, 250_000_000);
        let step = RosDuration::from_millis(1_500);
        assert_eq!(time.saturating_add(step).saturating_sub(step), time);
    }

    #[test]
    fn saturating_arithmetic_never_wraps() {
        assert_eq!(RosTime::MAX.saturating_add(RosDuration::MAX).sec, i32::MAX);
        assert_eq!(RosTime::MIN.saturating_sub(RosDuration::MAX).sec, i32::MIN);
        assert_eq!(
            RosDuration::MAX.saturating_add(RosDuration::MAX).sec,
            i32::MAX
        );
    }

    #[test]
    fn a_negative_duration_has_no_std_form() {
        assert_eq!(RosDuration::from_millis(-1).to_std(), None);
        assert_eq!(
            RosDuration::from_millis(1_500).to_std(),
            Some(StdDuration::from_millis(1_500))
        );
        assert_eq!(
            RosDuration::from_std(StdDuration::from_millis(1_500)),
            RosDuration::from_millis(1_500)
        );
    }

    #[test]
    fn an_infinite_dds_duration_saturates_rather_than_becoming_zero() {
        assert_eq!(
            RosDuration::from_dds(DdsDuration::INFINITE),
            RosDuration::MAX
        );
        assert_eq!(
            RosDuration::from_dds(DdsDuration::from_millis(250)),
            RosDuration::from_millis(250)
        );
        assert_eq!(
            RosDuration::from_millis(250).to_dds(),
            DdsDuration::from_millis(250)
        );
    }

    #[test]
    fn the_display_forms_are_the_ones_ros_tools_print() {
        assert_eq!(RosTime::new(17, 250_000_000).to_string(), "17.250000000");
        assert_eq!(RosTime::ZERO.to_string(), "0.000000000");
        assert_eq!(RosDuration::from_millis(1_500).to_string(), "1.500000000s");
    }

    #[test]
    fn the_zero_time_is_recognizable() {
        assert!(RosTime::ZERO.is_zero());
        assert!(RosTime::default().is_zero());
        assert!(!RosTime::new(0, 1).is_zero());
        assert!(RosDuration::ZERO.is_zero());
    }

    #[test]
    fn an_hlc_timestamp_converts_through_the_from_impl() {
        let stamp = HlcTimestamp::new(5_000_000_000, 0);
        assert_eq!(RosTime::from(stamp), RosTime::new(5, 0));
    }
}
