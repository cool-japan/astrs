//! `DATA` and `DATA_FRAG`: the two submessages that carry a sample.
//!
//! ```text
//!  DATA (§8.3.7.2)                       DATA_FRAG (§8.3.7.3)
//! +-------------+-------------+         +-------------+-------------+
//! |  extraFlags | octetsToIQ  |         |  extraFlags | octetsToIQ  |
//! +-------------+-------------+         +-------------+-------------+
//! |         readerId          |         |         readerId          |
//! |         writerId          |         |         writerId          |
//! |         writerSN (8)      |         |         writerSN (8)      |
//! +---------------------------+         |  fragmentStartingNum      |
//! ~  inlineQos   [if Q]       ~         |  fragsInSubmsg | fragSize |
//! +---------------------------+         |  sampleSize               |
//! ~  serializedPayload [D|K]  ~         +---------------------------+
//! +---------------------------+         ~  inlineQos   [if Q]       ~
//!                                       +---------------------------+
//!                                       ~  serializedPayload        ~
//!                                       +---------------------------+
//! ```
//!
//! # `octetsToInlineQos`
//!
//! The second field is the distance from the octet after it to the start of
//! `inlineQos` — 16 for a `DATA`, 28 for a `DATA_FRAG`. It exists so a later
//! minor version can insert fields between `writerSN` and `inlineQos` without
//! breaking a 2.3 reader, which skips forward by whatever the field says
//! (§8.3.7.2.2). A **larger** value is therefore legal and the extra octets
//! are stepped over; a **smaller** one would place `inlineQos` inside
//! `writerSN` and is refused with
//! [`RtpsError::InvalidOctetsToInlineQos`].
//!
//! # `D` and `K` are exclusive
//!
//! A `DATA`'s payload is the sample (`D`), or the key of a sample being
//! disposed or unregistered (`K`), or absent (neither) — never both, which is
//! why [`DataPayload`] makes the third state unrepresentable rather than
//! checking it after the fact.
//!
//! Note the flag positions: `DATA_FRAG` has no `D` flag, so its `K` and `N`
//! sit one bit lower than a `DATA`'s. See
//! [`flags`].
//!
//! ```
//! use astrs_rtps::messages::{Data, DataPayload, SerializedPayload};
//! use astrs_rtps::structure::{EntityId, EntityKind, SequenceNumber};
//!
//! let sample = Data::new(
//!     EntityId::UNKNOWN,
//!     EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
//!     SequenceNumber::FIRST,
//!     DataPayload::Data(SerializedPayload::from_cdr(&42_i32)?),
//! );
//! assert_eq!(sample.flags().raw(), 0x05); // E | D
//! assert_eq!(sample.body_len(), 20 + 8);
//! sample.validate()?;
//! # Ok::<(), astrs_rtps::RtpsError>(())
//! ```

use core::fmt;

use astrs_cdr::{CdrDeserialize, CdrReader, CdrSerialize, CdrWriter, Endianness, ParameterList};

use crate::error::{FragmentDefect, RtpsError, RtpsResult};
use crate::messages::flags::{self, Extension, SubmessageFlags, body_encoding};
use crate::messages::header::SubmessageHeader;
use crate::messages::kind::SubmessageId;
use crate::messages::payload::{SerializedPayload, read_inline_qos, relabel_inline_qos};
use crate::structure::fragment::FragmentNumber;
use crate::structure::guid::EntityId;
use crate::structure::sequence::SequenceNumber;

/// Octets of `DATA` between `octetsToInlineQos` and `inlineQos`.
///
/// `readerId` + `writerId` + `writerSN` = 4 + 4 + 8.
pub const DATA_OCTETS_TO_INLINE_QOS: usize = 16;

/// Octets of `DATA_FRAG` between `octetsToInlineQos` and `inlineQos`.
///
/// [`DATA_OCTETS_TO_INLINE_QOS`] plus `fragmentStartingNum` (4),
/// `fragmentsInSubmessage` (2), `fragmentSize` (2) and `sampleSize` (4).
pub const DATA_FRAG_OCTETS_TO_INLINE_QOS: usize = 28;

/// Octets before `inlineQos` in a `DATA` body, `extraFlags` and
/// `octetsToInlineQos` included.
pub const DATA_PRELUDE_LEN: usize = 4 + DATA_OCTETS_TO_INLINE_QOS;

/// Octets before `inlineQos` in a `DATA_FRAG` body.
pub const DATA_FRAG_PRELUDE_LEN: usize = 4 + DATA_FRAG_OCTETS_TO_INLINE_QOS;

