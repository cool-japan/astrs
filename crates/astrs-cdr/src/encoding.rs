//! Encapsulation headers, endianness, and the encoding knobs a stream
//! carries.
//!
//! Every DDS serialized payload starts with a four-octet **encapsulation
//! header** (OMG DDSI-RTPS 2.3 §10.2, OMG DDS-XTypes 1.3 §7.6.3.1.2):
//!
//! ```text
//! byte:   0        1        2        3
//!       +--------+--------+--------+--------+
//!       | representation  |  representation |
//!       |   identifier    |     options     |
//!       +--------+--------+--------+--------+
//! ```
//!
//! The identifier is **always big-endian** — it has to be, since it is what
//! tells the reader the endianness of everything after it. The options field
//! is likewise big-endian and carries, in its two least significant bits, the
//! number of padding octets appended to the end of the payload (XCDR2 only;
//! XCDR1 senders leave the whole field zero).
//!
//! # The alignment origin
//!
//! CDR alignment is measured from the first octet **after** the header, not
//! from the start of the buffer. That octet is the *alignment origin*, and
//! every `align()` in this crate is relative to it. Concretely: a `double`
//! written as the very first member lands at buffer offset 4, not 8, because
//! its stream position is 0. [`crate::CdrWriter`] and [`crate::CdrReader`]
//! track the origin explicitly so a nested scope (a ParameterList value, an
//! RTPS payload appended into a larger datagram buffer) can restate it.
//!
//! # Identifier table
//!
//! Reproduced from OMG DDS-XTypes 1.3 §7.6.3.1.2, Table 47:
//!
//! | Representation | Encoding | Endianness | Identifier |
//! |---|---|---|---|
//! | `PLAIN_CDR`   | XCDR1 | big    | `0x0000` |
//! | `PLAIN_CDR`   | XCDR1 | little | `0x0001` |
//! | `PL_CDR`      | XCDR1 | big    | `0x0002` |
//! | `PL_CDR`      | XCDR1 | little | `0x0003` |
//! | *(XML)*       | —     | —      | `0x0004` |
//! | `PLAIN_CDR2`  | XCDR2 | big    | `0x0006` |
//! | `PLAIN_CDR2`  | XCDR2 | little | `0x0007` |
//! | `DELIMIT_CDR` | XCDR2 | big    | `0x0008` |
//! | `DELIMIT_CDR` | XCDR2 | little | `0x0009` |
//! | `PL_CDR2`     | XCDR2 | big    | `0x000a` |
//! | `PL_CDR2`     | XCDR2 | little | `0x000b` |
//!
//! ROS 2 publishes user topics as `CDR_LE` (`0x0001`) on every distro to
//! date; RTPS discovery (SPDP/SEDP) uses `PL_CDR_LE` (`0x0003`). Those two
//! are the hot paths, and both are exercised by the golden vectors.

use crate::error::{CdrError, CdrResult};

/// Byte order of a CDR stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Endianness {
    /// Big-endian, the OMG "network" order.
    Big,
    /// Little-endian, what every ROS 2 host emits in practice.
    Little,
}

impl Endianness {
    /// The endianness of the machine this code is running on.
    #[must_use]
    pub const fn native() -> Self {
        if cfg!(target_endian = "big") {
            Self::Big
        } else {
            Self::Little
        }
    }

    /// True when this is [`Endianness::Little`].
    #[must_use]
    pub const fn is_little(self) -> bool {
        matches!(self, Self::Little)
    }
}

/// CDR encoding version.
///
/// The version fixes the maximum alignment: XCDR1 aligns 8-octet primitives
/// to 8, XCDR2 caps every alignment at 4 (OMG DDS-XTypes 1.3 §7.4.3.4.1).
/// That single difference is why an XCDR2 stream is never byte-identical to
/// the XCDR1 stream of the same value once a `double` follows an odd-sized
/// member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CdrVersion {
    /// Classic OMG CDR, as used by ROS 2 Humble/Iron/Jazzy for user topics
    /// and by RTPS discovery on every distro.
    Xcdr1,
    /// OMG DDS-XTypes CDR version 2: alignment capped at 4, DHEADER-delimited
    /// appendable types, EMHEADER-tagged mutable types.
    Xcdr2,
}

impl CdrVersion {
    /// The largest alignment this version ever applies.
    #[must_use]
    pub const fn max_alignment(self) -> usize {
        match self {
            Self::Xcdr1 => 8,
            Self::Xcdr2 => 4,
        }
    }

    /// True for [`CdrVersion::Xcdr2`].
    #[must_use]
    pub const fn is_v2(self) -> bool {
        matches!(self, Self::Xcdr2)
    }
}

