//! Timestamps and durations.
//!
//! RTPS keeps time in the NTP shape: a signed count of seconds and an
//! unsigned binary fraction of a second, each four octets (OMG DDSI-RTPS 2.3
//! §9.4.2.9). One `fraction` unit is 2⁻³² s ≈ 233 ps, so the representation
//! is finer than nanoseconds and the conversions here round rather than
//! truncate.
//!
//! ```text
//! +--------+--------+--------+--------+
//! |      seconds (long, signed)       |
//! +--------+--------+--------+--------+
//! |      fraction (unsigned long)     |
//! +--------+--------+--------+--------+
//! ```
//!
//! # Two different `Duration_t`s
//!
//! This is the trap the module exists to make explicit.
//!
//! - [`Duration`] is the **RTPS** `Duration_t`: seconds plus a 2⁻³²
//!   fraction, the same layout as [`Time`]. It is what
//!   `PID_PARTICIPANT_LEASE_DURATION` carries in an SPDP announcement.
//! - [`DdsDuration`] is the **DDS** `Duration_t`: seconds plus *nanoseconds*.
//!   It is what the QoS parameters carry — `PID_DEADLINE`, `PID_LIFESPAN`,
//!   `PID_LATENCY_BUDGET` — and its infinite value is
//!   `{0x7fffffff, 0xffffffff}`.
//!
//! Both are eight octets and both are two 32-bit fields, so a decoder that
//! confuses them produces a number roughly 4.29 times too large rather than
//! an error. Keeping them apart in the type system is the only defence.
//!
//! ```
//! use astrs_rtps::structure::{DdsDuration, Duration, Time};
//!
//! // Half a second, three ways.
//! assert_eq!(Time::new(1, 1 << 31).subsec_nanos(), 500_000_000);
//! assert_eq!(Duration::from_millis(500).fraction(), 1 << 31);
//! assert_eq!(DdsDuration::from_millis(500).nanosec, 500_000_000);
//! ```

use core::fmt;
use std::time::Duration as StdDuration;

use astrs_cdr::{CdrDeserialize, CdrReader, CdrResult, CdrSerialize, CdrType, CdrWriter};

/// Octets a [`Time`], a [`Duration`] or a [`DdsDuration`] occupies.
pub const TIME_LEN: usize = 8;

/// Nanoseconds in a second, as the conversions use it.
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// `2^32`, the denominator of the RTPS fraction field.
const FRACTION_SCALE: u64 = 1_u64 << 32;

/// Convert a binary fraction of a second into nanoseconds, rounding to
/// nearest.
///
/// The result is clamped to `999_999_999`: the largest fraction,
/// `0xffff_ffff`, rounds to a whole second, and a "sub-second" accessor that
/// could return one second would break every caller that adds it to a seconds
/// field.
const fn fraction_to_nanos(fraction: u32) -> u32 {
    let scaled = (fraction as u64) * NANOS_PER_SEC + FRACTION_SCALE / 2;
    let nanos = scaled / FRACTION_SCALE;
    if nanos >= NANOS_PER_SEC {
        (NANOS_PER_SEC - 1) as u32
    } else {
        nanos as u32
    }
}

/// Convert nanoseconds into a binary fraction of a second, rounding to
/// nearest.
///
/// Values at or above one second saturate at `u32::MAX`, which is the
/// largest fraction the field can hold.
const fn nanos_to_fraction(nanos: u32) -> u32 {
    let scaled = (nanos as u64) * FRACTION_SCALE + NANOS_PER_SEC / 2;
    let fraction = scaled / NANOS_PER_SEC;
    if fraction > u32::MAX as u64 {
        u32::MAX
    } else {
        fraction as u32
    }
}

/// An RTPS timestamp: `Time_t` (§9.4.2.9).
///
/// The epoch is not fixed by RTPS — a participant's `INFO_TS` says only that
/// the samples that follow were written at this reading of *its* clock.
/// Interoperating stacks use the Unix epoch, and so does AstRS
/// ([`Time::from_unix_nanos`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Time {
    /// Whole seconds, signed.
    pub seconds: i32,
    /// Fraction of a second, in units of 2⁻³² s.
    pub fraction: u32,
}

impl Time {
    /// `TIME_ZERO`.
    pub const ZERO: Self = Self::new(0, 0);

