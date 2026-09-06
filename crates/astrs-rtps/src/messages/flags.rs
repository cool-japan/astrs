//! The flags octet, and the parts of a submessage this build does not
//! interpret.
//!
//! Every submessage header carries one octet of flags (OMG DDSI-RTPS 2.3
//! §8.3.3.2.2). Bit 0 is the same in every kind — the `EndiannessFlag`,
//! which selects the byte order of the submessage *body* and nothing else —
//! and the remaining seven mean whatever the kind says they mean.
//!
//! ```text
//!  bit  7   6   5   4   3   2   1   0
//!     +---+---+---+---+---+---+---+---+
//!     |            kind-specific  | E |
//!     +---+---+---+---+---+---+---+---+
//! ```
//!
//! # Two forward-compatibility rules, and how this crate keeps them
//!
//! §8.6 lets a later minor version extend the protocol in two ways, and a 2.3
//! receiver must tolerate both:
//!
//! 1. **New flag bits.** A reader ignores the bits it does not know. Ignoring
//!    them is not the same as *dropping* them, though: AstRS keeps them in
//!    [`Extension::reserved_flags`] so a submessage decoded from a peer and
//!    re-emitted carries the same octet it arrived with.
//! 2. **New fields at the end of a submessage body.** A reader skips from the
//!    last field it knows to `octetsToNextHeader`. AstRS keeps those octets
//!    in [`Extension::trailing`], for the same reason.
//!
//! Together they make [`Extension`] the answer to "what did this peer say
//! that we did not understand?", and they are why decoding a submessage from
//! a future version and re-encoding it reproduces the original octets.
//!
//! The RTPS 2.3 group-info flags on `HEARTBEAT` and `GAP` land here: this
//! repository has no specification-independent source for the layout of the
//! fields they introduce, so rather than guess at it AstRS carries the octets
//! verbatim. Nothing is lost — a receiver that ignores group ordering behaves
//! exactly as §8.6 requires — and nothing is invented.
//!
//! ```
//! use astrs_rtps::messages::{SubmessageFlags, flags};
//!
//! let data_flags = SubmessageFlags::new(0x07); // E | Q | D
//! assert!(data_flags.is_little_endian());
//! assert!(data_flags.has(flags::INLINE_QOS));
//! assert!(data_flags.has(flags::DATA));
//! assert!(!data_flags.has(flags::KEY));
//! ```

use core::fmt;

use astrs_cdr::{EncapsulationKind, Encoding, Endianness};

// The flag bit positions follow, by the name the specification gives each
// one. The values repeat across kinds — `0x02` is `Q` in a `DATA`, `F` in a
// `HEARTBEAT`, `I` in an `INFO_TS` and `M` in an `INFO_REPLY` — so a constant
// is only meaningful together with the submessage it belongs to, and each one
// names the kinds it applies to.

/// `E`, bit 0 — set when the body is little-endian. Every submessage.
pub const ENDIANNESS: u8 = 0x01;

/// `Q`, bit 1 — an `inlineQos` parameter list follows the fixed fields.
/// `DATA`, `DATA_FRAG`.
pub const INLINE_QOS: u8 = 0x02;

/// `D`, bit 2 — the `serializedPayload` holds the sample. `DATA` only;
/// a `DATA_FRAG` always carries payload, so it has no `D`.
pub const DATA: u8 = 0x04;

/// `K`, bit 3 — the `serializedPayload` holds the key rather than the
/// sample. `DATA` only; see [`KEY_FRAG`] for the `DATA_FRAG` position.
pub const KEY: u8 = 0x08;

/// `N`, bit 4 — the payload is not CDR. `DATA` only; see
/// [`NON_STANDARD_PAYLOAD_FRAG`] for the `DATA_FRAG` position.
pub const NON_STANDARD_PAYLOAD: u8 = 0x10;

/// `K`, bit **2** — the key flag of a `DATA_FRAG`.
///
/// `DATA_FRAG` has no `D` flag, so its `K` and `N` sit one bit lower than
/// a `DATA`'s. Confusing the two is the classic RTPS decoder bug: the
/// octet `0x04` means "payload is the sample" in a `DATA` and "payload is
/// the key" in a `DATA_FRAG`.
pub const KEY_FRAG: u8 = 0x04;

/// `N`, bit **3** — the non-standard-payload flag of a `DATA_FRAG`.
pub const NON_STANDARD_PAYLOAD_FRAG: u8 = 0x08;

/// `F`, bit 1 — "no response required". `ACKNACK`, `HEARTBEAT`.
///
/// In a `HEARTBEAT` it means the writer is not asking for an `ACKNACK`;
/// in an `ACKNACK` it means the reader is not asking for a repair.
pub const FINAL: u8 = 0x02;

/// `L`, bit 2 — the heartbeat is a liveliness assertion rather than a
/// history announcement. `HEARTBEAT`.
pub const LIVELINESS: u8 = 0x04;