/// How the body after the encapsulation header is laid out.
///
/// This classifies an *identifier*, which is a statement about the top-level
/// type's extensibility. It does not by itself drive framing: a nested
/// member's framing follows that member's own type, which is why
/// [`crate::CdrWriter::write_struct`] consults
/// [`crate::CdrType::EXTENSIBILITY`] rather than the enclosing stream's kind.
/// Use [`EncapsulationKind::declared_extensibility`] to go from an identifier
/// on the wire to the extensibility it announces, and
/// [`Encoding::for_extensibility`] to go the other way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Representation {
    /// Members follow each other directly, aligned to their natural
    /// boundaries. This is `PLAIN_CDR` (XCDR1) and `PLAIN_CDR2` (XCDR2), the
    /// wire form of a `@final` type.
    Plain,
    /// Members are individually tagged rather than positional — the wire form
    /// of a `@mutable` type.
    ///
    /// **The two CDR versions tag them differently, and this variant covers
    /// both.** Under XCDR1 (`PL_CDR_BE` / `PL_CDR_LE`) each member carries a
    /// 16-bit `(parameterId, parameterLength)` pair and the body ends with
    /// `PID_SENTINEL`: that is OMG DDSI-RTPS 2.3 §9.4.2.11, the form RTPS
    /// discovery uses, and the one [`crate::ParameterList`] implements.
    /// Under XCDR2 (`PL_CDR2_BE` / `PL_CDR2_LE`) there is no sentinel at all:
    /// a DHEADER bounds the member list and each member carries a 32-bit
    /// [`EmHeader`](crate::EmHeader) — see [`crate::xcdr2`].
    ///
    /// [`EncapsulationKind::is_rtps_parameter_list`] distinguishes them.
    ParameterList,
    /// XCDR2 only: the body is preceded by a DHEADER giving its byte length,
    /// so an older reader can skip members it does not know
    /// (`DELIMIT_CDR`, the wire form of an `@appendable` type).
    Delimited,
}

/// The `representation_identifier` of a CDR encapsulation header.
///
/// See the [module documentation](self) for the full identifier table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum EncapsulationKind {
    /// `CDR_BE` — XCDR1, big-endian, plain. Identifier `0x0000`.
    CdrBe,
    /// `CDR_LE` — XCDR1, little-endian, plain. Identifier `0x0001`.
    ///
    /// The ROS 2 default for user topic data.
    CdrLe,
    /// `PL_CDR_BE` — XCDR1, big-endian, parameter list. Identifier `0x0002`.
    PlCdrBe,
    /// `PL_CDR_LE` — XCDR1, little-endian, parameter list. Identifier
    /// `0x0003`.
    ///
    /// The representation of every SPDP and SEDP discovery sample.
    PlCdrLe,
    /// `CDR2_BE` (`PLAIN_CDR2`) — XCDR2, big-endian, plain. Identifier
    /// `0x0006`.
    Cdr2Be,
    /// `CDR2_LE` (`PLAIN_CDR2`) — XCDR2, little-endian, plain. Identifier
    /// `0x0007`.
    Cdr2Le,
    /// `D_CDR2_BE` (`DELIMIT_CDR`) — XCDR2, big-endian, delimited.
    /// Identifier `0x0008`.
    DCdr2Be,
    /// `D_CDR2_LE` (`DELIMIT_CDR`) — XCDR2, little-endian, delimited.
    /// Identifier `0x0009`.
    DCdr2Le,
    /// `PL_CDR2_BE` — XCDR2, big-endian, parameter list. Identifier
    /// `0x000a`.
    PlCdr2Be,
    /// `PL_CDR2_LE` — XCDR2, little-endian, parameter list. Identifier
    /// `0x000b`.
    PlCdr2Le,
}

impl EncapsulationKind {
    /// Every kind this crate understands, in identifier order.
    ///
    /// Useful for exhaustive tests and for `astrs-rtps` to advertise the set
    /// it accepts.
    pub const ALL: [Self; 10] = [
        Self::CdrBe,
        Self::CdrLe,
        Self::PlCdrBe,
        Self::PlCdrLe,
        Self::Cdr2Be,
        Self::Cdr2Le,
        Self::DCdr2Be,
        Self::DCdr2Le,
        Self::PlCdr2Be,
        Self::PlCdr2Le,
    ];

    /// The two-octet `representation_identifier`, in host order.
    #[must_use]
    pub const fn identifier(self) -> u16 {
        match self {
            Self::CdrBe => 0x0000,
            Self::CdrLe => 0x0001,
            Self::PlCdrBe => 0x0002,
            Self::PlCdrLe => 0x0003,
            Self::Cdr2Be => 0x0006,
            Self::Cdr2Le => 0x0007,
            Self::DCdr2Be => 0x0008,
            Self::DCdr2Le => 0x0009,
            Self::PlCdr2Be => 0x000a,
            Self::PlCdr2Le => 0x000b,
        }
    }

