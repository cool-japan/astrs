//! [`TfStamp`] and [`TimePoint`] — the time domain the transform buffer's
//! interpolated, time-travel lookup (blueprint §10.6) is keyed on.
//!
//! # Why not `astrs_time::HlcTimestamp`
//!
//! `astrs-time`'s [`HlcTimestamp`] is the event-envelope currency the rest
//! of AstRS's merged event loop uses (blueprint §4.3), and it is tempting to
//! reuse it here rather than mint a second time type. It does not fit:
//!
//! - `HlcTimestamp::physical_ns` is a `u64` — nanoseconds since the UNIX
//!   epoch, unconditionally non-negative. tf2's own time domain
//!   (`tf2::TimePoint`, and `rclcpp::Time`'s internal
//!   `rcl_time_point_value_t`) is a signed 64-bit nanosecond count
//!   precisely *because* ROS simulation time and pre-epoch test fixtures
//!   are allowed to be negative. A buffer keyed on `HlcTimestamp` could
//!   never represent them.
//! - `HlcTimestamp` carries a `logical: u32` tie-breaking counter for
//!   events that share a physical nanosecond in one process's causal
//!   order. tf2 has no such concept — two `TransformStamped` values that
//!   land on the same nanosecond are either identical (idempotent resend)
//!   or a genuine publisher conflict, never a logical-clock tie to break.
//!
//! [`TfStamp`] is therefore a bespoke `i64` nanosecond count, matching
//! `tf2::TimePoint`'s own range and sign exactly. [`From<HlcTimestamp>`] and
//! [`TfStamp::to_hlc_timestamp`] bridge the two domains where a caller
//! genuinely has one and needs the other (e.g. handing a looked-up
//! transform's stamp to `Stamped::new` for AstRS's own event plane), but
//! neither type is defined in terms of the other.
//!
//! # `TimePoint::Latest`
//!
//! tf2's C++ API overloads `ros::Time(0)` to mean "the most recent common
//! time available," a magic sentinel that collides with the legitimate
//! (if rare) timestamp zero. [`TimePoint`] makes the same contract an
//! explicit enum variant instead, so `TimePoint::At(TfStamp::EPOCH)` and
//! `TimePoint::Latest` stay distinguishable. "Most recent common time" is
//! doing real work in that phrase, not just avoiding the zero collision: a
//! lookup spanning more than one dynamic edge resolves `Latest` to one
//! shared instant common to all of them (`tf2::BufferCore::
//! getLatestCommonTime`'s own algorithm — see [`TimePoint::Latest`]'s own
//! docs for the full behavior, including the case, rare but real, where it
//! still fails as an extrapolation).

use std::fmt;
use std::time::Duration;

use astrs_idl::generated::builtin_interfaces;
use astrs_time::HlcTimestamp;

use crate::error::TfError;

/// A signed nanosecond count since the UNIX epoch — tf2's own time domain.
///
/// See the [module documentation](self) for why this is not
/// `astrs_time::HlcTimestamp`. `Ord`-comparable, `Copy`, and cheap
/// everywhere: the transform buffer's whole interpolation/extrapolation
/// machinery is built on comparing and subtracting these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TfStamp(i64);

impl TfStamp {
    /// The UNIX epoch itself: `1970-01-01T00:00:00Z`.
    pub const EPOCH: Self = Self(0);

    /// The earliest representable stamp.
    pub const MIN: Self = Self(i64::MIN);

    /// The latest representable stamp.
    pub const MAX: Self = Self(i64::MAX);

    /// Builds a stamp directly from a signed nanosecond count since the
    /// UNIX epoch.
    #[must_use]
    pub const fn from_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    /// The signed nanosecond count since the UNIX epoch.
    #[must_use]
    pub const fn as_nanos(self) -> i64 {
        self.0
    }