/// `I`, bit 1 — the timestamp is *absent* and the receiver should forget
/// the one it had. `INFO_TS`.
pub const INVALIDATE: u8 = 0x02;

/// `M`, bit 1 — a multicast locator list follows the unicast one.
/// `INFO_REPLY`, `INFO_REPLY_IP4`.
pub const MULTICAST: u8 = 0x02;

/// The eight-bit flags field of a submessage header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SubmessageFlags(u8);

impl SubmessageFlags {
    /// No flags at all: a big-endian body with nothing optional.
    pub const NONE: Self = Self(0);

    /// Only the endianness flag: a little-endian body with nothing optional.
    pub const LITTLE_ENDIAN: Self = Self(ENDIANNESS);

    /// Wrap a raw octet.
    #[must_use]
    pub const fn new(octet: u8) -> Self {
        Self(octet)
    }

    /// The flags a body of the given byte order starts from.
    #[must_use]
    pub const fn from_endianness(endianness: Endianness) -> Self {
        match endianness {
            Endianness::Little => Self::LITTLE_ENDIAN,
            Endianness::Big => Self::NONE,
        }
    }

    /// The raw octet.
    #[must_use]
    pub const fn raw(self) -> u8 {
        self.0
    }

    /// True when every bit of `mask` is set.
    #[must_use]
    pub const fn has(self, mask: u8) -> bool {
        self.0 & mask == mask
    }

    /// The same flags with `mask` set.
    #[must_use]
    pub const fn with(self, mask: u8) -> Self {
        Self(self.0 | mask)
    }

    /// The same flags with `mask` set when `condition`, cleared otherwise.
    #[must_use]
    pub const fn set(self, mask: u8, condition: bool) -> Self {
        if condition {
            Self(self.0 | mask)
        } else {
            Self(self.0 & !mask)
        }
    }

    /// The byte order bit 0 selects.
    #[must_use]
    pub const fn endianness(self) -> Endianness {
        if self.0 & ENDIANNESS == 0 {
            Endianness::Big
        } else {
            Endianness::Little
        }
    }

    /// True when the body is little-endian.
    #[must_use]
    pub const fn is_little_endian(self) -> bool {
        self.0 & ENDIANNESS != 0
    }

    /// The bits outside `defined`, which for this submessage kind are the
    /// ones §8.6 says a receiver must ignore.
    #[must_use]
    pub const fn reserved(self, defined: u8) -> u8 {
        self.0 & !defined
    }
}

impl fmt::Display for SubmessageFlags {
    /// `0x07`, the form a packet capture shows.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:02x}", self.0)
    }
}

impl From<u8> for SubmessageFlags {
    fn from(octet: u8) -> Self {
        Self(octet)
    }
}

impl From<SubmessageFlags> for u8 {
    fn from(flags: SubmessageFlags) -> Self {
        flags.0
    }
}

impl From<Endianness> for SubmessageFlags {
    fn from(endianness: Endianness) -> Self {
        Self::from_endianness(endianness)
    }
}

/// The `astrs-cdr` encoding a submessage body of the given byte order is read
/// and written with.
///
/// A submessage body is plain CDR with no encapsulation header, so the
/// encoding names only a byte order and the alignment origin is the first
/// octet after the four-octet submessage header.
///
/// # Examples
///
/// ```
/// use astrs_cdr::{EncapsulationKind, Endianness};
/// use astrs_rtps::messages::body_encoding;
///
/// assert_eq!(
///     body_encoding(Endianness::Little).kind(),
///     EncapsulationKind::CdrLe,
/// );
/// ```
#[must_use]
pub const fn body_encoding(endianness: Endianness) -> Encoding {
    match endianness {
        Endianness::Little => Encoding::new(EncapsulationKind::CdrLe),
        Endianness::Big => Encoding::new(EncapsulationKind::CdrBe),
    }
}

/// What a peer said that this build does not interpret.
///
/// See the [module documentation](self) for the two §8.6 extension
/// mechanisms this type preserves. A submessage AstRS constructs itself
/// always carries [`Extension::EMPTY`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Extension {
    /// Flag bits the submessage's kind does not define.
    ///
    /// Never inspected; re-emitted verbatim so a forwarded submessage keeps
    /// the octet its sender chose.
    pub reserved_flags: u8,
    /// Octets between the last field this build knows and the end of the
    /// submessage body.
    ///
    /// Empty for every submessage AstRS builds, and for every one a 2.3 peer
    /// sends that uses no extension. Non-empty when a peer used the §8.6
    /// tail-extension mechanism — which is where the RTPS 2.3 group-info
    /// fields of a `HEARTBEAT` or `GAP` arrive.
    pub trailing: Vec<u8>,
}

impl Extension {
    /// Nothing unknown: what a locally constructed submessage carries.
    pub const EMPTY: Self = Self {
        reserved_flags: 0,
        trailing: Vec::new(),
    };