    /// `TIME_INVALID` — `seconds = -1`, `fraction = 0xffffffff`.
    pub const INVALID: Self = Self::new(-1, u32::MAX);

    /// `TIME_INFINITE` — `seconds = 0x7fffffff`, `fraction = 0xffffffff`.
    pub const INFINITE: Self = Self::new(i32::MAX, u32::MAX);

    /// Build a timestamp from its two fields.
    #[must_use]
    pub const fn new(seconds: i32, fraction: u32) -> Self {
        Self { seconds, fraction }
    }

    /// Build a timestamp from seconds and nanoseconds, rounding the
    /// nanoseconds into the binary fraction.
    ///
    /// Nanoseconds at or above one second are folded into `seconds`.
    #[must_use]
    pub const fn from_secs_nanos(seconds: i32, nanos: u32) -> Self {
        let carry = (nanos as u64) / NANOS_PER_SEC;
        let remainder = (nanos as u64) % NANOS_PER_SEC;
        Self {
            seconds: seconds.saturating_add(carry as i32),
            fraction: nanos_to_fraction(remainder as u32),
        }
    }

    /// Build a timestamp from nanoseconds since the Unix epoch.
    ///
    /// Seconds beyond the 32-bit field saturate, which puts the year-2038
    /// boundary in the type rather than in silent wrap-around.
    #[must_use]
    pub const fn from_unix_nanos(nanos: i128) -> Self {
        let seconds = nanos.div_euclid(NANOS_PER_SEC as i128);
        let remainder = nanos.rem_euclid(NANOS_PER_SEC as i128) as u32;
        let seconds = if seconds > i32::MAX as i128 {
            i32::MAX
        } else if seconds < i32::MIN as i128 {
            i32::MIN
        } else {
            seconds as i32
        };
        Self {
            seconds,
            fraction: nanos_to_fraction(remainder),
        }
    }

    /// The fraction field expressed in nanoseconds.
    #[must_use]
    pub const fn subsec_nanos(self) -> u32 {
        fraction_to_nanos(self.fraction)
    }

    /// The whole timestamp in nanoseconds, on whatever epoch the sender used.
    #[must_use]
    pub const fn to_nanos(self) -> i128 {
        (self.seconds as i128) * (NANOS_PER_SEC as i128) + (self.subsec_nanos() as i128)
    }

    /// True for [`Time::INVALID`].
    #[must_use]
    pub const fn is_invalid(self) -> bool {
        self.seconds == Self::INVALID.seconds && self.fraction == Self::INVALID.fraction
    }

    /// True for [`Time::INFINITE`].
    #[must_use]
    pub const fn is_infinite(self) -> bool {
        self.seconds == Self::INFINITE.seconds && self.fraction == Self::INFINITE.fraction
    }

    /// True for [`Time::ZERO`].
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.seconds == 0 && self.fraction == 0
    }
}

impl Default for Time {
    /// [`Time::ZERO`].
    fn default() -> Self {
        Self::ZERO
    }
}

impl fmt::Display for Time {
    /// `12.500000000` — seconds and the fraction rendered as nanoseconds.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_invalid() {
            return f.write_str("TIME_INVALID");
        }
        if self.is_infinite() {
            return f.write_str("TIME_INFINITE");
        }
        write!(f, "{}.{:09}", self.seconds, self.subsec_nanos())
    }
}

impl CdrType for Time {
    const MIN_SERIALIZED_SIZE: usize = TIME_LEN;
}

impl CdrSerialize for Time {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_i32(self.seconds)?;
        writer.write_u32(self.fraction)
    }
}

impl<'de> CdrDeserialize<'de> for Time {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        let seconds = reader.read_i32()?;
        let fraction = reader.read_u32()?;
        Ok(Self { seconds, fraction })
    }
}

/// An RTPS duration: `Duration_t` (§9.4.2.9), the same layout as [`Time`].
///
/// This is the type `PID_PARTICIPANT_LEASE_DURATION` carries. For the DDS QoS
/// durations, which count nanoseconds instead of a binary fraction, see
/// [`DdsDuration`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Duration {
    /// Whole seconds, signed.
    pub seconds: i32,
    /// Fraction of a second, in units of 2⁻³² s.
    pub fraction: u32,
}

impl Duration {
    /// `DURATION_ZERO`.
    pub const ZERO: Self = Self::new(0, 0);

