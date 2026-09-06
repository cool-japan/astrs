//! [`FeatureFlags`]: the optional capabilities a connection may use.
//!
//! Blueprint §7.2 puts a feature bitmap in `Hello` for the things that are not
//! worth a protocol version bump: an optional compressor, a zero-copy plane, a
//! side channel that a minimal peer may not implement. Negotiation is a single
//! bitwise `AND` — a feature is live on a connection only if **both** ends
//! offered it.
//!
//! # Unknown bits are kept, not rejected
//!
//! [`crate::FrameFlags`] refuses a frame that sets a reserved bit, because a
//! frame's flags change how its bytes are read and guessing is unsafe. Feature
//! bits are the opposite: a newer peer advertising a capability this build has
//! never heard of is *normal*, and the intersection removes it anyway. So
//! [`FeatureFlags`] preserves unknown bits through a round trip (a proxy must
//! be able to relay a `Hello` unchanged) and only [`FeatureFlags::known`]
//! filters them out.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::FeatureFlags;
//!
//! let daemon = FeatureFlags::SHM_ZERO_COPY | FeatureFlags::COMPRESSION_ZSTD;
//! let node = FeatureFlags::SHM_ZERO_COPY;
//!
//! let agreed = daemon.negotiate(node);
//! assert!(agreed.contains(FeatureFlags::SHM_ZERO_COPY));
//! assert!(!agreed.contains(FeatureFlags::COMPRESSION_ZSTD));
//! ```

use core::fmt;
use core::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, Not, Sub, SubAssign};

use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::frame::Compression;

/// The optional capabilities of one endpoint, as a 64-bit map.
///
/// # Examples
///
/// ```
/// use astrs_wire::FeatureFlags;
///
/// let flags = FeatureFlags::EMPTY
///     .with(FeatureFlags::TOPIC_TAP, true)
///     .with(FeatureFlags::STATE_CATCH_UP, true);
/// assert_eq!(flags.count(), 2);
/// assert_eq!(flags.names(), vec!["topic_tap", "state_catch_up"]);
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
pub struct FeatureFlags(u64);