    /// Widens a ROS 2 `builtin_interfaces/Time` (`sec: i32`, `nanosec: u32`)
    /// into a [`TfStamp`]. Always exact: `i32` seconds and a sub-second
    /// `u32` nanosecond component both fit an `i64` nanosecond count with
    /// wide margin.
    ///
    /// ```
    /// use astrs_tf::TfStamp;
    /// use astrs_idl::generated::builtin_interfaces::Time;
    ///
    /// let time = Time { sec: 5, nanosec: 250_000_000 };
    /// let stamp = TfStamp::from_ros_time(&time);
    /// assert_eq!(stamp.as_nanos(), 5_250_000_000);
    /// ```
    #[must_use]
    pub const fn from_ros_time(time: &builtin_interfaces::Time) -> Self {
        Self((time.sec as i64) * 1_000_000_000 + time.nanosec as i64)
    }

    /// Narrows this stamp into a ROS 2 `builtin_interfaces/Time`.
    ///
    /// Splits into whole seconds and a `[0, 1e9)` nanosecond remainder via
    /// Euclidean division, so the result is well-formed even for a negative
    /// `self` (`-1ns` becomes `sec: -1, nanosec: 999_999_999`, not
    /// `sec: 0, nanosec: -1`, which `nanosec: u32` cannot express anyway).
    ///
    /// # Errors
    ///
    /// [`TfError::RosTimeRangeExceeded`] when the whole-seconds component
    /// does not fit `i32` — the same Y2038-shaped ceiling
    /// `builtin_interfaces/Time.sec` carries on the wire regardless of
    /// which implementation produces it.
    ///
    /// ```
    /// use astrs_tf::TfStamp;
    ///
    /// # fn main() -> Result<(), astrs_tf::TfError> {
    /// let time = TfStamp::from_nanos(5_250_000_000).to_ros_time()?;
    /// assert_eq!(time.sec, 5);
    /// assert_eq!(time.nanosec, 250_000_000);
    ///
    /// // The whole-seconds component must fit `i32`.
    /// assert!(TfStamp::MAX.to_ros_time().is_err());
    /// # Ok(())
    /// # }
    /// ```
    pub fn to_ros_time(self) -> Result<builtin_interfaces::Time, TfError> {
        const NANOS_PER_SEC: i64 = 1_000_000_000;
        let sec_i64 = self.0.div_euclid(NANOS_PER_SEC);
        let nanosec = self.0.rem_euclid(NANOS_PER_SEC);
        let sec = i32::try_from(sec_i64)
            .map_err(|_source| TfError::RosTimeRangeExceeded { nanos: self.0 })?;
        // `rem_euclid` against a positive divisor is always in `[0,
        // NANOS_PER_SEC)`, which fits `u32` with wide margin.
        let nanosec = u32::try_from(nanosec).unwrap_or(0);
        Ok(builtin_interfaces::Time { sec, nanosec })
    }

    /// This stamp shifted forward by `delta`, clamped to [`TfStamp::MAX`]
    /// rather than overflowing.
    #[must_use]
    pub fn saturating_add(self, delta: Duration) -> Self {
        Self(self.0.saturating_add(duration_as_i64_nanos(delta)))
    }

    /// This stamp shifted backward by `delta`, clamped to [`TfStamp::MIN`]
    /// rather than overflowing.
    #[must_use]
    pub fn saturating_sub(self, delta: Duration) -> Self {
        Self(self.0.saturating_sub(duration_as_i64_nanos(delta)))
    }

    /// The signed nanosecond count from `earlier` to `self`
    /// (`self - earlier`), clamped to `i64`'s range rather than
    /// overflowing.
    ///
    /// Unlike [`Duration`]-returning subtraction this is signed and total:
    /// callers computing an interpolation fraction do not need to know in
    /// advance which of two stamps is earlier.
    #[must_use]
    pub fn signed_nanos_since(self, earlier: Self) -> i64 {
        self.0.saturating_sub(earlier.0)
    }