    /// `DURATION_INFINITE` — `seconds = 0x7fffffff`, `fraction = 0xffffffff`.
    pub const INFINITE: Self = Self::new(i32::MAX, u32::MAX);

    /// The lease duration a participant announces when nothing else was
    /// configured: 100 seconds, the value §8.5.3.2 gives as the default for
    /// `PARTICIPANT_LEASE_DURATION`.
    pub const DEFAULT_PARTICIPANT_LEASE: Self = Self::from_secs(100);

    /// Build a duration from its two fields.
    #[must_use]
    pub const fn new(seconds: i32, fraction: u32) -> Self {
        Self { seconds, fraction }
    }

    /// A whole number of seconds.
    #[must_use]
    pub const fn from_secs(seconds: i32) -> Self {
        Self::new(seconds, 0)
    }

    /// A whole number of milliseconds.
    #[must_use]
    pub const fn from_millis(millis: u32) -> Self {
        let seconds = millis / 1_000;
        let remainder = millis % 1_000;
        Self {
            seconds: seconds as i32,
            fraction: nanos_to_fraction(remainder * 1_000_000),
        }
    }

    /// A duration from seconds and nanoseconds.
    #[must_use]
    pub const fn from_secs_nanos(seconds: i32, nanos: u32) -> Self {
        let time = Time::from_secs_nanos(seconds, nanos);
        Self::new(time.seconds, time.fraction)
    }

    /// The fraction field, in units of 2⁻³² s.
    #[must_use]
    pub const fn fraction(self) -> u32 {
        self.fraction
    }

    /// The fraction field expressed in nanoseconds.
    #[must_use]
    pub const fn subsec_nanos(self) -> u32 {
        fraction_to_nanos(self.fraction)
    }

    /// True for [`Duration::INFINITE`].
    #[must_use]
    pub const fn is_infinite(self) -> bool {
        self.seconds == Self::INFINITE.seconds && self.fraction == Self::INFINITE.fraction
    }

    /// True for [`Duration::ZERO`].
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.seconds == 0 && self.fraction == 0
    }

    /// The equivalent [`std::time::Duration`], or `None` for a negative or
    /// infinite value.
    #[must_use]
    pub const fn to_std(self) -> Option<StdDuration> {
        if self.is_infinite() || self.seconds < 0 {
            return None;
        }
        Some(StdDuration::new(self.seconds as u64, self.subsec_nanos()))
    }

    /// The RTPS form of a [`std::time::Duration`], saturating at
    /// `i32::MAX` seconds.
    #[must_use]
    pub const fn from_std(duration: StdDuration) -> Self {
        let seconds = duration.as_secs();
        let seconds = if seconds > i32::MAX as u64 {
            i32::MAX
        } else {
            seconds as i32
        };
        Self {
            seconds,
            fraction: nanos_to_fraction(duration.subsec_nanos()),
        }
    }
}

impl Default for Duration {
    /// [`Duration::ZERO`].
    fn default() -> Self {
        Self::ZERO
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_infinite() {
            return f.write_str("DURATION_INFINITE");
        }
        write!(f, "{}.{:09}s", self.seconds, self.subsec_nanos())
    }
}

impl From<StdDuration> for Duration {
    fn from(duration: StdDuration) -> Self {
        Self::from_std(duration)
    }
}

impl CdrType for Duration {
    const MIN_SERIALIZED_SIZE: usize = TIME_LEN;
}

impl CdrSerialize for Duration {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_i32(self.seconds)?;
        writer.write_u32(self.fraction)
    }
}

impl<'de> CdrDeserialize<'de> for Duration {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        let seconds = reader.read_i32()?;
        let fraction = reader.read_u32()?;
        Ok(Self { seconds, fraction })
    }
}

/// The DDS QoS `Duration_t`: seconds and **nanoseconds**.
///
/// Distinct from [`Duration`] on purpose — see the [module
/// documentation](self). This is the type inside a `PID_DEADLINE`,
/// `PID_LIFESPAN`, `PID_LATENCY_BUDGET` or `PID_LIVELINESS` parameter value,
/// and its infinite value is `{0x7fffffff, 0xffffffff}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DdsDuration {
    /// Whole seconds, signed.
    pub sec: i32,
    /// Nanoseconds within the second.
    pub nanosec: u32,
}

