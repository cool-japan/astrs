//! [`HlcTimestamp`] — the compact, totally-ordered hybrid logical clock
//! timestamp shared by every event in AstRS.

use std::fmt;
use std::num::ParseIntError;
use std::str::FromStr;
use std::time::Duration;

use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// A hybrid logical clock (HLC) timestamp: a physical-time component paired
/// with a logical counter that breaks ties between events that land on the
/// same physical nanosecond.
///
/// # Layout
///
/// The blueprint (§4.3) offers two viable layouts: a 48-bit-truncated
/// physical time packed with a 16-bit logical counter into a single `u64`
/// (enabling lock-free atomic updates, at the cost of wrapping around every
/// ~78 hours), or a full 64-bit physical nanosecond count with a wider
/// logical counter. AstRS chooses the latter:
///
/// - `physical_ns: u64` — nanoseconds since the UNIX epoch, full range (no
///   truncation, no wraparound before the year 2554).
/// - `logical: u32` — a tie-breaking counter, reset to zero whenever
///   physical time advances.
///
/// The 48-bit truncated form was rejected because `.arec` recordings and
/// replay sessions (§14) depend on a *globally* total order across an
/// entire recording session, which routinely exceeds the truncated form's
/// ~78-hour wraparound window; a wrapped comparison would silently corrupt
/// ordering for any two entries straddling a wrap. Full-width physical time
/// avoids that failure mode entirely, and clock issuance happens at
/// message-send granularity (not a per-byte hot path), so the lock-free
/// single-`u64`-CAS optimization the truncated form would enable is not
/// worth the correctness risk here (see [`crate::HlcClock`] for the
/// resulting `Mutex`-guarded design and its rationale).
///
/// Despite storing two fields, the type is **totally ordered exactly as a
/// packed `u128` would be**: comparing two `HlcTimestamp` values is
/// equivalent to comparing `(physical_ns as u128) << 32 | (logical as
/// u128)`, because `physical_ns` is declared first and therefore dominates
/// the derived lexicographic [`Ord`] implementation. [`HlcTimestamp::as_u128`]
/// and [`HlcTimestamp::from_u128`] make that packed view concrete for
/// callers that want a single sortable integer (e.g. as a `BTreeMap` key or
/// a compact hash).
///
/// # Examples
///
/// ```
/// use astrs_time::HlcTimestamp;
///
/// let a = HlcTimestamp::new(1_000, 0);
/// let b = HlcTimestamp::new(1_000, 1);
/// let c = HlcTimestamp::new(1_001, 0);
/// assert!(a < b);
/// assert!(b < c);
/// assert_eq!(a.as_u128().cmp(&b.as_u128()), a.cmp(&b));
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
pub struct HlcTimestamp {
    /// Nanoseconds since the UNIX epoch. Declared first so the derived
    /// [`Ord`] compares physical time before the logical counter.
    physical_ns: u64,
    /// Tie-breaking counter for events sharing the same `physical_ns`.
    logical: u32,
}

impl HlcTimestamp {
    /// The zero value: the UNIX epoch with a zero logical counter.
    ///
    /// This is a sentinel/placeholder, not a real event timestamp — every
    /// [`crate::HlcClock`] starts its internal state here, since any real
    /// wall-clock reading compares greater than it.
    pub const EPOCH: Self = Self {
        physical_ns: 0,
        logical: 0,
    };

    /// The largest representable timestamp: `u64::MAX` physical nanoseconds
    /// since the epoch (the year ~2554, ~584.5 years after 1970) with a
    /// `u32::MAX` logical counter.
    ///
    /// Nothing compares greater than this — see [`crate::HlcClock`]'s docs
    /// for how `now`/`update_with` behave once a clock's state actually
    /// reaches it (they pin here rather than wrapping back to a smaller
    /// value, which is the only sound behavior once the type's range is
    /// exhausted).
    pub const MAX: Self = Self {
        physical_ns: u64::MAX,
        logical: u32::MAX,
    };

    /// Builds a timestamp from its physical and logical components.
    ///
    /// This is a low-level constructor for tests, deserialization, and
    /// other value-level uses; production code that needs a *current*
    /// timestamp should go through [`crate::HlcClock::now`] instead, which
    /// maintains the monotonicity and drift-rejection invariants this bare
    /// constructor does not.
    #[must_use]
    pub const fn new(physical_ns: u64, logical: u32) -> Self {
        Self {
            physical_ns,
            logical,
        }
    }

    /// The physical-time component: nanoseconds since the UNIX epoch.
    #[must_use]
    pub const fn physical_ns(&self) -> u64 {
        self.physical_ns
    }