/// What a `DATA` submessage's `serializedPayload` holds.
///
/// The `D` and `K` flags of §8.3.7.2.5 are exclusive, and this enum is how
/// that exclusivity is made unrepresentable rather than merely checked.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum DataPayload<'a> {
    /// Neither flag: the submessage carries no payload at all.
    ///
    /// A writer sends this to advance a reader's sequence-number state
    /// without delivering anything — the shape a `DATA` takes when the
    /// sample it would have carried is irrelevant to the reader.
    #[default]
    None,
    /// `D`: the payload is the serialized sample.
    Data(SerializedPayload<'a>),
    /// `K`: the payload is the serialized *key* of a sample being disposed
    /// or unregistered. The `statusInfo` inline QoS parameter says which.
    Key(SerializedPayload<'a>),
}

impl<'a> DataPayload<'a> {
    /// The payload octets, whichever flag they belong to.
    #[must_use]
    pub const fn payload(&self) -> Option<&SerializedPayload<'a>> {
        match self {
            Self::None => None,
            Self::Data(payload) | Self::Key(payload) => Some(payload),
        }
    }

    /// Octets the payload occupies in the submessage body.
    #[must_use]
    pub fn len(&self) -> usize {
        match self.payload() {
            Some(payload) => payload.len(),
            None => 0,
        }
    }

    /// True when there is no payload, or the payload has no octets.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// True for [`DataPayload::None`].
    #[must_use]
    pub const fn is_absent(&self) -> bool {
        matches!(self, Self::None)
    }

    /// The `D` and `K` bits this payload implies.
    #[must_use]
    const fn flag_bits(&self, data_bit: u8, key_bit: u8) -> u8 {
        match self {
            Self::None => 0,
            Self::Data(_) => data_bit,
            Self::Key(_) => key_bit,
        }
    }

    /// Detach from the input buffer.
    #[must_use]
    pub fn into_owned(self) -> DataPayload<'static> {
        match self {
            Self::None => DataPayload::None,
            Self::Data(payload) => DataPayload::Data(payload.into_owned()),
            Self::Key(payload) => DataPayload::Key(payload.into_owned()),
        }
    }
}

/// The four fields that describe how a sample was cut into fragments.
///
/// They only ever make sense together — [`DataFrag::validate`],
/// [`DataFrag::total_fragments`] and [`DataFrag::payload_offset`] each read
/// all four — so the constructor takes them as one value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FragmentGeometry {
    /// One-based number of the first fragment in the submessage.
    pub starting_num: FragmentNumber,
    /// How many consecutive fragments the payload holds.
    pub fragments_in_submessage: u16,
    /// Octets per fragment, the sample's last one excepted.
    pub fragment_size: u16,
    /// Octets the whole `serializedPayload` of the sample occupies.
    pub sample_size: u32,
}

impl FragmentGeometry {
    /// Describe a submessage carrying `count` fragments from `starting_num`.
    #[must_use]
    pub const fn new(
        starting_num: FragmentNumber,
        fragments_in_submessage: u16,
        fragment_size: u16,
        sample_size: u32,
    ) -> Self {
        Self {
            starting_num,
            fragments_in_submessage,
            fragment_size,
            sample_size,
        }
    }

    /// Total fragments the sample is divided into: `ceil(sampleSize /
    /// fragmentSize)`, or `None` when `fragmentSize` is zero.
    #[must_use]
    pub const fn total_fragments(self) -> Option<u32> {
        if self.fragment_size == 0 {
            return None;
        }
        Some(self.sample_size.div_ceil(self.fragment_size as u32))
    }
}

/// The `DATA` submessage (§8.3.7.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Data<'a> {
    /// Byte order of the body.
    pub endianness: Endianness,
    /// The `extraFlags` field, reserved by §8.3.7.2.2 and preserved verbatim.
    pub extra_flags: u16,
    /// The reader this sample is addressed to, or
    /// [`EntityId::UNKNOWN`] for every reader of the writer's topic.
    pub reader_id: EntityId,
    /// The writer that produced the sample.
    pub writer_id: EntityId,
    /// The sample's sequence number.
    pub writer_sn: SequenceNumber,
    /// Per-sample QoS, as a `PL_CDR` parameter list (`Q` flag).
    pub inline_qos: Option<ParameterList<'a>>,
    /// The sample, its key, or nothing.
    pub payload: DataPayload<'a>,
    /// `N`: the payload is not CDR.
    ///
    /// AstRS never sets it. A peer that does is announcing a payload this
    /// stack cannot interpret, which the behavior half reports rather than
    /// guesses at.
    pub non_standard_payload: bool,
    /// What this peer said that this build does not interpret.
    pub extension: Extension,
}

impl<'a> Data<'a> {
    /// The flag bits §8.3.7.2.1 defines for `DATA`.
    pub const DEFINED_FLAGS: u8 = flags::ENDIANNESS
        | flags::INLINE_QOS
        | flags::DATA
        | flags::KEY
        | flags::NON_STANDARD_PAYLOAD;

    /// A little-endian `DATA` with no inline QoS.
    #[must_use]
    pub fn new(
        reader_id: EntityId,
        writer_id: EntityId,
        writer_sn: SequenceNumber,
        payload: DataPayload<'a>,
    ) -> Self {
        Self {
            endianness: Endianness::Little,
            extra_flags: 0,
            reader_id,
            writer_id,
            writer_sn,
            inline_qos: None,
            payload,
            non_standard_payload: false,
            extension: Extension::EMPTY,
        }
    }

    /// The same submessage with inline QoS attached.
    ///
    /// The list is relabelled to the submessage's byte order (see
    /// [`relabel_inline_qos`]), which keeps the invariant that an inline-QoS
    /// list always carries the parameter-list encoding matching the `E` flag.
    #[must_use]
    pub fn with_inline_qos(mut self, inline_qos: ParameterList<'a>) -> Self {
        self.inline_qos = Some(relabel_inline_qos(inline_qos, self.endianness));
        self
    }

    /// The same submessage in the stated byte order.
    ///
    /// Any inline QoS already attached is relabelled with it.
    #[must_use]
    pub fn with_endianness(mut self, endianness: Endianness) -> Self {
        self.endianness = endianness;
        self.inline_qos = self
            .inline_qos
            .map(|list| relabel_inline_qos(list, endianness));
        self
    }