impl DdsDuration {
    /// `DURATION_ZERO_SEC` / `DURATION_ZERO_NSEC`.
    pub const ZERO: Self = Self::new(0, 0);

    /// `DURATION_INFINITE_SEC` / `DURATION_INFINITE_NSEC`.
    ///
    /// The value every "no limit" QoS carries: an infinite deadline, an
    /// infinite lifespan, an infinite lease.
    pub const INFINITE: Self = Self::new(i32::MAX, u32::MAX);

    /// Build a duration from its two fields.
    #[must_use]
    pub const fn new(sec: i32, nanosec: u32) -> Self {
        Self { sec, nanosec }
    }

    /// A whole number of seconds.
    #[must_use]
    pub const fn from_secs(sec: i32) -> Self {
        Self::new(sec, 0)
    }

    /// A whole number of milliseconds.
    #[must_use]
    pub const fn from_millis(millis: u32) -> Self {
        Self::new((millis / 1_000) as i32, (millis % 1_000) * 1_000_000)
    }

    /// True for [`DdsDuration::INFINITE`].
    #[must_use]
    pub const fn is_infinite(self) -> bool {
        self.sec == Self::INFINITE.sec && self.nanosec == Self::INFINITE.nanosec
    }

    /// True for [`DdsDuration::ZERO`].
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.sec == 0 && self.nanosec == 0
    }

    /// The equivalent [`std::time::Duration`], or `None` for a negative or
    /// infinite value.
    #[must_use]
    pub const fn to_std(self) -> Option<StdDuration> {
        if self.is_infinite() || self.sec < 0 {
            return None;
        }
        Some(StdDuration::new(self.sec as u64, self.nanosec))
    }

    /// The DDS form of a [`std::time::Duration`], saturating at `i32::MAX`
    /// seconds.
    #[must_use]
    pub const fn from_std(duration: StdDuration) -> Self {
        let seconds = duration.as_secs();
        let sec = if seconds > i32::MAX as u64 {
            i32::MAX
        } else {
            seconds as i32
        };
        Self {
            sec,
            nanosec: duration.subsec_nanos(),
        }
    }

    /// The same interval expressed as an RTPS [`Duration`].
    ///
    /// The explicit bridge between the two `Duration_t`s, so a conversion is
    /// always a visible call rather than a struct-literal accident.
    #[must_use]
    pub const fn to_rtps(self) -> Duration {
        if self.is_infinite() {
            return Duration::INFINITE;
        }
        Duration::new(self.sec, nanos_to_fraction(self.nanosec))
    }

    /// The same interval read back from an RTPS [`Duration`].
    #[must_use]
    pub const fn from_rtps(duration: Duration) -> Self {
        if duration.is_infinite() {
            return Self::INFINITE;
        }
        Self::new(duration.seconds, duration.subsec_nanos())
    }
}

impl Default for DdsDuration {
    /// [`DdsDuration::ZERO`].
    fn default() -> Self {
        Self::ZERO
    }
}

impl fmt::Display for DdsDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_infinite() {
            return f.write_str("DURATION_INFINITE");
        }
        write!(f, "{}.{:09}s", self.sec, self.nanosec)
    }
}

impl CdrType for DdsDuration {
    const MIN_SERIALIZED_SIZE: usize = TIME_LEN;
}

impl CdrSerialize for DdsDuration {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_i32(self.sec)?;
        writer.write_u32(self.nanosec)
    }
}

impl<'de> CdrDeserialize<'de> for DdsDuration {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        let sec = reader.read_i32()?;
        let nanosec = reader.read_u32()?;
        Ok(Self { sec, nanosec })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use astrs_cdr::{Encoding, from_bytes_headerless, to_vec_headerless};

    use super::*;

    #[test]
    fn the_fraction_field_is_a_binary_fraction_of_a_second() {
        // 2^31 / 2^32 = 0.5 s exactly.
        assert_eq!(Time::new(0, 1 << 31).subsec_nanos(), 500_000_000);
        // 2^30 / 2^32 = 0.25 s exactly.
        assert_eq!(Time::new(0, 1 << 30).subsec_nanos(), 250_000_000);
        // 0xffff_ffff / 2^32 rounds to a whole second; the accessor clamps so
        // "sub-second" stays true.
        assert_eq!(Time::new(0, u32::MAX).subsec_nanos(), 999_999_999);
        assert_eq!(Time::new(0, 0).subsec_nanos(), 0);
    }