/// One named feature bit: its mask and its stable wire name.
type FeatureEntry = (FeatureFlags, &'static str);

impl FeatureFlags {
    /// No optional features.
    pub const EMPTY: Self = Self(0);

    /// Route payloads may be lz4-compressed (§6.4).
    pub const COMPRESSION_LZ4: Self = Self(1 << 0);

    /// Route payloads may be zstd-compressed (§6.4).
    pub const COMPRESSION_ZSTD: Self = Self(1 << 1);

    /// Same-host routes may be upgraded to the shared-memory plane (§6.2).
    pub const SHM_ZERO_COPY: Self = Self(1 << 2);

    /// Cross-host routes may use QUIC rather than TCP (§6.4).
    pub const QUIC: Self = Self(1 << 3);

    /// The endpoint can serve `astrs topic echo` taps (§7.3).
    pub const TOPIC_TAP: Self = Self(1 << 4);

    /// The endpoint implements the reconnect state catch-up log (§24.1
    /// `StateCatchUp`).
    pub const STATE_CATCH_UP: Self = Self(1 << 5);

    /// The endpoint implements the extension table (`ExtStore`/`ExtLoad`).
    pub const EXTENSIONS: Self = Self(1 << 6);

    /// The endpoint can hand out pinned (page-locked) buffers.
    pub const PINNED_MEMORY: Self = Self(1 << 7);

    /// The endpoint propagates tracing spans through metadata (§13).
    pub const TRACING: Self = Self(1 << 8);

    /// The endpoint can record and replay `.arec` streams (§14).
    pub const RECORDING: Self = Self(1 << 9);

    /// The endpoint accepts dynamic topology edits (`AddNode`, `AddEdge`).
    pub const DYNAMIC_TOPOLOGY: Self = Self(1 << 10);

    /// The endpoint speaks the ROS 2 bridge control messages (§10.5).
    pub const ROS2_BRIDGE: Self = Self(1 << 11);

    /// The endpoint serves the parameter store API (§17 `param`).
    pub const PARAM_STORE: Self = Self(1 << 12);

    /// The endpoint exports OTLP telemetry (§13).
    pub const OTLP_EXPORT: Self = Self(1 << 13);

    /// Every bit this build has a name for.
    pub const KNOWN: Self = Self(
        Self::COMPRESSION_LZ4.0
            | Self::COMPRESSION_ZSTD.0
            | Self::SHM_ZERO_COPY.0
            | Self::QUIC.0
            | Self::TOPIC_TAP.0
            | Self::STATE_CATCH_UP.0
            | Self::EXTENSIONS.0
            | Self::PINNED_MEMORY.0
            | Self::TRACING.0
            | Self::RECORDING.0
            | Self::DYNAMIC_TOPOLOGY.0
            | Self::PARAM_STORE.0
            | Self::ROS2_BRIDGE.0
            | Self::OTLP_EXPORT.0,
    );

    /// The named bits, in ascending bit order, with their wire names.
    pub const NAMED: &'static [FeatureEntry] = &[
        (Self::COMPRESSION_LZ4, "compression_lz4"),
        (Self::COMPRESSION_ZSTD, "compression_zstd"),
        (Self::SHM_ZERO_COPY, "shm_zero_copy"),
        (Self::QUIC, "quic"),
        (Self::TOPIC_TAP, "topic_tap"),
        (Self::STATE_CATCH_UP, "state_catch_up"),
        (Self::EXTENSIONS, "extensions"),
        (Self::PINNED_MEMORY, "pinned_memory"),
        (Self::TRACING, "tracing"),
        (Self::RECORDING, "recording"),
        (Self::DYNAMIC_TOPOLOGY, "dynamic_topology"),
        (Self::ROS2_BRIDGE, "ros2_bridge"),
        (Self::PARAM_STORE, "param_store"),
        (Self::OTLP_EXPORT, "otlp_export"),
    ];

    /// The set a full AstRS daemon offers.
    ///
    /// A convenience for the common case; endpoints with fewer capabilities
    /// (an embedded node, a minimal bridge) build their own set.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FeatureFlags;
    ///
    /// assert!(FeatureFlags::daemon_defaults().contains(FeatureFlags::SHM_ZERO_COPY));
    /// ```
    #[must_use]
    pub const fn daemon_defaults() -> Self {
        Self(
            Self::COMPRESSION_LZ4.0
                | Self::COMPRESSION_ZSTD.0
                | Self::SHM_ZERO_COPY.0
                | Self::QUIC.0
                | Self::TOPIC_TAP.0
                | Self::STATE_CATCH_UP.0
                | Self::EXTENSIONS.0
                | Self::TRACING.0
                | Self::PARAM_STORE.0,
        )
    }

    /// Wraps a raw bitmap, unknown bits and all.
    #[must_use]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// The raw bitmap.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Whether no feature at all is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether **every** bit of `other` is set here.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether the two sets share at least one bit.
    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// This set with `other` added.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The bits present in both sets — the negotiated feature set.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FeatureFlags;
    ///
    /// let mine = FeatureFlags::QUIC | FeatureFlags::TRACING;
    /// let theirs = FeatureFlags::QUIC;
    /// assert_eq!(mine.negotiate(theirs), FeatureFlags::QUIC);
    /// ```
    #[must_use]
    pub const fn negotiate(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// This set without the bits of `other`.
    #[must_use]
    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// This set with one flag turned on or off.
    #[must_use]
    pub const fn with(self, flag: Self, enabled: bool) -> Self {
        if enabled {
            Self(self.0 | flag.0)
        } else {
            Self(self.0 & !flag.0)
        }
    }

    /// This set restricted to the bits this build knows.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FeatureFlags;
    ///
    /// let future = FeatureFlags::from_bits(1 << 63) | FeatureFlags::QUIC;
    /// assert_eq!(future.known(), FeatureFlags::QUIC);
    /// assert!(future.has_unknown());
    /// ```
    #[must_use]
    pub const fn known(self) -> Self {
        Self(self.0 & Self::KNOWN.0)
    }

    /// The bits this build has no name for.
    #[must_use]
    pub const fn unknown(self) -> Self {
        Self(self.0 & !Self::KNOWN.0)
    }

    /// Whether the peer advertised a capability this build does not know.
    #[must_use]
    pub const fn has_unknown(self) -> bool {
        !self.unknown().is_empty()
    }

    /// How many bits are set.
    #[must_use]
    pub const fn count(self) -> u32 {
        self.0.count_ones()
    }

    /// The names of the known bits that are set, in ascending bit order.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FeatureFlags;
    ///
    /// let flags = FeatureFlags::QUIC | FeatureFlags::TRACING;
    /// assert_eq!(flags.names(), vec!["quic", "tracing"]);
    /// ```
    #[must_use]
    pub fn names(self) -> Vec<&'static str> {
        Self::NAMED
            .iter()
            .filter(|(flag, _)| self.contains(*flag))
            .map(|(_, name)| *name)
            .collect()
    }

    /// The feature bit a compression algorithm needs, if any.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Compression, FeatureFlags};
    ///
    /// assert_eq!(
    ///     FeatureFlags::for_compression(Compression::Zstd),
    ///     FeatureFlags::COMPRESSION_ZSTD
    /// );
    /// assert!(FeatureFlags::for_compression(Compression::None).is_empty());
    /// ```
    #[must_use]
    pub const fn for_compression(compression: Compression) -> Self {
        match compression {
            Compression::Lz4 => Self::COMPRESSION_LZ4,
            Compression::Zstd => Self::COMPRESSION_ZSTD,
            _ => Self::EMPTY,
        }
    }

    /// Whether this set permits the given compression on a route.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Compression, FeatureFlags};
    ///
    /// let agreed = FeatureFlags::COMPRESSION_LZ4;
    /// assert!(agreed.allows_compression(Compression::Lz4));
    /// assert!(agreed.allows_compression(Compression::None));
    /// assert!(!agreed.allows_compression(Compression::Zstd));
    /// ```
    #[must_use]
    pub const fn allows_compression(self, compression: Compression) -> bool {
        self.contains(Self::for_compression(compression))
    }

    /// The best compression both ends support, preferring zstd's ratio over
    /// lz4's speed for the bulk payloads route compression targets.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Compression, FeatureFlags};
    ///
    /// assert_eq!(
    ///     FeatureFlags::COMPRESSION_LZ4.best_compression(),
    ///     Compression::Lz4
    /// );
    /// assert_eq!(FeatureFlags::EMPTY.best_compression(), Compression::None);
    /// ```
    #[must_use]
    pub const fn best_compression(self) -> Compression {
        if self.contains(Self::COMPRESSION_ZSTD) {
            Compression::Zstd
        } else if self.contains(Self::COMPRESSION_LZ4) {
            Compression::Lz4
        } else {
            Compression::None
        }
    }
}