    /// The flags octet this submessage encodes to.
    #[must_use]
    pub fn flags(&self) -> SubmessageFlags {
        SubmessageFlags::from_endianness(self.endianness)
            .set(flags::INLINE_QOS, self.inline_qos.is_some())
            .with(self.payload.flag_bits(flags::DATA, flags::KEY))
            .set(flags::NON_STANDARD_PAYLOAD, self.non_standard_payload)
            .with(self.extension.reserved_flags)
    }

    /// Octets the body occupies.
    #[must_use]
    pub fn body_len(&self) -> usize {
        DATA_PRELUDE_LEN
            + self
                .inline_qos
                .as_ref()
                .map_or(0, ParameterList::serialized_len)
            + self.payload.len()
            + self.extension.trailing_len()
    }

    /// Check the validity clauses of §8.3.7.2.3.
    ///
    /// # Errors
    ///
    /// [`RtpsError::InvalidSequenceNumber`] when `writerSN` is not strictly
    /// positive — which also rejects `SEQUENCENUMBER_UNKNOWN`.
    pub fn validate(&self) -> RtpsResult<()> {
        self.writer_sn.check_valid("DATA writerSN")?;
        Ok(())
    }

    /// Detach every borrowed field from the input buffer.
    #[must_use]
    pub fn into_owned(self) -> Data<'static> {
        Data {
            endianness: self.endianness,
            extra_flags: self.extra_flags,
            reader_id: self.reader_id,
            writer_id: self.writer_id,
            writer_sn: self.writer_sn,
            inline_qos: self.inline_qos.map(ParameterList::into_owned),
            payload: self.payload.into_owned(),
            non_standard_payload: self.non_standard_payload,
            extension: self.extension,
        }
    }

    /// Write the body — everything after the four-octet submessage header.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`] from a field write or from the inline QoS list.
    pub fn write_body(&self, writer: &mut CdrWriter) -> RtpsResult<()> {
        writer.write_u16(self.extra_flags)?;
        writer.write_u16(DATA_OCTETS_TO_INLINE_QOS as u16)?;
        self.reader_id.serialize(writer)?;
        self.writer_id.serialize(writer)?;
        self.writer_sn.serialize(writer)?;
        if let Some(inline_qos) = &self.inline_qos {
            inline_qos.write(writer)?;
        }
        if let Some(payload) = self.payload.payload() {
            writer.write_octets(payload.as_slice());
        }
        writer.write_octets(&self.extension.trailing);
        Ok(())
    }

    /// Read a body against the flags its header carried.
    ///
    /// # Errors
    ///
    /// - [`RtpsError::ConflictingDataFlags`] when both `D` and `K` are set.
    /// - [`RtpsError::InvalidOctetsToInlineQos`] when the field is below 16.
    /// - [`RtpsError::Cdr`] when the body ends inside a field or the inline
    ///   QoS list is malformed.
    pub fn read(header: &SubmessageHeader, body: &'a [u8]) -> RtpsResult<Self> {
        let endianness = header.endianness();
        let has_inline_qos = header.flags.has(flags::INLINE_QOS);
        let has_data = header.flags.has(flags::DATA);
        let has_key = header.flags.has(flags::KEY);
        if has_data && has_key {
            return Err(RtpsError::ConflictingDataFlags);
        }

        let mut reader = CdrReader::with_encoding(body, body_encoding(endianness));
        let extra_flags = reader.read_u16()?;
        let declared = usize::from(reader.read_u16()?);
        if declared < DATA_OCTETS_TO_INLINE_QOS {
            return Err(RtpsError::InvalidOctetsToInlineQos {
                id: SubmessageId::DATA,
                declared,
                minimum: DATA_OCTETS_TO_INLINE_QOS,
            });
        }
        let reader_id = EntityId::deserialize(&mut reader)?;
        let writer_id = EntityId::deserialize(&mut reader)?;
        let writer_sn = SequenceNumber::deserialize(&mut reader)?;
        // §8.3.7.2.2 forward compatibility: step over whatever a later minor
        // version put between writerSN and inlineQos.
        reader.skip(declared - DATA_OCTETS_TO_INLINE_QOS)?;

        let inline_qos = if has_inline_qos {
            Some(read_inline_qos(&mut reader, endianness)?)
        } else {
            None
        };

        let remaining = reader.peek_remaining();
        let (payload, trailing) = if has_data {
            (
                DataPayload::Data(SerializedPayload::new(remaining)),
                &[][..],
            )
        } else if has_key {
            (DataPayload::Key(SerializedPayload::new(remaining)), &[][..])
        } else {
            // No payload: anything left is a §8.6 tail extension.
            (DataPayload::None, remaining)
        };

        Ok(Self {
            endianness,
            extra_flags,
            reader_id,
            writer_id,
            writer_sn,
            inline_qos,
            payload,
            non_standard_payload: header.flags.has(flags::NON_STANDARD_PAYLOAD),
            extension: Extension {
                reserved_flags: header.flags.reserved(Self::DEFINED_FLAGS),
                trailing: trailing.to_vec(),
            },
        })
    }
}

impl fmt::Display for Data<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "DATA {} -> {} sn {} ({} payload octets)",
            self.writer_id,
            self.reader_id,
            self.writer_sn,
            self.payload.len()
        )
    }
}