    #[test]
    fn nanoseconds_round_trip_through_the_fraction_to_the_nearest_unit() {
        for nanos in [0_u32, 1, 500_000_000, 999_999_999, 123_456_789] {
            let time = Time::from_secs_nanos(3, nanos);
            let recovered = time.subsec_nanos();
            let error = recovered.abs_diff(nanos);
            assert!(error <= 1, "{nanos} came back as {recovered}");
            assert_eq!(time.seconds, 3);
        }
    }

    #[test]
    fn nanoseconds_at_or_above_a_second_carry_into_the_seconds_field() {
        let time = Time::from_secs_nanos(1, 2_500_000_000);
        assert_eq!(time.seconds, 3);
        assert_eq!(time.subsec_nanos(), 500_000_000);
    }

    #[test]
    fn the_sentinel_timestamps_are_the_ones_the_specification_names() {
        assert_eq!(Time::ZERO, Time::new(0, 0));
        assert!(Time::ZERO.is_zero());
        assert_eq!(Time::INVALID, Time::new(-1, 0xffff_ffff));
        assert!(Time::INVALID.is_invalid());
        assert_eq!(Time::INFINITE, Time::new(0x7fff_ffff, 0xffff_ffff));
        assert!(Time::INFINITE.is_infinite());
        assert_eq!(Time::default(), Time::ZERO);
        assert_eq!(Time::INVALID.to_string(), "TIME_INVALID");
        assert_eq!(Time::INFINITE.to_string(), "TIME_INFINITE");
        assert_eq!(Time::new(12, 1 << 31).to_string(), "12.500000000");
    }

    #[test]
    fn unix_nanoseconds_convert_in_both_directions() {
        let time = Time::from_unix_nanos(1_700_000_000_500_000_000);
        assert_eq!(time.seconds, 1_700_000_000);
        assert_eq!(time.subsec_nanos(), 500_000_000);
        assert_eq!(time.to_nanos(), 1_700_000_000_500_000_000);

        // Negative instants use Euclidean division, so the fraction stays
        // non-negative: -0.5 s is seconds = -1, fraction = 0.5.
        let before = Time::from_unix_nanos(-500_000_000);
        assert_eq!(before.seconds, -1);
        assert_eq!(before.subsec_nanos(), 500_000_000);

        // Beyond 2038 the seconds field saturates rather than wrapping.
        let far = Time::from_unix_nanos(i128::from(i64::MAX));
        assert_eq!(far.seconds, i32::MAX);
    }

    #[test]
    fn a_timestamp_is_eight_octets_seconds_then_fraction() {
        // §9.4.2.9: both fields take the stream's byte order.
        let time = Time::new(0x0102_0304, 0x8000_0000);
        assert_eq!(
            to_vec_headerless(&time, Encoding::ROS2).expect("encode"),
            [0x04, 0x03, 0x02, 0x01, 0x00, 0x00, 0x00, 0x80]
        );
        assert_eq!(
            to_vec_headerless(&time, Encoding::new(astrs_cdr::EncapsulationKind::CdrBe))
                .expect("encode"),
            [0x01, 0x02, 0x03, 0x04, 0x80, 0x00, 0x00, 0x00]
        );
        let bytes = to_vec_headerless(&time, Encoding::ROS2).expect("encode");
        assert_eq!(
            from_bytes_headerless::<Time>(&bytes, Encoding::ROS2).expect("decode"),
            time
        );
    }

    #[test]
    fn the_rtps_duration_shares_the_timestamp_layout() {
        assert_eq!(Duration::from_millis(500).fraction(), 1 << 31);
        assert_eq!(Duration::from_millis(500).subsec_nanos(), 500_000_000);
        assert_eq!(Duration::from_millis(1_500).seconds, 1);
        assert_eq!(Duration::from_secs(7), Duration::new(7, 0));
        assert_eq!(Duration::from_secs_nanos(2, 250_000_000).fraction, 1 << 30);
        assert!(Duration::ZERO.is_zero());
        assert!(Duration::INFINITE.is_infinite());
        assert_eq!(Duration::default(), Duration::ZERO);
        assert_eq!(Duration::DEFAULT_PARTICIPANT_LEASE.seconds, 100);
        assert_eq!(Duration::INFINITE.to_string(), "DURATION_INFINITE");
        assert_eq!(Duration::from_secs(3).to_string(), "3.000000000s");
    }