    /// Parse a `representation_identifier`.
    ///
    /// # Errors
    ///
    /// [`CdrError::UnknownEncapsulation`] for any value outside the table,
    /// including `0x0004` (XML), which is legal DDS but is not a CDR
    /// encoding and has no place in this crate.
    pub const fn from_identifier(identifier: u16) -> CdrResult<Self> {
        match identifier {
            0x0000 => Ok(Self::CdrBe),
            0x0001 => Ok(Self::CdrLe),
            0x0002 => Ok(Self::PlCdrBe),
            0x0003 => Ok(Self::PlCdrLe),
            0x0006 => Ok(Self::Cdr2Be),
            0x0007 => Ok(Self::Cdr2Le),
            0x0008 => Ok(Self::DCdr2Be),
            0x0009 => Ok(Self::DCdr2Le),
            0x000a => Ok(Self::PlCdr2Be),
            0x000b => Ok(Self::PlCdr2Le),
            other => Err(CdrError::UnknownEncapsulation { identifier: other }),
        }
    }

    /// Byte order of the stream this kind introduces.
    #[must_use]
    pub const fn endianness(self) -> Endianness {
        // Across the whole table the low bit of the identifier is the
        // endianness flag: even = big, odd = little.
        if self.identifier() & 1 == 0 {
            Endianness::Big
        } else {
            Endianness::Little
        }
    }

    /// CDR version of the stream this kind introduces.
    #[must_use]
    pub const fn version(self) -> CdrVersion {
        match self {
            Self::CdrBe | Self::CdrLe | Self::PlCdrBe | Self::PlCdrLe => CdrVersion::Xcdr1,
            Self::Cdr2Be
            | Self::Cdr2Le
            | Self::DCdr2Be
            | Self::DCdr2Le
            | Self::PlCdr2Be
            | Self::PlCdr2Le => CdrVersion::Xcdr2,
        }
    }

    /// Body layout of the stream this kind introduces.
    #[must_use]
    pub const fn representation(self) -> Representation {
        match self {
            Self::CdrBe | Self::CdrLe | Self::Cdr2Be | Self::Cdr2Le => Representation::Plain,
            Self::PlCdrBe | Self::PlCdrLe | Self::PlCdr2Be | Self::PlCdr2Le => {
                Representation::ParameterList
            }
            Self::DCdr2Be | Self::DCdr2Le => Representation::Delimited,
        }
    }

    /// True when the body tags its members individually — the wire form of a
    /// `@mutable` type, in either CDR version.
    ///
    /// Both versions are covered, and they are **not** the same format; see
    /// [`Representation::ParameterList`]. Use
    /// [`EncapsulationKind::is_rtps_parameter_list`] when what you need is
    /// specifically the sentinel-terminated RTPS form.
    #[must_use]
    pub const fn is_parameter_list(self) -> bool {
        matches!(self.representation(), Representation::ParameterList)
    }

    /// True only for `PL_CDR_BE` and `PL_CDR_LE`: the XCDR1,
    /// `PID_SENTINEL`-terminated `ParameterList` of OMG DDSI-RTPS 2.3
    /// §9.4.2.11, which is what SPDP and SEDP samples are written in and what
    /// [`crate::ParameterList`] parses.
    ///
    /// `PL_CDR2_BE` and `PL_CDR2_LE` are excluded: XCDR2's mutable form has
    /// no sentinel and tags members with an
    /// [`EmHeader`](crate::EmHeader) instead, so feeding one to a
    /// sentinel parser would mis-read it.
    #[must_use]
    pub const fn is_rtps_parameter_list(self) -> bool {
        matches!(self, Self::PlCdrBe | Self::PlCdrLe)
    }

    /// The extensibility a **top-level** value under this identifier is
    /// declared to have.
    ///
    /// The partial inverse of [`Encoding::for_extensibility`]. It is partial
    /// because XCDR1 has no delimited form: an `@appendable` type is written
    /// exactly like a `@final` one, so `CDR_LE` reports
    /// [`Extensibility::Final`](crate::Extensibility::Final) and the two are
    /// indistinguishable on the wire. Under XCDR2 the mapping is exact.
    ///
    /// ```
    /// use astrs_cdr::{EncapsulationKind, Extensibility};
    ///
    /// assert_eq!(
    ///     EncapsulationKind::DCdr2Le.declared_extensibility(),
    ///     Extensibility::Appendable,
    /// );
    /// assert_eq!(
    ///     EncapsulationKind::PlCdrLe.declared_extensibility(),
    ///     Extensibility::Mutable,
    /// );
    /// assert_eq!(
    ///     EncapsulationKind::CdrLe.declared_extensibility(),
    ///     Extensibility::Final,
    /// );
    /// ```
    #[must_use]
    pub const fn declared_extensibility(self) -> crate::xcdr2::Extensibility {
        use crate::xcdr2::Extensibility;

        match self.representation() {
            Representation::Plain => Extensibility::Final,
            Representation::Delimited => Extensibility::Appendable,
            Representation::ParameterList => Extensibility::Mutable,
        }
    }