    /// An extension holding only unknown flag bits.
    #[must_use]
    pub const fn from_reserved_flags(reserved_flags: u8) -> Self {
        Self {
            reserved_flags,
            trailing: Vec::new(),
        }
    }

    /// True when the peer used neither extension mechanism.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reserved_flags == 0 && self.trailing.is_empty()
    }

    /// Octets the trailing extension adds to the submessage body.
    #[must_use]
    pub fn trailing_len(&self) -> usize {
        self.trailing.len()
    }
}

impl fmt::Display for Extension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("none");
        }
        write!(
            f,
            "flags 0x{:02x}, {} trailing octet(s)",
            self.reserved_flags,
            self.trailing.len()
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn bit_zero_selects_the_body_byte_order() {
        assert_eq!(SubmessageFlags::NONE.endianness(), Endianness::Big);
        assert_eq!(
            SubmessageFlags::LITTLE_ENDIAN.endianness(),
            Endianness::Little
        );
        assert!(!SubmessageFlags::NONE.is_little_endian());
        assert!(SubmessageFlags::LITTLE_ENDIAN.is_little_endian());
        assert_eq!(
            SubmessageFlags::from_endianness(Endianness::Little),
            SubmessageFlags::LITTLE_ENDIAN
        );
        assert_eq!(
            SubmessageFlags::from(Endianness::Big),
            SubmessageFlags::NONE
        );
        assert_eq!(SubmessageFlags::default(), SubmessageFlags::NONE);
        // Every other bit leaves the byte order alone.
        assert_eq!(SubmessageFlags::new(0xfe).endianness(), Endianness::Big);
    }

    #[test]
    fn the_data_and_data_frag_flag_positions_differ_by_one_bit() {
        // The classic decoder bug: DATA_FRAG has no D flag, so its K and N
        // are one position lower.
        assert_eq!(DATA, 0x04);
        assert_eq!(KEY, 0x08);
        assert_eq!(NON_STANDARD_PAYLOAD, 0x10);
        assert_eq!(KEY_FRAG, DATA);
        assert_eq!(NON_STANDARD_PAYLOAD_FRAG, KEY);
    }

    #[test]
    fn the_shared_bit_one_means_something_different_in_each_kind() {
        assert_eq!(INLINE_QOS, 0x02);
        assert_eq!(FINAL, 0x02);
        assert_eq!(INVALIDATE, 0x02);
        assert_eq!(MULTICAST, 0x02);
        assert_eq!(LIVELINESS, 0x04);
        assert_eq!(ENDIANNESS, 0x01);
    }

    #[test]
    fn setting_and_testing_bits_composes() {
        let value = SubmessageFlags::NONE
            .with(ENDIANNESS)
            .with(INLINE_QOS)
            .set(DATA, true)
            .set(KEY, false);
        assert_eq!(value.raw(), 0x07);
        assert!(value.has(ENDIANNESS));
        assert!(value.has(INLINE_QOS));
        assert!(value.has(DATA));
        assert!(!value.has(KEY));
        assert!(value.has(ENDIANNESS | DATA));
        assert!(!value.has(ENDIANNESS | KEY));
        assert_eq!(value.set(DATA, false).raw(), 0x03);
        assert_eq!(u8::from(SubmessageFlags::from(0x1f_u8)), 0x1f);
        assert_eq!(value.to_string(), "0x07");
    }

    #[test]
    fn reserved_bits_are_whatever_the_kind_does_not_define() {
        let defined = ENDIANNESS | FINAL;
        assert_eq!(SubmessageFlags::new(0xff).reserved(defined), 0xfc);
        assert_eq!(SubmessageFlags::new(0x03).reserved(defined), 0x00);
    }

    #[test]
    fn the_body_encoding_follows_the_endianness_flag() {
        assert_eq!(
            body_encoding(Endianness::Little).kind(),
            EncapsulationKind::CdrLe
        );
        assert_eq!(
            body_encoding(Endianness::Big).kind(),
            EncapsulationKind::CdrBe
        );
        assert_eq!(
            body_encoding(SubmessageFlags::LITTLE_ENDIAN.endianness()).endianness(),
            Endianness::Little
        );
    }

    #[test]
    fn an_empty_extension_is_the_normal_case() {
        let empty = Extension::EMPTY;
        assert!(empty.is_empty());
        assert_eq!(empty.trailing_len(), 0);
        assert_eq!(empty.to_string(), "none");
        assert_eq!(Extension::default(), Extension::EMPTY);

        let flagged = Extension::from_reserved_flags(0x40);
        assert!(!flagged.is_empty());
        assert_eq!(flagged.to_string(), "flags 0x40, 0 trailing octet(s)");

        let tail = Extension {
            reserved_flags: 0x08,
            trailing: Vec::from([1_u8, 2, 3, 4]),
        };
        assert!(!tail.is_empty());
        assert_eq!(tail.trailing_len(), 4);
        assert_eq!(tail.to_string(), "flags 0x08, 4 trailing octet(s)");
    }
}