    #[test]
    fn rtps_durations_convert_to_and_from_std() {
        let std_duration = StdDuration::new(4, 250_000_000);
        let rtps = Duration::from_std(std_duration);
        assert_eq!(rtps.seconds, 4);
        assert_eq!(rtps.fraction, 1 << 30);
        assert_eq!(rtps.to_std(), Some(std_duration));
        assert_eq!(Duration::from(std_duration), rtps);
        assert_eq!(Duration::INFINITE.to_std(), None);
        assert_eq!(Duration::new(-1, 0).to_std(), None);
        assert_eq!(
            Duration::from_std(StdDuration::from_secs(u64::from(u32::MAX))).seconds,
            i32::MAX
        );
    }

    #[test]
    fn the_dds_duration_counts_nanoseconds_not_a_fraction() {
        let dds = DdsDuration::from_millis(500);
        assert_eq!(dds.sec, 0);
        assert_eq!(dds.nanosec, 500_000_000);
        assert_eq!(DdsDuration::from_millis(2_500).sec, 2);
        assert_eq!(DdsDuration::from_secs(9), DdsDuration::new(9, 0));
        assert!(DdsDuration::ZERO.is_zero());
        assert!(DdsDuration::INFINITE.is_infinite());
        assert_eq!(DdsDuration::default(), DdsDuration::ZERO);
        assert_eq!(DdsDuration::INFINITE.to_string(), "DURATION_INFINITE");
        assert_eq!(DdsDuration::new(1, 5).to_string(), "1.000000005s");
        assert_eq!(
            DdsDuration::from_std(StdDuration::new(1, 5)),
            DdsDuration::new(1, 5)
        );
        assert_eq!(
            DdsDuration::from_std(StdDuration::from_secs(u64::from(u32::MAX))).sec,
            i32::MAX
        );
        assert_eq!(
            DdsDuration::new(3, 0).to_std(),
            Some(StdDuration::from_secs(3))
        );
        assert_eq!(DdsDuration::INFINITE.to_std(), None);
        assert_eq!(DdsDuration::new(-1, 0).to_std(), None);
    }

    #[test]
    fn the_two_duration_types_are_the_same_octets_read_differently() {
        // The trap this module exists for: 500 ms is 0x8000_0000 in the RTPS
        // fraction field and 0x1dcd_6500 in the DDS nanosecond field. Reading
        // one as the other is off by a factor of ~4.29, not an error.
        let rtps = to_vec_headerless(&Duration::from_millis(500), Encoding::ROS2).expect("encode");
        let dds =
            to_vec_headerless(&DdsDuration::from_millis(500), Encoding::ROS2).expect("encode");
        assert_eq!(rtps, [0, 0, 0, 0, 0x00, 0x00, 0x00, 0x80]);
        assert_eq!(dds, [0, 0, 0, 0, 0x00, 0x65, 0xcd, 0x1d]);
        assert_ne!(rtps, dds);

        // …and the explicit bridge converts rather than reinterprets.
        assert_eq!(
            DdsDuration::from_millis(500).to_rtps(),
            Duration::from_millis(500)
        );
        assert_eq!(
            DdsDuration::from_rtps(Duration::from_millis(500)),
            DdsDuration::from_millis(500)
        );
        assert_eq!(DdsDuration::INFINITE.to_rtps(), Duration::INFINITE);
        assert_eq!(
            DdsDuration::from_rtps(Duration::INFINITE),
            DdsDuration::INFINITE
        );
    }

    #[test]
    fn durations_round_trip_through_cdr() {
        for encoding in [
            Encoding::ROS2,
            Encoding::new(astrs_cdr::EncapsulationKind::CdrBe),
        ] {
            let rtps = Duration::new(-3, 0x1234_5678);
            let bytes = to_vec_headerless(&rtps, encoding).expect("encode");
            assert_eq!(
                from_bytes_headerless::<Duration>(&bytes, encoding).expect("decode"),
                rtps
            );

            let dds = DdsDuration::new(-3, 999_999_999);
            let bytes = to_vec_headerless(&dds, encoding).expect("encode");
            assert_eq!(
                from_bytes_headerless::<DdsDuration>(&bytes, encoding).expect("decode"),
                dds
            );
        }
    }
}