    /// Bridges to an [`HlcTimestamp`] for AstRS's own event plane
    /// (blueprint §4.3), when this stamp is not negative.
    ///
    /// Returns `None` for a `self` before the UNIX epoch — `HlcTimestamp`'s
    /// `physical_ns` is unconditionally non-negative (see the
    /// [module documentation](self)), so a pre-epoch `TfStamp` (legal for
    /// simulation time) has no representation. This is not an error
    /// condition, just a domain mismatch, hence `Option` rather than
    /// [`TfError`].
    #[must_use]
    pub fn to_hlc_timestamp(self) -> Option<HlcTimestamp> {
        u64::try_from(self.0)
            .ok()
            .map(|physical_ns| HlcTimestamp::new(physical_ns, 0))
    }
}

/// `Duration::as_nanos` returns `u128`; every real `Duration` this crate
/// ever adds or subtracts is a bounded history window (seconds, not
/// centuries), so the clamp to `i64::MAX` only ever engages for a
/// pathological caller-supplied window, never in practice.
fn duration_as_i64_nanos(delta: Duration) -> i64 {
    i64::try_from(delta.as_nanos()).unwrap_or(i64::MAX)
}

impl fmt::Display for TfStamp {
    /// Formats as a bare signed nanosecond count, e.g. `"1771286400123456789"`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<HlcTimestamp> for TfStamp {
    /// Widens `ts.physical_ns()` into a [`TfStamp`], discarding the logical
    /// counter (tf2's time domain has no equivalent — see the
    /// [module documentation](self)). Clamps to [`TfStamp::MAX`] rather
    /// than overflowing, which only engages within the last ~292 years of
    /// `u64` nanosecond range.
    fn from(ts: HlcTimestamp) -> Self {
        Self(i64::try_from(ts.physical_ns()).unwrap_or(i64::MAX))
    }
}

impl From<builtin_interfaces::Time> for TfStamp {
    fn from(time: builtin_interfaces::Time) -> Self {
        Self::from_ros_time(&time)
    }
}

/// A query time for [`crate::buffer::TransformBuffer::lookup_transform`].
///
/// See the [module documentation](self) for why `Latest` is a distinct
/// variant rather than a magic sentinel timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimePoint {
    /// tf2's `ros::Time(0)` convention — resolved to one shared instant
    /// before the walk runs, not "whatever's freshest" per hop.
    ///
    /// For a lookup whose walk touches **at most one** dynamic edge (the
    /// common case: an all-static chain, or exactly one moving frame), this
    /// is exactly the newest sample available, un-interpolated, and never
    /// an extrapolation error.
    ///
    /// For a lookup whose walk touches **two or more** dynamic edges, tf2
    /// does not let each edge report its own independently freshest
    /// sample — doing so could compose transforms that never coexisted at
    /// any single real instant (e.g. a fast-updating base link at its
    /// newest sample composed with a slow-updating sensor mount still
    /// several seconds behind). Instead, [`crate::buffer::TransformBuffer::lookup_transform`]
    /// resolves `Latest` to the *latest common time* — the minimum, over
    /// every dynamic edge the walk touches, of that edge's own newest
    /// sample — and evaluates every edge at that one shared instant. This
    /// can mean an edge whose own history extends further is interpolated
    /// *backward* to match, and can (rarely) still fail with
    /// [`TfError::Extrapolation`] if the shared instant falls before some
    /// edge's oldest buffered sample — matching `tf2::BufferCore::
    /// getLatestCommonTime`'s own documented behavior and limitation, not
    /// a new one introduced here.
    Latest,
    /// A specific instant; may interpolate between bracketing samples or
    /// fail with [`TfError::Extrapolation`] if it falls outside the
    /// buffer's retained history.
    At(TfStamp),
}