    /// The same representation and version with the opposite byte order.
    ///
    /// Golden vectors use this to assert that a BE and an LE encoding of one
    /// value differ only in byte order.
    #[must_use]
    pub const fn swapped(self) -> Self {
        match self {
            Self::CdrBe => Self::CdrLe,
            Self::CdrLe => Self::CdrBe,
            Self::PlCdrBe => Self::PlCdrLe,
            Self::PlCdrLe => Self::PlCdrBe,
            Self::Cdr2Be => Self::Cdr2Le,
            Self::Cdr2Le => Self::Cdr2Be,
            Self::DCdr2Be => Self::DCdr2Le,
            Self::DCdr2Le => Self::DCdr2Be,
            Self::PlCdr2Be => Self::PlCdr2Le,
            Self::PlCdr2Le => Self::PlCdr2Be,
        }
    }

    /// This kind's plain (non-parameter-list, non-delimited) sibling.
    ///
    /// Serializing the *value* of a `PL_CDR` parameter uses the plain rules;
    /// only the enclosing list is parameterised.
    #[must_use]
    pub const fn plain(self) -> Self {
        match (self.version(), self.endianness()) {
            (CdrVersion::Xcdr1, Endianness::Big) => Self::CdrBe,
            (CdrVersion::Xcdr1, Endianness::Little) => Self::CdrLe,
            (CdrVersion::Xcdr2, Endianness::Big) => Self::Cdr2Be,
            (CdrVersion::Xcdr2, Endianness::Little) => Self::Cdr2Le,
        }
    }
}

/// The `representation_options` half of an encapsulation header.
///
/// XCDR2 senders record, in bits 1..0, how many octets of padding they
/// appended after the last member so the payload length is a multiple of
/// four (OMG DDS-XTypes 1.3 §7.6.3.1.2). A reader that ignores the field
/// sees those pad octets as trailing garbage — which is precisely what this
/// crate refuses — so [`crate::CdrReader`] subtracts them up front.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct EncapsulationOptions(u16);

impl EncapsulationOptions {
    /// All-zero options: no trailing padding declared. What XCDR1 senders
    /// (and ROS 2 on every distro to date) emit.
    pub const NONE: Self = Self(0);

    /// Mask of the bits that carry the trailing-padding count.
    pub const PADDING_MASK: u16 = 0x0003;

    /// Wrap a raw options word.
    #[must_use]
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    /// The raw options word.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Number of padding octets the sender appended after the last member.
    #[must_use]
    pub const fn padding(self) -> u8 {
        (self.0 & Self::PADDING_MASK) as u8
    }

    /// The same options with a new trailing-padding count.
    ///
    /// `padding` is taken modulo four; a caller can only ever need 0..=3
    /// octets to reach a four-octet boundary.
    #[must_use]
    pub const fn with_padding(self, padding: u8) -> Self {
        Self((self.0 & !Self::PADDING_MASK) | ((padding as u16) & Self::PADDING_MASK))
    }

    /// True when every bit outside the padding field is zero, which is what
    /// the specification reserves them to be.
    #[must_use]
    pub const fn reserved_bits_clear(self) -> bool {
        self.0 & !Self::PADDING_MASK == 0
    }
}

/// A parsed four-octet encapsulation header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EncapsulationHeader {
    /// The `representation_identifier`.
    pub kind: EncapsulationKind,
    /// The `representation_options`.
    pub options: EncapsulationOptions,
}

/// Length of the encapsulation header, in octets.
pub const ENCAPSULATION_HEADER_LEN: usize = 4;

impl EncapsulationHeader {
    /// A header with zero options.
    #[must_use]
    pub const fn new(kind: EncapsulationKind) -> Self {
        Self {
            kind,
            options: EncapsulationOptions::NONE,
        }
    }

    /// A header carrying an explicit options word.
    #[must_use]
    pub const fn with_options(kind: EncapsulationKind, options: EncapsulationOptions) -> Self {
        Self { kind, options }
    }

    /// Serialize the header.
    ///
    /// Both fields are big-endian regardless of the endianness the identifier
    /// selects for the body — the reader has to be able to read them before
    /// it knows the body's byte order.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; ENCAPSULATION_HEADER_LEN] {
        let id = self.kind.identifier().to_be_bytes();
        let opt = self.options.bits().to_be_bytes();
        [id[0], id[1], opt[0], opt[1]]
    }

    /// Parse a header from the front of `bytes`.
    ///
    /// # Errors
    ///
    /// - [`CdrError::Truncated`] when fewer than four octets are available.
    /// - [`CdrError::UnknownEncapsulation`] for an identifier outside the
    ///   XTypes table.
    pub fn from_bytes(bytes: &[u8]) -> CdrResult<Self> {
        let Some(head) = bytes.get(..ENCAPSULATION_HEADER_LEN) else {
            return Err(CdrError::Truncated {
                needed: ENCAPSULATION_HEADER_LEN,
                available: bytes.len(),
                context: "encapsulation header",
            });
        };
        let identifier = u16::from_be_bytes([head[0], head[1]]);
        let options = u16::from_be_bytes([head[2], head[3]]);
        Ok(Self {
            kind: EncapsulationKind::from_identifier(identifier)?,
            options: EncapsulationOptions::from_bits(options),
        })
    }
}