/// The `DATA_FRAG` submessage (§8.3.7.3).
///
/// One `DATA_FRAG` carries `fragmentsInSubmessage` consecutive fragments of
/// the sample `writerSN`, starting at `fragmentStartingNum`. Every fragment
/// is `fragmentSize` octets except the last one of the sample, which is
/// whatever remains of `sampleSize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataFrag<'a> {
    /// Byte order of the body.
    pub endianness: Endianness,
    /// The `extraFlags` field, reserved and preserved verbatim.
    pub extra_flags: u16,
    /// The reader this fragment is addressed to.
    pub reader_id: EntityId,
    /// The writer that produced the sample.
    pub writer_id: EntityId,
    /// The sequence number of the sample being fragmented.
    pub writer_sn: SequenceNumber,
    /// One-based number of the first fragment in this submessage.
    pub fragment_starting_num: FragmentNumber,
    /// How many consecutive fragments the payload holds.
    pub fragments_in_submessage: u16,
    /// Octets per fragment, the last one of the sample excepted.
    pub fragment_size: u16,
    /// Octets the whole `serializedPayload` of the sample occupies.
    pub sample_size: u32,
    /// Per-sample QoS (`Q` flag).
    pub inline_qos: Option<ParameterList<'a>>,
    /// The fragments themselves, concatenated.
    pub payload: SerializedPayload<'a>,
    /// `K`: the fragments belong to a key rather than a sample.
    pub key: bool,
    /// `N`: the payload is not CDR.
    pub non_standard_payload: bool,
    /// What this peer said that this build does not interpret.
    pub extension: Extension,
}

impl<'a> DataFrag<'a> {
    /// The flag bits §8.3.7.3.1 defines for `DATA_FRAG`.
    ///
    /// One bit narrower than [`Data::DEFINED_FLAGS`]: there is no `D` flag,
    /// because a `DATA_FRAG` always carries payload.
    pub const DEFINED_FLAGS: u8 =
        flags::ENDIANNESS | flags::INLINE_QOS | flags::KEY_FRAG | flags::NON_STANDARD_PAYLOAD_FRAG;

    /// A little-endian `DATA_FRAG` with no inline QoS.
    #[must_use]
    pub fn new(
        reader_id: EntityId,
        writer_id: EntityId,
        writer_sn: SequenceNumber,
        geometry: FragmentGeometry,
        payload: SerializedPayload<'a>,
    ) -> Self {
        Self {
            endianness: Endianness::Little,
            extra_flags: 0,
            reader_id,
            writer_id,
            writer_sn,
            fragment_starting_num: geometry.starting_num,
            fragments_in_submessage: geometry.fragments_in_submessage,
            fragment_size: geometry.fragment_size,
            sample_size: geometry.sample_size,
            inline_qos: None,
            payload,
            key: false,
            non_standard_payload: false,
            extension: Extension::EMPTY,
        }
    }

    /// The four fragmentation fields as one value.
    #[must_use]
    pub const fn geometry(&self) -> FragmentGeometry {
        FragmentGeometry::new(
            self.fragment_starting_num,
            self.fragments_in_submessage,
            self.fragment_size,
            self.sample_size,
        )
    }

    /// The same submessage with inline QoS attached.
    ///
    /// The list is relabelled to the submessage's byte order, exactly as in
    /// [`Data::with_inline_qos`].
    #[must_use]
    pub fn with_inline_qos(mut self, inline_qos: ParameterList<'a>) -> Self {
        self.inline_qos = Some(relabel_inline_qos(inline_qos, self.endianness));
        self
    }

    /// The same submessage in the stated byte order.
    ///
    /// Any inline QoS already attached is relabelled with it.
    #[must_use]
    pub fn with_endianness(mut self, endianness: Endianness) -> Self {
        self.endianness = endianness;
        self.inline_qos = self
            .inline_qos
            .map(|list| relabel_inline_qos(list, endianness));
        self
    }

    /// The flags octet this submessage encodes to.
    #[must_use]
    pub fn flags(&self) -> SubmessageFlags {
        SubmessageFlags::from_endianness(self.endianness)
            .set(flags::INLINE_QOS, self.inline_qos.is_some())
            .set(flags::KEY_FRAG, self.key)
            .set(flags::NON_STANDARD_PAYLOAD_FRAG, self.non_standard_payload)
            .with(self.extension.reserved_flags)
    }

    /// Octets the body occupies.
    #[must_use]
    pub fn body_len(&self) -> usize {
        DATA_FRAG_PRELUDE_LEN
            + self
                .inline_qos
                .as_ref()
                .map_or(0, ParameterList::serialized_len)
            + self.payload.len()
            + self.extension.trailing_len()
    }

    /// Total fragments the sample is divided into: `ceil(sampleSize /
    /// fragmentSize)`.
    ///
    /// `None` when `fragmentSize` is zero, which
    /// [`DataFrag::validate`] rejects.
    #[must_use]
    pub const fn total_fragments(&self) -> Option<u32> {
        self.geometry().total_fragments()
    }

    /// One past the last fragment number this submessage carries.
    ///
    /// Saturating, so a hostile `fragmentStartingNum` near `u32::MAX` cannot
    /// wrap into a small number.
    #[must_use]
    pub const fn fragment_end(&self) -> u32 {
        self.fragment_starting_num
            .value()
            .saturating_add(self.fragments_in_submessage as u32)
    }

    /// Byte offset of this submessage's first fragment within the sample's
    /// `serializedPayload`.
    ///
    /// The number a reassembler copies the payload to.
    #[must_use]
    pub const fn payload_offset(&self) -> u64 {
        (self.fragment_starting_num.value() as u64 - 1) * (self.fragment_size as u64)
    }