    /// The logical tie-breaking counter.
    #[must_use]
    pub const fn logical(&self) -> u32 {
        self.logical
    }

    /// Packs this timestamp into a single `u128`, with `physical_ns` in the
    /// high 64 bits and `logical` in the low 32 bits (bits 96..128 are
    /// always zero). Comparing two packed values with `u128::cmp` gives the
    /// same result as comparing the two `HlcTimestamp` values directly.
    #[must_use]
    pub const fn as_u128(self) -> u128 {
        ((self.physical_ns as u128) << 32) | (self.logical as u128)
    }

    /// Unpacks a `u128` produced by [`HlcTimestamp::as_u128`].
    ///
    /// Only the low 96 bits are meaningful; any bits at position 96 or
    /// above are silently discarded. Round-trips exactly for any value
    /// actually produced by `as_u128`.
    #[must_use]
    pub const fn from_u128(packed: u128) -> Self {
        Self {
            physical_ns: (packed >> 32) as u64,
            logical: (packed & 0xFFFF_FFFF) as u32,
        }
    }

    /// The physical-time elapsed between `earlier` and `self`, ignoring the
    /// logical counter entirely.
    ///
    /// Returns `None` if `earlier`'s physical time is not less than or
    /// equal to `self`'s — i.e. if `self` is not physically at-or-after
    /// `earlier` — rather than panicking or saturating, since that would
    /// silently hide a caller bug (comparing timestamps in the wrong
    /// order).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_time::HlcTimestamp;
    /// use std::time::Duration;
    ///
    /// let a = HlcTimestamp::new(1_000, 5);
    /// let b = HlcTimestamp::new(1_500, 0);
    /// assert_eq!(b.physical_duration_since(&a), Some(Duration::from_nanos(500)));
    /// assert_eq!(a.physical_duration_since(&b), None);
    /// ```
    #[must_use]
    pub fn physical_duration_since(&self, earlier: &Self) -> Option<Duration> {
        self.physical_ns
            .checked_sub(earlier.physical_ns)
            .map(Duration::from_nanos)
    }
}

impl fmt::Display for HlcTimestamp {
    /// Formats as `"<physical_ns>-<logical>"`, e.g. `"1771286400123456789-7"`.
    ///
    /// `physical_ns` is an unsigned decimal integer and therefore never
    /// contains a `-`, so the separator is unambiguous and
    /// [`HlcTimestamp::from_str`] always round-trips this format exactly.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.physical_ns, self.logical)
    }
}