/// Width of a CDR `wchar` on the wire.
///
/// OMG DDS-XTypes 1.3 §7.4.3.4.1 gives `wchar` a size and alignment of two
/// octets: a UTF-16 code unit. That is the reading this crate implements and
/// the one its golden vectors encode.
///
/// [`WCharWidth::Four`] exists because some C++ DDS stacks historically
/// serialized `std::wstring` through a 32-bit `wchar_t`, producing four
/// octets per character. No specification text supports that layout, so it
/// carries **no golden vector** — it is a compatibility switch
/// `astrs-rtps` can flip per peer if the out-of-repo validation project ever
/// proves it necessary, and nothing more.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum WCharWidth {
    /// Two octets per `wchar`: a UTF-16 code unit. The specification
    /// behaviour, and the default.
    #[default]
    Two,
    /// Four octets per `wchar`, zero-extended from the UTF-16 code unit.
    Four,
}

impl WCharWidth {
    /// Octets one `wchar` occupies, which is also its alignment before the
    /// XCDR2 cap is applied.
    #[must_use]
    pub const fn octets(self) -> usize {
        match self {
            Self::Two => 2,
            Self::Four => 4,
        }
    }
}

/// The complete set of knobs a [`crate::CdrWriter`] or [`crate::CdrReader`]
/// carries.
///
/// An `Encoding` is `Copy` and cheap; pass it by value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Encoding {
    kind: EncapsulationKind,
    wchar_width: WCharWidth,
}

impl Encoding {
    /// The ROS 2 default for user topic data: `CDR_LE`, two-octet `wchar`.
    pub const ROS2: Self = Self::new(EncapsulationKind::CdrLe);

    /// The RTPS discovery default: `PL_CDR_LE`, two-octet `wchar`.
    pub const DISCOVERY: Self = Self::new(EncapsulationKind::PlCdrLe);

    /// An encoding for `kind` with specification-default `wchar` width.
    #[must_use]
    pub const fn new(kind: EncapsulationKind) -> Self {
        Self {
            kind,
            wchar_width: WCharWidth::Two,
        }
    }

    /// The same encoding with a different `wchar` width.
    #[must_use]
    pub const fn with_wchar_width(self, wchar_width: WCharWidth) -> Self {
        Self {
            wchar_width,
            ..self
        }
    }

    /// The same encoding with a different encapsulation kind.
    ///
    /// Used when a nested scope switches representation — a `PL_CDR`
    /// parameter's *value* is plain CDR of the same version and byte order.
    #[must_use]
    pub const fn with_kind(self, kind: EncapsulationKind) -> Self {
        Self { kind, ..self }
    }

    /// This encoding's plain sibling: same version, same byte order, no
    /// parameter list and no delimiter.
    #[must_use]
    pub const fn plain(self) -> Self {
        self.with_kind(self.kind.plain())
    }

    /// The encapsulation kind.
    #[must_use]
    pub const fn kind(self) -> EncapsulationKind {
        self.kind
    }

    /// The `wchar` width.
    #[must_use]
    pub const fn wchar_width(self) -> WCharWidth {
        self.wchar_width
    }

    /// Byte order of the stream.
    #[must_use]
    pub const fn endianness(self) -> Endianness {
        self.kind.endianness()
    }

    /// CDR version of the stream.
    #[must_use]
    pub const fn version(self) -> CdrVersion {
        self.kind.version()
    }

    /// Body layout of the stream.
    #[must_use]
    pub const fn representation(self) -> Representation {
        self.kind.representation()
    }

    /// The effective alignment for a member whose natural alignment is
    /// `natural`, after the XCDR2 cap.
    ///
    /// This is the single function that expresses "XCDR2 aligns everything to
    /// at most 4". Both the writer and the reader route every alignment
    /// through it, so the two can never disagree.
    #[must_use]
    pub const fn align_for(self, natural: usize) -> usize {
        let cap = self.version().max_alignment();
        if natural > cap { cap } else { natural }
    }

    /// True when the stream is XCDR2.
    #[must_use]
    pub const fn is_v2(self) -> bool {
        self.version().is_v2()
    }