    /// Check the validity clauses of §8.3.7.3.3.
    ///
    /// In order: `writerSN` positive, `fragmentStartingNum` positive,
    /// `fragmentSize` and `fragmentsInSubmessage` nonzero, `fragmentSize`
    /// no larger than `sampleSize`, the fragment window inside the sample,
    /// and the payload long enough for the fragments claimed.
    ///
    /// # Errors
    ///
    /// [`RtpsError::InvalidSequenceNumber`],
    /// [`RtpsError::InvalidFragmentNumber`] or
    /// [`RtpsError::InvalidFragmentGeometry`] naming the clause that failed.
    pub fn validate(&self) -> RtpsResult<()> {
        self.writer_sn.check_valid("DATA_FRAG writerSN")?;
        self.fragment_starting_num
            .check_valid("DATA_FRAG fragmentStartingNum")?;
        if self.fragment_size == 0 {
            return Err(RtpsError::InvalidFragmentGeometry {
                reason: FragmentDefect::ZeroFragmentSize,
            });
        }
        if self.fragments_in_submessage == 0 {
            return Err(RtpsError::InvalidFragmentGeometry {
                reason: FragmentDefect::NoFragments,
            });
        }
        if u32::from(self.fragment_size) > self.sample_size {
            return Err(RtpsError::InvalidFragmentGeometry {
                reason: FragmentDefect::FragmentLargerThanSample {
                    fragment_size: self.fragment_size,
                    sample_size: self.sample_size,
                },
            });
        }
        let total = self
            .total_fragments()
            .ok_or(RtpsError::InvalidFragmentGeometry {
                reason: FragmentDefect::ZeroFragmentSize,
            })?;
        let end = self.fragment_end();
        if end.saturating_sub(1) > total {
            return Err(RtpsError::InvalidFragmentGeometry {
                reason: FragmentDefect::WindowPastEnd {
                    starting: self.fragment_starting_num.value(),
                    count: self.fragments_in_submessage,
                    total,
                },
            });
        }
        // Every fragment but the sample's last must be full, so the payload
        // has a computable minimum.
        let count = u64::from(self.fragments_in_submessage);
        let size = u64::from(self.fragment_size);
        let includes_last = end.saturating_sub(1) == total;
        let needed = if includes_last {
            (count - 1) * size + 1
        } else {
            count * size
        };
        if (self.payload.len() as u64) < needed {
            return Err(RtpsError::InvalidFragmentGeometry {
                reason: FragmentDefect::PayloadTooShort {
                    needed: needed as usize,
                    available: self.payload.len(),
                },
            });
        }
        Ok(())
    }

    /// Detach every borrowed field from the input buffer.
    #[must_use]
    pub fn into_owned(self) -> DataFrag<'static> {
        DataFrag {
            endianness: self.endianness,
            extra_flags: self.extra_flags,
            reader_id: self.reader_id,
            writer_id: self.writer_id,
            writer_sn: self.writer_sn,
            fragment_starting_num: self.fragment_starting_num,
            fragments_in_submessage: self.fragments_in_submessage,
            fragment_size: self.fragment_size,
            sample_size: self.sample_size,
            inline_qos: self.inline_qos.map(ParameterList::into_owned),
            payload: self.payload.into_owned(),
            key: self.key,
            non_standard_payload: self.non_standard_payload,
            extension: self.extension,
        }
    }

    /// Write the body — everything after the four-octet submessage header.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`] from a field write or from the inline QoS list.
    pub fn write_body(&self, writer: &mut CdrWriter) -> RtpsResult<()> {
        writer.write_u16(self.extra_flags)?;
        writer.write_u16(DATA_FRAG_OCTETS_TO_INLINE_QOS as u16)?;
        self.reader_id.serialize(writer)?;
        self.writer_id.serialize(writer)?;
        self.writer_sn.serialize(writer)?;
        self.fragment_starting_num.serialize(writer)?;
        writer.write_u16(self.fragments_in_submessage)?;
        writer.write_u16(self.fragment_size)?;
        writer.write_u32(self.sample_size)?;
        if let Some(inline_qos) = &self.inline_qos {
            inline_qos.write(writer)?;
        }
        writer.write_octets(self.payload.as_slice());
        writer.write_octets(&self.extension.trailing);
        Ok(())
    }

    /// Read a body against the flags its header carried.
    ///
    /// # Errors
    ///
    /// - [`RtpsError::InvalidOctetsToInlineQos`] when the field is below 28.
    /// - [`RtpsError::Cdr`] when the body ends inside a field or the inline
    ///   QoS list is malformed.
    pub fn read(header: &SubmessageHeader, body: &'a [u8]) -> RtpsResult<Self> {
        let endianness = header.endianness();
        let mut reader = CdrReader::with_encoding(body, body_encoding(endianness));
        let extra_flags = reader.read_u16()?;
        let declared = usize::from(reader.read_u16()?);
        if declared < DATA_FRAG_OCTETS_TO_INLINE_QOS {
            return Err(RtpsError::InvalidOctetsToInlineQos {
                id: SubmessageId::DATA_FRAG,
                declared,
                minimum: DATA_FRAG_OCTETS_TO_INLINE_QOS,
            });
        }
        let reader_id = EntityId::deserialize(&mut reader)?;
        let writer_id = EntityId::deserialize(&mut reader)?;
        let writer_sn = SequenceNumber::deserialize(&mut reader)?;
        let fragment_starting_num = FragmentNumber::deserialize(&mut reader)?;
        let fragments_in_submessage = reader.read_u16()?;
        let fragment_size = reader.read_u16()?;
        let sample_size = reader.read_u32()?;
        reader.skip(declared - DATA_FRAG_OCTETS_TO_INLINE_QOS)?;

        let inline_qos = if header.flags.has(flags::INLINE_QOS) {
            Some(read_inline_qos(&mut reader, endianness)?)
        } else {
            None
        };

        Ok(Self {
            endianness,
            extra_flags,
            reader_id,
            writer_id,
            writer_sn,
            fragment_starting_num,
            fragments_in_submessage,
            fragment_size,
            sample_size,
            inline_qos,
            payload: SerializedPayload::new(reader.peek_remaining()),
            key: header.flags.has(flags::KEY_FRAG),
            non_standard_payload: header.flags.has(flags::NON_STANDARD_PAYLOAD_FRAG),
            extension: Extension::from_reserved_flags(header.flags.reserved(Self::DEFINED_FLAGS)),
        })
    }
}