impl BitOr for FeatureFlags {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        self.union(other)
    }
}

impl BitOrAssign for FeatureFlags {
    fn bitor_assign(&mut self, other: Self) {
        *self = self.union(other);
    }
}

impl BitAnd for FeatureFlags {
    type Output = Self;

    fn bitand(self, other: Self) -> Self {
        self.negotiate(other)
    }
}

impl BitAndAssign for FeatureFlags {
    fn bitand_assign(&mut self, other: Self) {
        *self = self.negotiate(other);
    }
}

impl Sub for FeatureFlags {
    type Output = Self;

    fn sub(self, other: Self) -> Self {
        self.difference(other)
    }
}

impl SubAssign for FeatureFlags {
    fn sub_assign(&mut self, other: Self) {
        *self = self.difference(other);
    }
}

impl Not for FeatureFlags {
    type Output = Self;

    /// Complements within the **known** set, so `!EMPTY` is every named
    /// feature rather than a bitmap full of undefined bits.
    fn not(self) -> Self {
        Self::KNOWN.difference(self)
    }
}

impl fmt::Display for FeatureFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("none");
        }
        let mut first = true;
        for name in self.names() {
            if !first {
                f.write_str("|")?;
            }
            f.write_str(name)?;
            first = false;
        }
        let unknown = self.unknown();
        if !unknown.is_empty() {
            if !first {
                f.write_str("|")?;
            }
            write!(f, "unknown(0x{:x})", unknown.bits())?;
        }
        Ok(())
    }
}

impl From<u64> for FeatureFlags {
    fn from(bits: u64) -> Self {
        Self(bits)
    }
}

impl From<FeatureFlags> for u64 {
    fn from(flags: FeatureFlags) -> Self {
        flags.0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::round_trip;

    #[test]
    fn named_bits_are_distinct_and_ascending() {
        let mut seen = 0u64;
        let mut previous = 0u64;
        for (flag, name) in FeatureFlags::NAMED {
            assert_eq!(flag.count(), 1, "{name} is not a single bit");
            assert_eq!(seen & flag.bits(), 0, "{name} duplicates an earlier bit");
            assert!(flag.bits() > previous, "{name} is out of order");
            previous = flag.bits();
            seen |= flag.bits();
        }
        assert_eq!(FeatureFlags::KNOWN.bits(), seen);
        assert_eq!(
            FeatureFlags::NAMED.len(),
            usize::try_from(FeatureFlags::KNOWN.count()).unwrap()
        );
    }

    #[test]
    fn names_are_unique_snake_case() {
        let mut seen = std::collections::BTreeSet::new();
        for (_, name) in FeatureFlags::NAMED {
            assert!(seen.insert(*name), "duplicate feature name {name}");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{name} is not snake_case"
            );
        }
    }