    /// The encapsulation kind a **top-level** value of the given
    /// extensibility must be announced with, keeping this encoding's version
    /// and byte order.
    ///
    /// Under XCDR2 the identifier is part of the contract: a `@final` type is
    /// `PLAIN_CDR2`, an `@appendable` one `DELIMIT_CDR`, a `@mutable` one
    /// `PL_CDR2` (OMG DDS-XTypes 1.3 §7.6.3.1.2). Under XCDR1 there are only
    /// two: `PL_CDR` for a mutable type, `PLAIN_CDR` for everything else.
    ///
    /// This matters only at the top level. A nested member's framing follows
    /// *its own* type's extensibility, which is why
    /// [`crate::CdrWriter::write_struct`] reads that rather than the
    /// enclosing stream's kind.
    ///
    /// ```
    /// use astrs_cdr::{EncapsulationKind, Encoding, Extensibility};
    ///
    /// let v2 = Encoding::new(EncapsulationKind::Cdr2Le);
    /// assert_eq!(
    ///     v2.for_extensibility(Extensibility::Appendable).kind(),
    ///     EncapsulationKind::DCdr2Le,
    /// );
    /// assert_eq!(
    ///     v2.for_extensibility(Extensibility::Mutable).kind(),
    ///     EncapsulationKind::PlCdr2Le,
    /// );
    /// // XCDR1 has no delimited form, so an appendable type stays plain.
    /// let v1 = Encoding::ROS2;
    /// assert_eq!(
    ///     v1.for_extensibility(Extensibility::Appendable).kind(),
    ///     EncapsulationKind::CdrLe,
    /// );
    /// ```
    #[must_use]
    pub const fn for_extensibility(self, extensibility: crate::xcdr2::Extensibility) -> Self {
        use crate::xcdr2::Extensibility;

        let kind = match (self.version(), self.endianness(), extensibility) {
            (CdrVersion::Xcdr1, Endianness::Big, Extensibility::Mutable) => {
                EncapsulationKind::PlCdrBe
            }
            (CdrVersion::Xcdr1, Endianness::Little, Extensibility::Mutable) => {
                EncapsulationKind::PlCdrLe
            }
            (CdrVersion::Xcdr1, Endianness::Big, _) => EncapsulationKind::CdrBe,
            (CdrVersion::Xcdr1, Endianness::Little, _) => EncapsulationKind::CdrLe,
            (CdrVersion::Xcdr2, Endianness::Big, Extensibility::Final) => EncapsulationKind::Cdr2Be,
            (CdrVersion::Xcdr2, Endianness::Little, Extensibility::Final) => {
                EncapsulationKind::Cdr2Le
            }
            (CdrVersion::Xcdr2, Endianness::Big, Extensibility::Appendable) => {
                EncapsulationKind::DCdr2Be
            }
            (CdrVersion::Xcdr2, Endianness::Little, Extensibility::Appendable) => {
                EncapsulationKind::DCdr2Le
            }
            (CdrVersion::Xcdr2, Endianness::Big, Extensibility::Mutable) => {
                EncapsulationKind::PlCdr2Be
            }
            (CdrVersion::Xcdr2, Endianness::Little, Extensibility::Mutable) => {
                EncapsulationKind::PlCdr2Le
            }
        };
        self.with_kind(kind)
    }
}