impl From<TfStamp> for TimePoint {
    fn from(stamp: TfStamp) -> Self {
        Self::At(stamp)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use proptest::prelude::*;

    #[test]
    fn ros_time_round_trips_through_tf_stamp() {
        let time = builtin_interfaces::Time {
            sec: 1_771_286_400,
            nanosec: 123_456_789,
        };
        let stamp = TfStamp::from_ros_time(&time);
        assert_eq!(stamp.as_nanos(), 1_771_286_400_123_456_789);
        assert_eq!(stamp.to_ros_time().unwrap(), time);
    }

    #[test]
    fn negative_stamp_splits_into_euclidean_sec_and_nanosec() {
        // -1ns is one nanosecond before the epoch: sec -1, nanosec
        // 999_999_999 (never a negative `nanosec`, which `u32` cannot hold).
        let stamp = TfStamp::from_nanos(-1);
        let time = stamp.to_ros_time().unwrap();
        assert_eq!(time.sec, -1);
        assert_eq!(time.nanosec, 999_999_999);
        assert_eq!(TfStamp::from_ros_time(&time), stamp);
    }

    #[test]
    fn to_ros_time_rejects_a_whole_seconds_overflow() {
        let stamp = TfStamp::from_nanos(i64::MAX);
        assert_eq!(
            stamp.to_ros_time(),
            Err(TfError::RosTimeRangeExceeded { nanos: i64::MAX })
        );
    }

    #[test]
    fn saturating_add_and_sub_shift_by_the_duration() {
        let stamp = TfStamp::from_nanos(1_000);
        assert_eq!(
            stamp.saturating_add(Duration::from_nanos(500)).as_nanos(),
            1_500
        );
        assert_eq!(
            stamp.saturating_sub(Duration::from_nanos(500)).as_nanos(),
            500
        );
    }

    #[test]
    fn saturating_add_clamps_at_the_maximum() {
        let stamp = TfStamp::MAX;
        assert_eq!(stamp.saturating_add(Duration::from_secs(1)), TfStamp::MAX);
    }

    #[test]
    fn saturating_sub_clamps_at_the_minimum() {
        let stamp = TfStamp::MIN;
        assert_eq!(stamp.saturating_sub(Duration::from_secs(1)), TfStamp::MIN);
    }

    #[test]
    fn signed_nanos_since_is_negative_when_self_is_earlier() {
        let a = TfStamp::from_nanos(1_000);
        let b = TfStamp::from_nanos(1_500);
        assert_eq!(b.signed_nanos_since(a), 500);
        assert_eq!(a.signed_nanos_since(b), -500);
    }

    #[test]
    fn hlc_timestamp_bridges_in_both_directions_for_non_negative_stamps() {
        let hlc = HlcTimestamp::new(1_771_286_400_123_456_789, 7);
        let stamp = TfStamp::from(hlc);
        assert_eq!(stamp.as_nanos(), 1_771_286_400_123_456_789);
        // The logical counter has no tf2 equivalent and is discarded.
        assert_eq!(
            stamp.to_hlc_timestamp(),
            Some(HlcTimestamp::new(1_771_286_400_123_456_789, 0))
        );
    }

    #[test]
    fn a_pre_epoch_stamp_has_no_hlc_timestamp() {
        let stamp = TfStamp::from_nanos(-1);
        assert_eq!(stamp.to_hlc_timestamp(), None);
    }

    #[test]
    fn display_is_a_bare_nanosecond_count() {
        assert_eq!(TfStamp::from_nanos(1_000).to_string(), "1000");
        assert_eq!(TfStamp::from_nanos(-1).to_string(), "-1");
    }

    #[test]
    fn time_point_from_tf_stamp_wraps_at() {
        let stamp = TfStamp::from_nanos(42);
        assert_eq!(TimePoint::from(stamp), TimePoint::At(stamp));
    }

    proptest! {
        /// Any ROS time whose seconds component is a valid `i32` round trips
        /// exactly through [`TfStamp`] regardless of nanosecond value.
        #[test]
        fn ros_time_round_trip_is_exact(
            sec in i32::MIN..i32::MAX,
            nanosec in 0u32..1_000_000_000u32,
        ) {
            let time = builtin_interfaces::Time { sec, nanosec };
            let stamp = TfStamp::from_ros_time(&time);
            prop_assert_eq!(stamp.to_ros_time().unwrap(), time);
        }
    }
}