/// Error returned when parsing an [`HlcTimestamp`] from its canonical
/// `"<physical_ns>-<logical>"` string form fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HlcTimestampParseError {
    /// The input did not contain the `-` separator between the physical
    /// and logical components.
    #[error("missing '-' separator between physical and logical components in {0:?}")]
    MissingSeparator(String),
    /// The physical-time component was not a valid `u64`.
    #[error("invalid physical-time component {0:?}: {1}")]
    InvalidPhysical(String, #[source] ParseIntError),
    /// The logical-counter component was not a valid `u32`.
    #[error("invalid logical-counter component {0:?}: {1}")]
    InvalidLogical(String, #[source] ParseIntError),
}

impl FromStr for HlcTimestamp {
    type Err = HlcTimestampParseError;

    /// Parses the canonical `"<physical_ns>-<logical>"` form produced by
    /// [`HlcTimestamp`]'s `Display` implementation.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_time::HlcTimestamp;
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let ts = HlcTimestamp::new(1_771_286_400_123_456_789, 7);
    /// let text = ts.to_string();
    /// assert_eq!(text, "1771286400123456789-7");
    /// let parsed: HlcTimestamp = text.parse()?;
    /// assert_eq!(parsed, ts);
    /// # Ok(())
    /// # }
    /// ```
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (phys_str, logical_str) = s
            .split_once('-')
            .ok_or_else(|| HlcTimestampParseError::MissingSeparator(s.to_owned()))?;
        let physical_ns = phys_str
            .parse::<u64>()
            .map_err(|e| HlcTimestampParseError::InvalidPhysical(phys_str.to_owned(), e))?;
        let logical = logical_str
            .parse::<u32>()
            .map_err(|e| HlcTimestampParseError::InvalidLogical(logical_str.to_owned(), e))?;
        Ok(Self::new(physical_ns, logical))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn ordering_is_physical_then_logical() {
        let a = HlcTimestamp::new(10, 5);
        let b = HlcTimestamp::new(10, 6);
        let c = HlcTimestamp::new(11, 0);
        assert!(a < b);
        assert!(b < c);
        assert!(a < c);
    }

    #[test]
    fn ordering_matches_u128_packing() {
        let a = HlcTimestamp::new(10, 5);
        let b = HlcTimestamp::new(9, u32::MAX);
        assert!(a > b);
        assert!(a.as_u128() > b.as_u128());
    }

    #[test]
    fn as_u128_round_trip() {
        let ts = HlcTimestamp::new(u64::MAX, u32::MAX);
        assert_eq!(HlcTimestamp::from_u128(ts.as_u128()), ts);
        let zero = HlcTimestamp::EPOCH;
        assert_eq!(HlcTimestamp::from_u128(zero.as_u128()), zero);
    }

    #[test]
    fn from_u128_discards_high_bits() {
        let packed = (1u128 << 100) | HlcTimestamp::new(42, 7).as_u128();
        assert_eq!(HlcTimestamp::from_u128(packed), HlcTimestamp::new(42, 7));
    }

    #[test]
    fn default_is_epoch() {
        assert_eq!(HlcTimestamp::default(), HlcTimestamp::EPOCH);
        assert_eq!(HlcTimestamp::EPOCH.physical_ns(), 0);
        assert_eq!(HlcTimestamp::EPOCH.logical(), 0);
    }

    #[test]
    fn max_is_the_largest_representable_value() {
        assert_eq!(HlcTimestamp::MAX, HlcTimestamp::new(u64::MAX, u32::MAX));
        assert!(HlcTimestamp::MAX > HlcTimestamp::new(u64::MAX, u32::MAX - 1));
        assert!(HlcTimestamp::MAX > HlcTimestamp::new(u64::MAX - 1, u32::MAX));
        assert!(HlcTimestamp::MAX > HlcTimestamp::EPOCH);
    }

    #[test]
    fn display_format() {
        assert_eq!(HlcTimestamp::new(1_000, 7).to_string(), "1000-7");
        assert_eq!(HlcTimestamp::new(0, 0).to_string(), "0-0");
    }

    #[test]
    fn from_str_round_trip() {
        let ts = HlcTimestamp::new(1_771_286_400_123_456_789, 4_000_000_000);
        let parsed: HlcTimestamp = ts.to_string().parse().unwrap();
        assert_eq!(parsed, ts);
    }

    #[test]
    fn from_str_rejects_missing_separator() {
        let err = "12345".parse::<HlcTimestamp>().unwrap_err();
        assert!(matches!(err, HlcTimestampParseError::MissingSeparator(_)));
    }

    #[test]
    fn from_str_rejects_invalid_physical() {
        let err = "abc-1".parse::<HlcTimestamp>().unwrap_err();
        assert!(matches!(err, HlcTimestampParseError::InvalidPhysical(_, _)));
    }

    #[test]
    fn from_str_rejects_invalid_logical() {
        let err = "1-abc".parse::<HlcTimestamp>().unwrap_err();
        assert!(matches!(err, HlcTimestampParseError::InvalidLogical(_, _)));
    }

    #[test]
    fn from_str_rejects_logical_overflow() {
        // u32::MAX + 1 does not fit the logical component.
        let err = "1-4294967296".parse::<HlcTimestamp>().unwrap_err();
        assert!(matches!(err, HlcTimestampParseError::InvalidLogical(_, _)));
    }

    #[test]
    fn physical_duration_since_ordering() {
        let a = HlcTimestamp::new(1_000, 5);
        let b = HlcTimestamp::new(1_500, 0);
        assert_eq!(
            b.physical_duration_since(&a),
            Some(Duration::from_nanos(500))
        );
        assert_eq!(a.physical_duration_since(&b), None);
        assert_eq!(a.physical_duration_since(&a), Some(Duration::ZERO));
    }

    #[test]
    fn oxicode_round_trip() {
        let ts = HlcTimestamp::new(123_456_789, 42);
        let bytes = oxicode::encode_to_vec(&ts).unwrap();
        let (decoded, _len): (HlcTimestamp, usize) = oxicode::decode_from_slice(&bytes).unwrap();
        assert_eq!(decoded, ts);
    }

    #[test]
    fn serde_json_round_trip() {
        let ts = HlcTimestamp::new(123_456_789, 42);
        let json = serde_json::to_string(&ts).unwrap();
        let decoded: HlcTimestamp = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, ts);
    }

    #[test]
    fn is_totally_ordered_hashable_and_copy() {
        fn assert_bounds<T: Copy + Eq + Ord + std::hash::Hash>() {}
        assert_bounds::<HlcTimestamp>();
    }
}