impl Default for Encoding {
    /// [`Encoding::ROS2`] — `CDR_LE`, the representation every ROS 2 node
    /// publishes with.
    fn default() -> Self {
        Self::ROS2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_table_matches_xtypes_table_47() {
        // OMG DDS-XTypes 1.3 §7.6.3.1.2, Table 47, transcribed.
        let expected: [(EncapsulationKind, u16); 10] = [
            (EncapsulationKind::CdrBe, 0x0000),
            (EncapsulationKind::CdrLe, 0x0001),
            (EncapsulationKind::PlCdrBe, 0x0002),
            (EncapsulationKind::PlCdrLe, 0x0003),
            (EncapsulationKind::Cdr2Be, 0x0006),
            (EncapsulationKind::Cdr2Le, 0x0007),
            (EncapsulationKind::DCdr2Be, 0x0008),
            (EncapsulationKind::DCdr2Le, 0x0009),
            (EncapsulationKind::PlCdr2Be, 0x000a),
            (EncapsulationKind::PlCdr2Le, 0x000b),
        ];
        for (kind, id) in expected {
            assert_eq!(kind.identifier(), id, "{kind:?}");
            assert_eq!(EncapsulationKind::from_identifier(id), Ok(kind));
        }
        assert_eq!(EncapsulationKind::ALL, expected.map(|(kind, _)| kind));
    }

    #[test]
    fn xml_identifier_is_rejected() {
        // 0x0004 is XML in the XTypes table: legal DDS, but not a CDR stream.
        assert_eq!(
            EncapsulationKind::from_identifier(0x0004),
            Err(CdrError::UnknownEncapsulation { identifier: 0x0004 })
        );
        assert_eq!(
            EncapsulationKind::from_identifier(0x0005),
            Err(CdrError::UnknownEncapsulation { identifier: 0x0005 })
        );
        assert_eq!(
            EncapsulationKind::from_identifier(0xffff),
            Err(CdrError::UnknownEncapsulation { identifier: 0xffff })
        );
    }

    #[test]
    fn low_bit_of_the_identifier_is_the_endianness_flag() {
        for kind in EncapsulationKind::ALL {
            let expected = if kind.identifier() % 2 == 0 {
                Endianness::Big
            } else {
                Endianness::Little
            };
            assert_eq!(kind.endianness(), expected, "{kind:?}");
        }
    }

    #[test]
    fn version_and_representation_partition_the_table() {
        assert_eq!(EncapsulationKind::CdrLe.version(), CdrVersion::Xcdr1);
        assert_eq!(EncapsulationKind::PlCdrLe.version(), CdrVersion::Xcdr1);
        assert_eq!(EncapsulationKind::Cdr2Le.version(), CdrVersion::Xcdr2);
        assert_eq!(EncapsulationKind::PlCdr2Le.version(), CdrVersion::Xcdr2);

        assert_eq!(
            EncapsulationKind::CdrBe.representation(),
            Representation::Plain
        );
        assert_eq!(
            EncapsulationKind::PlCdrBe.representation(),
            Representation::ParameterList
        );
        assert_eq!(
            EncapsulationKind::DCdr2Be.representation(),
            Representation::Delimited
        );
        assert!(EncapsulationKind::PlCdr2Le.is_parameter_list());
        assert!(!EncapsulationKind::Cdr2Le.is_parameter_list());
    }

    #[test]
    fn swapped_is_an_involution_that_preserves_everything_but_byte_order() {
        for kind in EncapsulationKind::ALL {
            let other = kind.swapped();
            assert_eq!(other.swapped(), kind, "{kind:?}");
            assert_ne!(other.endianness(), kind.endianness(), "{kind:?}");
            assert_eq!(other.version(), kind.version(), "{kind:?}");
            assert_eq!(other.representation(), kind.representation(), "{kind:?}");
        }
    }

    #[test]
    fn plain_keeps_version_and_byte_order() {
        for kind in EncapsulationKind::ALL {
            let plain = kind.plain();
            assert_eq!(plain.representation(), Representation::Plain, "{kind:?}");
            assert_eq!(plain.version(), kind.version(), "{kind:?}");
            assert_eq!(plain.endianness(), kind.endianness(), "{kind:?}");
        }
    }

    #[test]
    fn header_bytes_are_big_endian_regardless_of_body_order() {
        // PL_CDR_LE selects a little-endian body, yet its own identifier is
        // still written big-endian: 0x0003 -> [0x00, 0x03].
        let header = EncapsulationHeader::new(EncapsulationKind::PlCdrLe);
        assert_eq!(header.to_bytes(), [0x00, 0x03, 0x00, 0x00]);
        assert_eq!(
            EncapsulationHeader::new(EncapsulationKind::CdrBe).to_bytes(),
            [0x00, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            EncapsulationHeader::new(EncapsulationKind::PlCdr2Le).to_bytes(),
            [0x00, 0x0b, 0x00, 0x00]
        );
    }

    #[test]
    fn header_round_trips_for_every_kind() {
        for kind in EncapsulationKind::ALL {
            let header =
                EncapsulationHeader::with_options(kind, EncapsulationOptions::NONE.with_padding(2));
            let bytes = header.to_bytes();
            assert_eq!(EncapsulationHeader::from_bytes(&bytes), Ok(header));
        }
    }

    #[test]
    fn header_rejects_a_short_buffer() {
        assert_eq!(
            EncapsulationHeader::from_bytes(&[0x00, 0x01, 0x00]),
            Err(CdrError::Truncated {
                needed: 4,
                available: 3,
                context: "encapsulation header",
            })
        );
    }

    #[test]
    fn options_padding_lives_in_the_two_low_bits() {
        let options = EncapsulationOptions::NONE.with_padding(3);
        assert_eq!(options.bits(), 0x0003);
        assert_eq!(options.padding(), 3);
        assert!(options.reserved_bits_clear());

        // Values above 3 wrap: only 0..=3 pad octets can ever be needed.
        assert_eq!(EncapsulationOptions::NONE.with_padding(7).padding(), 3);
        assert_eq!(EncapsulationOptions::NONE.with_padding(4).padding(), 0);

        // Bits outside the mask survive a padding update untouched.
        let reserved = EncapsulationOptions::from_bits(0x0100);
        assert!(!reserved.reserved_bits_clear());
        assert_eq!(reserved.with_padding(1).bits(), 0x0101);
    }

    #[test]
    fn alignment_cap_is_the_only_version_difference() {
        let v1 = Encoding::new(EncapsulationKind::CdrLe);
        let v2 = Encoding::new(EncapsulationKind::Cdr2Le);
        for natural in [1_usize, 2, 4, 8] {
            assert_eq!(v1.align_for(natural), natural);
            assert_eq!(v2.align_for(natural), natural.min(4));
        }
        assert_eq!(v1.align_for(8), 8);
        assert_eq!(v2.align_for(8), 4);
    }

    #[test]
    fn encoding_defaults_to_ros2_cdr_le() {
        let encoding = Encoding::default();
        assert_eq!(encoding, Encoding::ROS2);
        assert_eq!(encoding.kind(), EncapsulationKind::CdrLe);
        assert_eq!(encoding.wchar_width(), WCharWidth::Two);
        assert_eq!(Encoding::DISCOVERY.kind(), EncapsulationKind::PlCdrLe);
    }

    #[test]
    fn encoding_builders_change_one_field_at_a_time() {
        let base = Encoding::DISCOVERY;
        assert_eq!(base.plain().kind(), EncapsulationKind::CdrLe);
        assert_eq!(base.plain().endianness(), Endianness::Little);
        let wide = base.with_wchar_width(WCharWidth::Four);
        assert_eq!(wide.kind(), base.kind());
        assert_eq!(wide.wchar_width().octets(), 4);
        assert_eq!(WCharWidth::Two.octets(), 2);
    }

    #[test]
    fn extensibility_selects_the_top_level_identifier() {
        use crate::xcdr2::Extensibility;

        // XCDR2 has one identifier per extensibility, per byte order.
        let le = Encoding::new(EncapsulationKind::Cdr2Le);
        assert_eq!(
            le.for_extensibility(Extensibility::Final).kind(),
            EncapsulationKind::Cdr2Le
        );
        assert_eq!(
            le.for_extensibility(Extensibility::Appendable).kind(),
            EncapsulationKind::DCdr2Le
        );
        assert_eq!(
            le.for_extensibility(Extensibility::Mutable).kind(),
            EncapsulationKind::PlCdr2Le
        );

        let be = Encoding::new(EncapsulationKind::Cdr2Be);
        assert_eq!(
            be.for_extensibility(Extensibility::Final).kind(),
            EncapsulationKind::Cdr2Be
        );
        assert_eq!(
            be.for_extensibility(Extensibility::Appendable).kind(),
            EncapsulationKind::DCdr2Be
        );
        assert_eq!(
            be.for_extensibility(Extensibility::Mutable).kind(),
            EncapsulationKind::PlCdr2Be
        );

        // XCDR1 has only the plain and parameter-list forms.
        for (encoding, plain, mutable) in [
            (
                Encoding::new(EncapsulationKind::CdrLe),
                EncapsulationKind::CdrLe,
                EncapsulationKind::PlCdrLe,
            ),
            (
                Encoding::new(EncapsulationKind::CdrBe),
                EncapsulationKind::CdrBe,
                EncapsulationKind::PlCdrBe,
            ),
        ] {
            assert_eq!(
                encoding.for_extensibility(Extensibility::Final).kind(),
                plain
            );
            assert_eq!(
                encoding.for_extensibility(Extensibility::Appendable).kind(),
                plain
            );
            assert_eq!(
                encoding.for_extensibility(Extensibility::Mutable).kind(),
                mutable
            );
        }
    }

    #[test]
    fn declared_extensibility_inverts_the_selection_under_xcdr2() {
        use crate::xcdr2::Extensibility;

        for encoding in [
            Encoding::new(EncapsulationKind::Cdr2Le),
            Encoding::new(EncapsulationKind::Cdr2Be),
        ] {
            for extensibility in [
                Extensibility::Final,
                Extensibility::Appendable,
                Extensibility::Mutable,
            ] {
                assert_eq!(
                    encoding
                        .for_extensibility(extensibility)
                        .kind()
                        .declared_extensibility(),
                    extensibility
                );
            }
        }

        // Under XCDR1 an appendable type is written exactly like a final one,
        // so the identifier cannot tell them apart and reports `Final`.
        let v1 = Encoding::ROS2;
        assert_eq!(
            v1.for_extensibility(Extensibility::Appendable)
                .kind()
                .declared_extensibility(),
            Extensibility::Final
        );
        assert_eq!(
            v1.for_extensibility(Extensibility::Mutable)
                .kind()
                .declared_extensibility(),
            Extensibility::Mutable
        );
    }

    #[test]
    fn only_the_xcdr1_parameter_list_kinds_are_the_rtps_form() {
        // XCDR2's mutable form has no PID_SENTINEL — it tags members with an
        // EMHEADER and bounds them with a DHEADER — so it must not be fed to
        // a sentinel parser.
        for kind in EncapsulationKind::ALL {
            let rtps = matches!(
                kind,
                EncapsulationKind::PlCdrBe | EncapsulationKind::PlCdrLe
            );
            assert_eq!(kind.is_rtps_parameter_list(), rtps, "{kind:?}");
            if rtps {
                assert!(kind.is_parameter_list());
            }
        }
        assert!(EncapsulationKind::PlCdr2Le.is_parameter_list());
        assert!(!EncapsulationKind::PlCdr2Le.is_rtps_parameter_list());
    }

    #[test]
    fn extensibility_selection_preserves_the_wchar_width() {
        use crate::xcdr2::Extensibility;

        let wide = Encoding::new(EncapsulationKind::Cdr2Le).with_wchar_width(WCharWidth::Four);
        assert_eq!(
            wide.for_extensibility(Extensibility::Mutable).wchar_width(),
            WCharWidth::Four
        );
    }

    #[test]
    fn native_endianness_agrees_with_the_target() {
        let expected = if cfg!(target_endian = "big") {
            Endianness::Big
        } else {
            Endianness::Little
        };
        assert_eq!(Endianness::native(), expected);
        assert!(Endianness::Little.is_little());
        assert!(!Endianness::Big.is_little());
    }
}