    #[test]
    fn negotiation_is_intersection_and_is_commutative() {
        let left = FeatureFlags::daemon_defaults();
        let right = FeatureFlags::SHM_ZERO_COPY | FeatureFlags::RECORDING;
        assert_eq!(left.negotiate(right), right.negotiate(left));
        assert_eq!(left.negotiate(right), FeatureFlags::SHM_ZERO_COPY);
        assert_eq!(left & right, left.negotiate(right));
    }

    #[test]
    fn negotiation_never_invents_a_feature() {
        for (flag, name) in FeatureFlags::NAMED {
            let agreed = FeatureFlags::EMPTY.negotiate(*flag);
            assert!(agreed.is_empty(), "{name} appeared out of nowhere");
        }
    }

    #[test]
    fn unknown_bits_survive_a_round_trip_but_not_negotiation() {
        let future = FeatureFlags::from_bits(1 << 62) | FeatureFlags::QUIC;
        assert_eq!(round_trip(&future).unwrap(), future);
        assert!(future.has_unknown());
        assert_eq!(future.known(), FeatureFlags::QUIC);
        assert_eq!(
            future.negotiate(FeatureFlags::KNOWN),
            FeatureFlags::QUIC,
            "an unknown bit cannot survive an intersection with the known set"
        );
    }

    #[test]
    fn set_algebra_behaves() {
        let mut flags = FeatureFlags::EMPTY;
        flags |= FeatureFlags::QUIC;
        assert!(flags.contains(FeatureFlags::QUIC));
        flags -= FeatureFlags::QUIC;
        assert!(flags.is_empty());

        let both = FeatureFlags::QUIC | FeatureFlags::TRACING;
        assert!(both.intersects(FeatureFlags::TRACING));
        assert!(!both.contains(FeatureFlags::RECORDING));
        assert_eq!(both.difference(FeatureFlags::QUIC), FeatureFlags::TRACING);
        assert_eq!(both.with(FeatureFlags::QUIC, false), FeatureFlags::TRACING);
        assert_eq!(!FeatureFlags::EMPTY, FeatureFlags::KNOWN);
        assert_eq!(FeatureFlags::EMPTY.count(), 0);
    }

    #[test]
    fn compression_bits_map_to_the_frame_flag_vocabulary() {
        assert_eq!(
            FeatureFlags::for_compression(Compression::Lz4),
            FeatureFlags::COMPRESSION_LZ4
        );
        assert_eq!(
            FeatureFlags::for_compression(Compression::Zstd),
            FeatureFlags::COMPRESSION_ZSTD
        );
        assert!(FeatureFlags::for_compression(Compression::None).is_empty());

        let both = FeatureFlags::COMPRESSION_LZ4 | FeatureFlags::COMPRESSION_ZSTD;
        assert_eq!(both.best_compression(), Compression::Zstd);
        assert_eq!(
            FeatureFlags::COMPRESSION_LZ4.best_compression(),
            Compression::Lz4
        );
        assert_eq!(FeatureFlags::EMPTY.best_compression(), Compression::None);
        assert!(FeatureFlags::EMPTY.allows_compression(Compression::None));
    }

    #[test]
    fn display_lists_names_and_flags_unknown_bits() {
        assert_eq!(FeatureFlags::EMPTY.to_string(), "none");
        assert_eq!(
            (FeatureFlags::QUIC | FeatureFlags::TRACING).to_string(),
            "quic|tracing"
        );
        let future = FeatureFlags::QUIC | FeatureFlags::from_bits(1 << 40);
        assert!(future.to_string().contains("unknown(0x10000000000)"));
        assert_eq!(
            FeatureFlags::from_bits(1 << 40).to_string(),
            "unknown(0x10000000000)"
        );
    }

    #[test]
    fn the_wire_form_is_a_varint_and_serde_is_transparent() {
        assert_eq!(round_trip(&FeatureFlags::EMPTY).unwrap().bits(), 0);
        let json = serde_json::to_string(&FeatureFlags::QUIC).unwrap();
        assert_eq!(json, FeatureFlags::QUIC.bits().to_string());
        assert_eq!(
            serde_json::from_str::<FeatureFlags>(&json).unwrap(),
            FeatureFlags::QUIC
        );
    }

    #[test]
    fn conversions_are_lossless() {
        for bits in [0u64, 1, 0xFFFF, u64::MAX] {
            assert_eq!(u64::from(FeatureFlags::from(bits)), bits);
        }
    }

    #[test]
    fn daemon_defaults_are_a_subset_of_the_known_set() {
        assert!(FeatureFlags::KNOWN.contains(FeatureFlags::daemon_defaults()));
        assert!(!FeatureFlags::daemon_defaults().has_unknown());
    }
}