impl fmt::Display for DataFrag<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "DATA_FRAG {} -> {} sn {} frags {}..{} of {} ({} octets each)",
            self.writer_id,
            self.reader_id,
            self.writer_sn,
            self.fragment_starting_num,
            self.fragment_end().saturating_sub(1),
            self.total_fragments().unwrap_or(0),
            self.fragment_size
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use astrs_cdr::{Encoding, ParameterId, pid};

    use super::*;
    use crate::structure::guid::EntityKind;

    fn writer_id() -> EntityId {
        EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY)
    }

    fn round_trip(data: &Data<'_>) -> Data<'static> {
        let mut writer = CdrWriter::headerless(body_encoding(data.endianness));
        data.write_body(&mut writer).expect("write");
        let body = writer.finish();
        assert_eq!(body.len(), data.body_len());
        let header = SubmessageHeader::new(SubmessageId::Data, data.flags(), 0);
        Data::read(&header, &body).expect("read").into_owned()
    }

    #[test]
    fn a_plain_data_is_a_prelude_and_a_payload() {
        let data = Data::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            DataPayload::Data(SerializedPayload::from_cdr(&1_u32).expect("encode")),
        );
        assert_eq!(data.flags().raw(), 0x05);
        assert_eq!(data.body_len(), 20 + 8);
        assert_eq!(data.validate(), Ok(()));

        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        data.write_body(&mut writer).expect("write");
        assert_eq!(
            writer.finish(),
            [
                0x00, 0x00, // extraFlags
                0x10, 0x00, // octetsToInlineQos = 16
                0x00, 0x00, 0x00, 0x00, // readerId = ENTITYID_UNKNOWN
                0x00, 0x00, 0x01, 0x03, // writerId
                0x00, 0x00, 0x00, 0x00, // writerSN.high = 0
                0x01, 0x00, 0x00, 0x00, // writerSN.low = 1
                0x00, 0x01, 0x00, 0x00, // payload: CDR_LE header
                0x01, 0x00, 0x00, 0x00, // payload: the u32
            ]
        );
        assert_eq!(round_trip(&data), data.clone().into_owned());
        assert_eq!(
            data.to_string(),
            "DATA 00.00.01.03 -> ENTITYID_UNKNOWN sn 1 (8 payload octets)"
        );
    }

    #[test]
    fn the_data_and_key_flags_are_exclusive() {
        let key = Data::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            DataPayload::Key(SerializedPayload::from_cdr(&2_u32).expect("encode")),
        );
        assert_eq!(key.flags().raw(), 0x09); // E | K
        assert_eq!(round_trip(&key), key.clone().into_owned());

        // A peer that sets both is refused rather than guessed at.
        let header = SubmessageHeader::new(
            SubmessageId::Data,
            SubmessageFlags::new(flags::ENDIANNESS | flags::DATA | flags::KEY),
            0,
        );
        let body = [0_u8; 20];
        assert_eq!(
            Data::read(&header, &body).map(|_| ()),
            Err(RtpsError::ConflictingDataFlags)
        );
    }

    #[test]
    fn a_data_with_neither_flag_carries_no_payload() {
        let bare = Data::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::new(9),
            DataPayload::None,
        );
        assert_eq!(bare.flags().raw(), 0x01);
        assert_eq!(bare.body_len(), 20);
        assert!(bare.payload.is_absent());
        assert!(bare.payload.is_empty());
        assert_eq!(bare.payload.payload(), None);
        assert_eq!(round_trip(&bare), bare.clone().into_owned());
        assert_eq!(DataPayload::default(), DataPayload::None);
    }

    #[test]
    fn inline_qos_sits_between_the_prelude_and_the_payload() {
        let mut qos = ParameterList::new(Encoding::DISCOVERY);
        qos.push_octets(
            ParameterId::new(pid::STATUS_INFO),
            &[0x00_u8, 0x00, 0x00, 0x03][..],
        )
        .expect("short enough");
        let data = Data::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            DataPayload::Key(SerializedPayload::from_cdr(&7_u32).expect("encode")),
        )
        .with_inline_qos(qos);

        assert_eq!(data.flags().raw(), 0x0b); // E | Q | K
        // 20 prelude + (4 header + 4 value + 4 sentinel) + 8 payload.
        assert_eq!(data.body_len(), 20 + 12 + 8);
        assert_eq!(round_trip(&data), data.clone().into_owned());

        let decoded = round_trip(&data);
        let list = decoded.inline_qos.as_ref().expect("present");
        assert_eq!(
            list.get_by_base(pid::STATUS_INFO)
                .expect("status info")
                .value
                .as_ref(),
            &[0x00, 0x00, 0x00, 0x03]
        );
    }

    #[test]
    fn a_big_endian_data_swaps_only_the_numeric_fields() {
        let data = Data::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::new(0x0000_0002_0000_0003),
            DataPayload::None,
        )
        .with_endianness(Endianness::Big);
        assert_eq!(data.flags().raw(), 0x00);

        let mut writer = CdrWriter::headerless(body_encoding(Endianness::Big));
        data.write_body(&mut writer).expect("write");
        assert_eq!(
            writer.finish(),
            [
                0x00, 0x00, // extraFlags
                0x00, 0x10, // octetsToInlineQos = 16, big-endian
                0x00, 0x00, 0x00, 0x00, // readerId — octets, unswapped
                0x00, 0x00, 0x01, 0x03, // writerId — octets, unswapped
                0x00, 0x00, 0x00, 0x02, // writerSN.high, big-endian
                0x00, 0x00, 0x00, 0x03, // writerSN.low, big-endian
            ]
        );
        assert_eq!(round_trip(&data), data.clone().into_owned());
    }

    #[test]
    fn a_larger_octets_to_inline_qos_skips_forward() {
        // A later minor version inserted four octets before inlineQos.
        let mut body = Vec::from([0x00_u8, 0x00, 0x14, 0x00]); // declares 20
        body.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // readerId
        body.extend_from_slice(&[0x00, 0x00, 0x01, 0x03]); // writerId
        body.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // writerSN.high
        body.extend_from_slice(&[0x05, 0x00, 0x00, 0x00]); // writerSN.low
        body.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]); // the new field
        body.extend_from_slice(&[0x00, 0x01, 0x00, 0x00, 1, 0, 0, 0]); // payload

        let header = SubmessageHeader::new(
            SubmessageId::Data,
            SubmessageFlags::new(flags::ENDIANNESS | flags::DATA),
            0,
        );
        let data = Data::read(&header, &body).expect("read");
        assert_eq!(data.writer_sn, SequenceNumber::new(5));
        assert_eq!(data.payload.len(), 8);
    }

    #[test]
    fn a_smaller_octets_to_inline_qos_would_overlap_writer_sn_and_is_refused() {
        let mut body = Vec::from([0x00_u8, 0x00, 0x04, 0x00]); // declares 4
        body.resize(20, 0);
        let header = SubmessageHeader::new(
            SubmessageId::Data,
            SubmessageFlags::new(flags::ENDIANNESS),
            0,
        );
        assert_eq!(
            Data::read(&header, &body).map(|_| ()),
            Err(RtpsError::InvalidOctetsToInlineQos {
                id: 0x15,
                declared: 4,
                minimum: 16,
            })
        );
    }

    #[test]
    fn a_zero_sequence_number_fails_the_validity_clause() {
        let data = Data::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::ZERO,
            DataPayload::None,
        );
        assert_eq!(
            data.validate(),
            Err(RtpsError::InvalidSequenceNumber {
                value: 0,
                context: "DATA writerSN",
            })
        );
        let unknown = Data::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::UNKNOWN,
            DataPayload::None,
        );
        assert!(unknown.validate().is_err());
    }

    #[test]
    fn reserved_flag_bits_survive_a_round_trip() {
        let header = SubmessageHeader::new(
            SubmessageId::Data,
            SubmessageFlags::new(flags::ENDIANNESS | 0x40),
            0,
        );
        let body = [
            0_u8, 0, 0x10, 0, 0, 0, 0, 0, 0, 0, 1, 3, 0, 0, 0, 0, 1, 0, 0, 0,
        ];
        let data = Data::read(&header, &body).expect("read");
        assert_eq!(data.extension.reserved_flags, 0x40);
        assert_eq!(data.flags().raw(), flags::ENDIANNESS | 0x40);
    }

    #[test]
    fn a_data_frag_prelude_is_twelve_octets_longer_than_a_data() {
        let frag = DataFrag::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            FragmentGeometry::new(FragmentNumber::FIRST, 2, 1_000, 2_500),
            SerializedPayload::new(vec![0_u8; 2_000]),
        );
        assert_eq!(frag.flags().raw(), 0x01);
        assert_eq!(DATA_FRAG_PRELUDE_LEN, DATA_PRELUDE_LEN + 12);
        assert_eq!(frag.body_len(), 32 + 2_000);
        assert_eq!(frag.total_fragments(), Some(3));
        assert_eq!(frag.fragment_end(), 3);
        assert_eq!(frag.payload_offset(), 0);
        assert_eq!(frag.validate(), Ok(()));

        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        frag.write_body(&mut writer).expect("write");
        let body = writer.finish();
        assert_eq!(
            &body[..32],
            &[
                0x00, 0x00, // extraFlags
                0x1c, 0x00, // octetsToInlineQos = 28
                0x00, 0x00, 0x00, 0x00, // readerId
                0x00, 0x00, 0x01, 0x03, // writerId
                0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // writerSN = 1
                0x01, 0x00, 0x00, 0x00, // fragmentStartingNum = 1
                0x02, 0x00, // fragmentsInSubmessage = 2
                0xe8, 0x03, // fragmentSize = 1000
                0xc4, 0x09, 0x00, 0x00, // sampleSize = 2500
            ]
        );

        let header = SubmessageHeader::new(SubmessageId::DataFrag, frag.flags(), 0);
        let decoded = DataFrag::read(&header, &body).expect("read");
        assert_eq!(decoded.into_owned(), frag.clone().into_owned());
    }

    #[test]
    fn the_data_frag_key_flag_is_one_bit_below_the_data_one() {
        let mut frag = DataFrag::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            FragmentGeometry::new(FragmentNumber::FIRST, 1, 4, 4),
            SerializedPayload::new(vec![0_u8; 4]),
        );
        frag.key = true;
        assert_eq!(frag.flags().raw(), flags::ENDIANNESS | 0x04);
        frag.non_standard_payload = true;
        assert_eq!(frag.flags().raw(), flags::ENDIANNESS | 0x04 | 0x08);

        let header = SubmessageHeader::new(SubmessageId::DataFrag, frag.flags(), 0);
        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        frag.write_body(&mut writer).expect("write");
        let body = writer.finish();
        let decoded = DataFrag::read(&header, &body).expect("read");
        assert!(decoded.key);
        assert!(decoded.non_standard_payload);
    }

    #[test]
    fn the_last_fragment_of_a_sample_may_be_short() {
        // 2500 octets in 1000-octet fragments: 3 fragments, the last 500.
        let last = DataFrag::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            FragmentGeometry::new(FragmentNumber::new(3), 1, 1_000, 2_500),
            SerializedPayload::new(vec![0_u8; 500]),
        );
        assert_eq!(last.validate(), Ok(()));
        assert_eq!(last.payload_offset(), 2_000);
        assert_eq!(
            last.to_string(),
            "DATA_FRAG 00.00.01.03 -> ENTITYID_UNKNOWN sn 1 frags 3..3 of 3 (1000 octets each)"
        );

        // A middle fragment must be full.
        let short_middle = DataFrag::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            FragmentGeometry::new(FragmentNumber::new(2), 1, 1_000, 2_500),
            SerializedPayload::new(vec![0_u8; 500]),
        );
        assert_eq!(
            short_middle.validate(),
            Err(RtpsError::InvalidFragmentGeometry {
                reason: FragmentDefect::PayloadTooShort {
                    needed: 1_000,
                    available: 500,
                },
            })
        );
    }

    #[test]
    fn every_fragment_geometry_clause_is_checked() {
        let base = DataFrag::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            FragmentGeometry::new(FragmentNumber::FIRST, 1, 1_000, 2_500),
            SerializedPayload::new(vec![0_u8; 1_000]),
        );
        assert_eq!(base.validate(), Ok(()));

        let mut zero_size = base.clone();
        zero_size.fragment_size = 0;
        assert_eq!(
            zero_size.validate(),
            Err(RtpsError::InvalidFragmentGeometry {
                reason: FragmentDefect::ZeroFragmentSize,
            })
        );
        assert_eq!(zero_size.total_fragments(), None);

        let mut no_frags = base.clone();
        no_frags.fragments_in_submessage = 0;
        assert_eq!(
            no_frags.validate(),
            Err(RtpsError::InvalidFragmentGeometry {
                reason: FragmentDefect::NoFragments,
            })
        );

        let mut too_big = base.clone();
        too_big.sample_size = 100;
        assert_eq!(
            too_big.validate(),
            Err(RtpsError::InvalidFragmentGeometry {
                reason: FragmentDefect::FragmentLargerThanSample {
                    fragment_size: 1_000,
                    sample_size: 100,
                },
            })
        );

        let mut past_end = base.clone();
        past_end.fragment_starting_num = FragmentNumber::new(3);
        past_end.fragments_in_submessage = 2;
        assert_eq!(
            past_end.validate(),
            Err(RtpsError::InvalidFragmentGeometry {
                reason: FragmentDefect::WindowPastEnd {
                    starting: 3,
                    count: 2,
                    total: 3,
                },
            })
        );

        let mut zero_start = base.clone();
        zero_start.fragment_starting_num = FragmentNumber::ZERO;
        assert_eq!(
            zero_start.validate(),
            Err(RtpsError::InvalidFragmentNumber {
                value: 0,
                context: "DATA_FRAG fragmentStartingNum",
            })
        );

        let mut zero_sn = base.clone();
        zero_sn.writer_sn = SequenceNumber::ZERO;
        assert!(zero_sn.validate().is_err());
    }

    #[test]
    fn a_data_frag_with_a_short_octets_to_inline_qos_is_refused() {
        let mut body = Vec::from([0x00_u8, 0x00, 0x10, 0x00]); // 16, not 28
        body.resize(32, 0);
        let header =
            SubmessageHeader::new(SubmessageId::DataFrag, SubmessageFlags::LITTLE_ENDIAN, 0);
        assert_eq!(
            DataFrag::read(&header, &body).map(|_| ()),
            Err(RtpsError::InvalidOctetsToInlineQos {
                id: 0x16,
                declared: 16,
                minimum: 28,
            })
        );
    }

    #[test]
    fn a_data_frag_carries_inline_qos_after_the_geometry() {
        let mut qos = ParameterList::new(Encoding::DISCOVERY);
        qos.push_octets(ParameterId::new(pid::KEY_HASH), &[9_u8; 16][..])
            .expect("short enough");
        let frag = DataFrag::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::new(4),
            FragmentGeometry::new(FragmentNumber::FIRST, 1, 8, 8),
            SerializedPayload::new(vec![3_u8; 8]),
        )
        .with_inline_qos(qos)
        .with_endianness(Endianness::Big);

        assert_eq!(frag.flags().raw(), flags::INLINE_QOS);
        assert_eq!(frag.body_len(), 32 + (4 + 16 + 4) + 8);
        assert_eq!(frag.validate(), Ok(()));

        let mut writer = CdrWriter::headerless(body_encoding(Endianness::Big));
        frag.write_body(&mut writer).expect("write");
        let body = writer.finish();
        let header = SubmessageHeader::new(SubmessageId::DataFrag, frag.flags(), 0);
        assert_eq!(
            DataFrag::read(&header, &body).expect("read").into_owned(),
            frag.clone().into_owned()
        );
    }
}
