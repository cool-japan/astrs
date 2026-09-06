//! `PL_CDR` parameter lists — the representation RTPS discovery is built
//! from.
//!
//! # Scope: this is the RTPS form, not XCDR2's mutable form
//!
//! Two different wire formats are called "parameter list", and this module
//! implements exactly one of them:
//!
//! - **OMG DDSI-RTPS 2.3 §9.4.2.11 `ParameterList`** — 16-bit
//!   `(parameterId, parameterLength)` entry headers, terminated by
//!   `PID_SENTINEL`. Its encapsulation identifiers are `PL_CDR_BE` (`0x0002`)
//!   and `PL_CDR_LE` (`0x0003`), it is what SPDP and SEDP samples and RTPS
//!   inline QoS are written in, and it is what this module handles.
//! - **OMG DDS-XTypes 1.3 `PL_CDR2`** (`0x000a` / `0x000b`) — the XCDR2
//!   encoding of a `@mutable` type. It has **no sentinel**: a DHEADER bounds
//!   the member list and every member is tagged with a 32-bit
//!   [`EmHeader`](crate::EmHeader). That format lives in [`crate::xcdr2`]
//!   and is read with [`CdrReader::read_member_header`]; handing one of its
//!   payloads to [`ParameterList::decode`] would mis-read it, so `decode`
//!   refuses the identifier.
//!
//! [`EncapsulationKind::is_rtps_parameter_list`] is the predicate that
//! separates them.
//!
//! # Wire format
//!
//! An RTPS parameter list body is a sequence of self-describing entries
//! terminated by a sentinel:
//!
//! ```text
//! +--------+--------+--------+--------+
//! |   parameterId   | parameterLength |
//! +--------+--------+--------+--------+
//! |                                   |
//! ~   value (parameterLength octets)  ~
//! |                                   |
//! +--------+--------+--------+--------+
//! ~                ...                ~
//! +--------+--------+--------+--------+
//! | PID_SENTINEL (1) |       0        |
//! +--------+--------+--------+--------+
//! ```
//!
//! Both header fields are `unsigned short` in the stream's byte order. Every
//! entry starts on a four-octet boundary, which is what makes each value a
//! CDR stream in its own right: the value begins at an offset that is a
//! multiple of four, so alignments up to four give the same answer whether
//! they are counted from the value or from the enclosing body.
//!
//! # What a value is, exactly
//!
//! [`Parameter::value`] is **the octets `parameterLength` declares** — no more,
//! no less. Encoding writes `parameterLength = value.len()` verbatim.
//!
//! RTPS requires `parameterLength` to be a multiple of four, so that the next
//! entry lands on a four-octet boundary. [`Parameter::new`] and
//! [`Parameter::encode_value`] therefore **pad the value on construction**:
//! a ten-octet `string` value becomes twelve octets, and the parameter
//! declares twelve. Nothing this crate builds is ever non-conformant.
//!
//! A *decoded* parameter keeps whatever the sender declared, padded or not,
//! so re-encoding reproduces the peer's octets exactly even when the peer was
//! wrong. Together the two rules make both round trips exact:
//! `decode(encode(list)) == list` and `encode(decode(bytes)) == bytes`.
//!
//! Because a value carries its padding, [`Parameter::decode_value`] finishes
//! with [`CdrReader::finish_tolerant`], forgiving up to three trailing
//! octets.
//!
//! # Unknown parameters survive
//!
//! Nothing here interprets a parameter id. An id this crate has never heard
//! of is carried through decode and re-encode unchanged, which is what lets
//! `astrs-rtps` forward a vendor's discovery data it does not model. The two
//! exceptions are [`pid::EXTENDED`] and [`pid::LIST_END`], whose long-form
//! layout this repository has no C/C++-free source for; they are refused
//! rather than guessed at.
//!
//! ```
//! use astrs_cdr::{Encoding, ParameterList, ParameterId, pid};
//!
//! let mut list = ParameterList::new(Encoding::DISCOVERY);
//! list.push_value(ParameterId::new(pid::TOPIC_NAME), &"rt/chatter".to_owned())?;
//! let bytes = list.encode()?;
//! assert_eq!(&bytes[..4], &[0x00, 0x03, 0x00, 0x00]); // PL_CDR_LE
//!
//! let (decoded, _) = ParameterList::decode(&bytes)?;
//! let topic: &str = decoded
//!     .get(ParameterId::new(pid::TOPIC_NAME))
//!     .ok_or(astrs_cdr::CdrError::MissingSentinel)?
//!     .decode_value(Encoding::DISCOVERY)?;
//! assert_eq!(topic, "rt/chatter");
//! # Ok::<(), astrs_cdr::CdrError>(())
//! ```

pub mod pid;

use std::borrow::Cow;

use crate::align::{ALIGN_4, align_up, padding_to};
use crate::encoding::{EncapsulationHeader, EncapsulationKind, Encoding};
use crate::error::{CdrError, CdrResult};
use crate::reader::CdrReader;
use crate::traits::{CdrDeserialize, CdrSerialize};
use crate::writer::CdrWriter;

/// `PID_PAD`, re-exported at the crate root for convenience.
pub const PID_PAD: u16 = pid::PAD;
/// `PID_SENTINEL`, re-exported at the crate root for convenience.
pub const PID_SENTINEL: u16 = pid::SENTINEL;

/// Octets the terminating sentinel entry occupies.
pub const SENTINEL_LEN: usize = 4;

/// Octets a parameter's `(parameterId, parameterLength)` header occupies.
pub const PARAMETER_HEADER_LEN: usize = 4;

/// The number of parameters [`ParameterList::decode`] accepts before it
/// treats the input as hostile.
///
/// Every entry costs at least four octets on the wire, so the count is
/// already bounded by the datagram size; this ceiling exists so that bound is
/// explicit rather than incidental. Real SPDP and SEDP samples carry a few
/// dozen.
pub const DEFAULT_MAX_PARAMETERS: usize = 16_384;

/// A parameter id, with the two flags RTPS puts in its top bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ParameterId(u16);

impl ParameterId {
    /// The sentinel that terminates a list.
    pub const SENTINEL: Self = Self(pid::SENTINEL);
    /// The ignorable filler entry.
    pub const PAD: Self = Self(pid::PAD);

    /// Wrap a raw id.
    #[must_use]
    pub const fn new(raw: u16) -> Self {
        Self(raw)
    }

    /// The raw sixteen-bit id, flags included.
    #[must_use]
    pub const fn raw(self) -> u16 {
        self.0
    }

    /// The id with both flags masked off.
    #[must_use]
    pub const fn base(self) -> u16 {
        self.0 & pid::BASE_MASK
    }

    /// True when the vendor-specific flag (bit 15) is set.
    #[must_use]
    pub const fn is_vendor_specific(self) -> bool {
        self.0 & pid::FLAG_VENDOR_SPECIFIC != 0
    }

    /// True when the must-understand flag (bit 14) is set: a receiver that
    /// does not recognise this parameter must discard the sample.
    #[must_use]
    pub const fn must_understand(self) -> bool {
        self.0 & pid::FLAG_MUST_UNDERSTAND != 0
    }

    /// The same id with the must-understand flag set.
    #[must_use]
    pub const fn with_must_understand(self) -> Self {
        Self(self.0 | pid::FLAG_MUST_UNDERSTAND)
    }

    /// True for [`ParameterId::SENTINEL`].
    #[must_use]
    pub const fn is_sentinel(self) -> bool {
        self.0 == pid::SENTINEL
    }

    /// True for [`ParameterId::PAD`].
    #[must_use]
    pub const fn is_pad(self) -> bool {
        self.0 == pid::PAD
    }

    /// True for the long-form escapes this crate refuses.
    #[must_use]
    pub const fn is_unsupported(self) -> bool {
        self.0 == pid::EXTENDED || self.0 == pid::LIST_END
    }

    /// The standard name of this id, when it has one.
    #[must_use]
    pub fn name(self) -> Option<&'static str> {
        pid::name(self.0)
    }
}

impl From<u16> for ParameterId {
    fn from(raw: u16) -> Self {
        Self(raw)
    }
}

/// One entry of a parameter list.
///
/// `value` borrows the input buffer after a decode and owns its octets after
/// a [`Parameter::encode_value`], which is what lets `astrs-rtps` re-emit a
/// received discovery sample without copying the parameters it did not
/// change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parameter<'a> {
    /// The parameter id.
    pub id: ParameterId,
    /// Exactly `parameterLength` octets, trailing padding included. See the
    /// [module documentation](self) for the value model.
    pub value: Cow<'a, [u8]>,
}

impl<'a> Parameter<'a> {
    /// Build a parameter from raw octets, padding the value up to the
    /// four-octet multiple `parameterLength` must be.
    ///
    /// A borrowed value whose length is already a multiple of four stays
    /// borrowed; one that needs padding is copied, because there is nowhere
    /// else to put the extra octets.
    ///
    /// # Errors
    ///
    /// [`CdrError::ParameterTooLong`] when the padded value does not fit the
    /// sixteen-bit `parameterLength` field.
    pub fn new(id: ParameterId, value: impl Into<Cow<'a, [u8]>>) -> CdrResult<Self> {
        let mut value = value.into();
        let padded = align_up(value.len(), ALIGN_4).unwrap_or(usize::MAX);
        if u16::try_from(padded).is_err() {
            return Err(CdrError::ParameterTooLong {
                id: id.raw(),
                length: padded,
            });
        }
        if padded != value.len() {
            let mut owned = value.into_owned();
            owned.resize(padded, 0);
            value = Cow::Owned(owned);
        }
        Ok(Self { id, value })
    }

    /// Build a parameter from octets that are taken verbatim, padded or not.
    ///
    /// The escape hatch for reproducing a non-conformant sender's bytes.
    /// [`Parameter::new`] is what a caller building a sample should use.
    ///
    /// # Errors
    ///
    /// [`CdrError::ParameterTooLong`] when the value does not fit the
    /// sixteen-bit `parameterLength` field.
    pub fn verbatim(id: ParameterId, value: impl Into<Cow<'a, [u8]>>) -> CdrResult<Self> {
        let value = value.into();
        if u16::try_from(value.len()).is_err() {
            return Err(CdrError::ParameterTooLong {
                id: id.raw(),
                length: value.len(),
            });
        }
        Ok(Self { id, value })
    }

    /// Serialize `value` with plain CDR rules and wrap it as a parameter.
    ///
    /// The value is written with a fresh alignment origin and no
    /// encapsulation header of its own, which is what a `PL_CDR` entry holds.
    ///
    /// # Errors
    ///
    /// Whatever `value`'s [`CdrSerialize`] impl returns, or
    /// [`CdrError::ParameterTooLong`].
    pub fn encode_value<T: CdrSerialize + ?Sized>(
        id: ParameterId,
        value: &T,
        encoding: Encoding,
    ) -> CdrResult<Parameter<'static>> {
        let mut writer = CdrWriter::headerless(encoding.plain());
        writer.serialize(value)?;
        Parameter::new(id, Cow::Owned(writer.finish()))
    }

    /// Decode this parameter's value as `T`.
    ///
    /// Up to three trailing octets are forgiven, because a peer may declare
    /// the padded length rather than the exact one.
    ///
    /// # Errors
    ///
    /// Whatever `T`'s impl returns, plus [`CdrError::TrailingBytes`] when
    /// more than three octets are left over.
    pub fn decode_value<'de, T>(&'de self, encoding: Encoding) -> CdrResult<T>
    where
        T: CdrDeserialize<'de>,
    {
        let mut reader = CdrReader::with_encoding(&self.value, encoding.plain());
        let decoded = reader.deserialize()?;
        reader.finish_tolerant()?;
        Ok(decoded)
    }

    /// The declared `parameterLength`.
    #[must_use]
    pub fn declared_len(&self) -> usize {
        self.value.len()
    }

    /// Octets this entry occupies on the wire: the four-octet header plus the
    /// value rounded up to a four-octet boundary.
    #[must_use]
    pub fn serialized_len(&self) -> usize {
        PARAMETER_HEADER_LEN + align_up(self.value.len(), ALIGN_4).unwrap_or(usize::MAX)
    }

    /// Detach from the input buffer, cloning the value if it is borrowed.
    #[must_use]
    pub fn into_owned(self) -> Parameter<'static> {
        Parameter {
            id: self.id,
            value: Cow::Owned(self.value.into_owned()),
        }
    }
}

/// A `PID_SENTINEL`-terminated list of parameters.
///
/// See the [module documentation](self) for the wire format and the value
/// model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterList<'a> {
    parameters: Vec<Parameter<'a>>,
    encoding: Encoding,
}

impl<'a> ParameterList<'a> {
    /// An empty list that will be written with `encoding`.
    ///
    /// `encoding` should name `PL_CDR_LE` or `PL_CDR_BE`; the kind fixes only
    /// the byte order of the entry headers and the identifier
    /// [`ParameterList::encode`] emits, so a plain kind is accepted for a
    /// caller building a body that will be framed some other way.
    #[must_use]
    pub const fn new(encoding: Encoding) -> Self {
        Self {
            parameters: Vec::new(),
            encoding,
        }
    }

    /// The encoding this list was decoded with, or will be encoded with.
    #[must_use]
    pub const fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// Append a parameter.
    pub fn push(&mut self, parameter: Parameter<'a>) {
        self.parameters.push(parameter);
    }

    /// Serialize `value` and append it as a parameter.
    ///
    /// # Errors
    ///
    /// Whatever `value`'s [`CdrSerialize`] impl returns, or
    /// [`CdrError::ParameterTooLong`].
    pub fn push_value<T: CdrSerialize + ?Sized>(
        &mut self,
        id: ParameterId,
        value: &T,
    ) -> CdrResult<()> {
        let parameter = Parameter::encode_value(id, value, self.encoding)?;
        self.parameters.push(parameter);
        Ok(())
    }

    /// Append raw octets as a parameter.
    ///
    /// # Errors
    ///
    /// [`CdrError::ParameterTooLong`].
    pub fn push_octets(
        &mut self,
        id: ParameterId,
        value: impl Into<Cow<'a, [u8]>>,
    ) -> CdrResult<()> {
        self.parameters.push(Parameter::new(id, value)?);
        Ok(())
    }

    /// The first parameter whose id matches exactly.
    #[must_use]
    pub fn get(&self, id: ParameterId) -> Option<&Parameter<'a>> {
        self.parameters.iter().find(|entry| entry.id == id)
    }

    /// The first parameter whose id matches once both flags are masked off.
    ///
    /// Senders differ on whether they set the must-understand flag on a
    /// standard parameter, so a lookup that must succeed either way uses
    /// this.
    #[must_use]
    pub fn get_by_base(&self, id: u16) -> Option<&Parameter<'a>> {
        let base = id & pid::BASE_MASK;
        self.parameters.iter().find(|entry| entry.id.base() == base)
    }

    /// Every parameter with the given base id, in wire order.
    ///
    /// Locator parameters repeat, so discovery reads them all.
    pub fn all_by_base(&self, id: u16) -> impl Iterator<Item = &Parameter<'a>> {
        let base = id & pid::BASE_MASK;
        self.parameters
            .iter()
            .filter(move |entry| entry.id.base() == base)
    }

    /// Iterate over the parameters in wire order.
    pub fn iter(&self) -> core::slice::Iter<'_, Parameter<'a>> {
        self.parameters.iter()
    }

    /// The parameters as a slice.
    #[must_use]
    pub fn as_slice(&self) -> &[Parameter<'a>] {
        &self.parameters
    }

    /// Number of parameters, sentinel excluded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.parameters.len()
    }

    /// True when the list holds no parameters.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.parameters.is_empty()
    }

    /// Octets the body occupies: every entry padded to four, plus the
    /// sentinel.
    #[must_use]
    pub fn serialized_len(&self) -> usize {
        self.parameters
            .iter()
            .map(Parameter::serialized_len)
            .sum::<usize>()
            + SENTINEL_LEN
    }

    /// Detach every parameter from the input buffer.
    #[must_use]
    pub fn into_owned(self) -> ParameterList<'static> {
        ParameterList {
            parameters: self
                .parameters
                .into_iter()
                .map(Parameter::into_owned)
                .collect(),
            encoding: self.encoding,
        }
    }

    /// Write the body — entries and sentinel, no encapsulation header —
    /// through `writer`.
    ///
    /// # Errors
    ///
    /// [`CdrError::ParameterTooLong`] when an entry's padded length does not
    /// fit the sixteen-bit field.
    pub fn write(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        for parameter in &self.parameters {
            writer.align(ALIGN_4);
            let declared = parameter.value.len();
            let length = u16::try_from(declared).map_err(|_| CdrError::ParameterTooLong {
                id: parameter.id.raw(),
                length: declared,
            })?;
            writer.write_u16(parameter.id.raw())?;
            writer.write_u16(length)?;
            writer.write_octets(&parameter.value);
            writer.write_zeros(padding_to(declared, ALIGN_4));
        }
        writer.align(ALIGN_4);
        writer.write_u16(pid::SENTINEL)?;
        writer.write_u16(0)?;
        Ok(())
    }

    /// Encode the whole sample: encapsulation header, entries, sentinel.
    ///
    /// # Errors
    ///
    /// Those of [`ParameterList::write`].
    pub fn encode(&self) -> CdrResult<Vec<u8>> {
        let mut writer =
            CdrWriter::with_capacity(self.encoding, self.serialized_len() + SENTINEL_LEN);
        self.write(&mut writer)?;
        Ok(writer.finish())
    }

    /// Read a body — entries and sentinel — from `reader`.
    ///
    /// # Errors
    ///
    /// - [`CdrError::MissingSentinel`] when the body ends without one.
    /// - [`CdrError::UnsupportedParameter`] for `PID_EXTENDED` and
    ///   `PID_LIST_END`.
    /// - [`CdrError::SequenceTooLong`] when the list exceeds
    ///   [`DEFAULT_MAX_PARAMETERS`] entries.
    pub fn read(reader: &mut CdrReader<'a>) -> CdrResult<Self> {
        Self::read_with_limit(reader, DEFAULT_MAX_PARAMETERS)
    }

    /// [`ParameterList::read`] with an explicit ceiling on the entry count.
    ///
    /// # Errors
    ///
    /// Those of [`ParameterList::read`].
    pub fn read_with_limit(reader: &mut CdrReader<'a>, limit: usize) -> CdrResult<Self> {
        let encoding = reader.encoding();
        let mut parameters = Vec::new();
        loop {
            // Entries are four-octet aligned. Check that the padding *and*
            // the next entry header both fit before consuming anything, so a
            // body that simply stops reports the missing sentinel rather than
            // a truncated pad run.
            let padding = padding_to(reader.position(), ALIGN_4);
            if reader.remaining() < padding + PARAMETER_HEADER_LEN {
                return Err(CdrError::MissingSentinel);
            }
            reader.align(ALIGN_4)?;
            let id = ParameterId::new(reader.read_u16()?);
            let declared = usize::from(reader.read_u16()?);
            if id.is_sentinel() {
                break;
            }
            if id.is_unsupported() {
                return Err(CdrError::UnsupportedParameter { id: id.raw() });
            }
            let available = reader.remaining();
            if declared > available {
                return Err(CdrError::LengthOverflow {
                    declared: declared as u64,
                    available,
                    element_size: 1,
                    context: "parameter value",
                });
            }
            let value = reader.read_octets(declared)?;
            if id.is_pad() {
                // A filler entry carries no information; step over it.
                continue;
            }
            if parameters.len() == limit {
                return Err(CdrError::SequenceTooLong {
                    length: limit as u64 + 1,
                    maximum: limit as u64,
                });
            }
            parameters.push(Parameter {
                id,
                value: Cow::Borrowed(value),
            });
        }
        Ok(Self {
            parameters,
            encoding,
        })
    }

    /// Decode a whole sample: encapsulation header, entries, sentinel.
    ///
    /// Returns the list together with the encoding its header selected, so a
    /// caller that must re-emit the sample in the peer's byte order does not
    /// have to parse the header twice.
    ///
    /// # Errors
    ///
    /// Those of [`CdrReader::new`] and [`ParameterList::read`], plus
    /// [`CdrError::UnsupportedEncapsulation`] when the header does not name
    /// `PL_CDR_BE` or `PL_CDR_LE`.
    ///
    /// `PL_CDR2_BE` and `PL_CDR2_LE` are refused here on purpose: XCDR2's
    /// mutable encoding has no `PID_SENTINEL` and tags members with an
    /// [`EmHeader`](crate::EmHeader), so a sentinel parser would mis-read it.
    /// See the [module documentation](self).
    pub fn decode(data: &'a [u8]) -> CdrResult<(Self, Encoding)> {
        let header = EncapsulationHeader::from_bytes(data)?;
        if !header.kind.is_rtps_parameter_list() {
            return Err(CdrError::UnsupportedEncapsulation(
                "an RTPS ParameterList needs a PL_CDR_BE or PL_CDR_LE encapsulation",
            ));
        }
        let mut reader = CdrReader::new(data)?;
        let list = Self::read(&mut reader)?;
        let encoding = list.encoding;
        Ok((list, encoding))
    }

    /// [`ParameterList::decode`] without insisting the encapsulation kind be
    /// an RTPS parameter-list one.
    ///
    /// A few stacks label a discovery payload `CDR_LE` by mistake, and a
    /// caller that has already established out of band that a payload is a
    /// sentinel-terminated list may not want the check. This is the lenient
    /// door for both — and, unlike [`ParameterList::decode`], it will happily
    /// mis-read a genuine `PL_CDR2` payload, so use it knowingly.
    ///
    /// # Errors
    ///
    /// Those of [`CdrReader::new`] and [`ParameterList::read`].
    pub fn decode_any(data: &'a [u8]) -> CdrResult<(Self, Encoding)> {
        let mut reader = CdrReader::new(data)?;
        let list = Self::read(&mut reader)?;
        let encoding = list.encoding;
        Ok((list, encoding))
    }

    /// Decode a headerless parameter list, such as RTPS inline QoS.
    ///
    /// # Errors
    ///
    /// Those of [`ParameterList::read`].
    pub fn decode_headerless(data: &'a [u8], encoding: Encoding) -> CdrResult<Self> {
        let mut reader = CdrReader::with_encoding(data, encoding);
        Self::read(&mut reader)
    }
}

impl<'a> IntoIterator for ParameterList<'a> {
    type Item = Parameter<'a>;
    type IntoIter = std::vec::IntoIter<Parameter<'a>>;

    fn into_iter(self) -> Self::IntoIter {
        self.parameters.into_iter()
    }
}

impl<'a, 'b> IntoIterator for &'b ParameterList<'a> {
    type Item = &'b Parameter<'a>;
    type IntoIter = core::slice::Iter<'b, Parameter<'a>>;

    fn into_iter(self) -> Self::IntoIter {
        self.parameters.iter()
    }
}

impl Default for ParameterList<'_> {
    /// An empty list in the `PL_CDR_LE` encoding RTPS discovery uses.
    fn default() -> Self {
        Self::new(Encoding::new(EncapsulationKind::PlCdrLe))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn an_empty_list_is_a_header_and_a_sentinel() {
        let list = ParameterList::default();
        assert!(list.is_empty());
        assert_eq!(list.serialized_len(), 4);
        assert_eq!(
            list.encode().expect("encode"),
            [0x00, 0x03, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn an_entry_is_id_length_and_a_four_octet_multiple_value() {
        let mut list = ParameterList::new(Encoding::DISCOVERY);
        list.push_octets(ParameterId::new(pid::KEY_HASH), &[0xaa_u8, 0xbb][..])
            .expect("short enough");
        let bytes = list.encode().expect("encode");
        assert_eq!(
            bytes,
            [
                0x00, 0x03, 0x00, 0x00, // PL_CDR_LE
                0x70, 0x00, // id = 0x0070
                0x04, 0x00, // length = 4: the two octets, padded
                0xaa, 0xbb, 0x00, 0x00, // value, padding included
                0x01, 0x00, 0x00, 0x00, // PID_SENTINEL
            ]
        );
        assert_eq!(list.serialized_len(), bytes.len() - 4);
    }

    #[test]
    fn a_verbatim_parameter_reproduces_a_non_conformant_sender() {
        // RTPS says parameterLength is a multiple of four; a sender that
        // declared 2 is wrong, and re-encoding must still reproduce it.
        let mut list = ParameterList::new(Encoding::DISCOVERY);
        list.push(
            Parameter::verbatim(ParameterId::new(pid::KEY_HASH), &[0xaa_u8, 0xbb][..])
                .expect("short enough"),
        );
        let bytes = list.encode().expect("encode");
        assert_eq!(
            bytes,
            [
                0x00, 0x03, 0x00, 0x00, 0x70, 0x00, 0x02, 0x00, 0xaa, 0xbb, 0x00, 0x00, 0x01, 0x00,
                0x00, 0x00,
            ]
        );
        let (decoded, _) = ParameterList::decode(&bytes).expect("decode");
        assert_eq!(decoded, list);
        assert_eq!(decoded.encode().expect("re-encode"), bytes);
    }

    #[test]
    fn big_endian_lists_swap_the_header_fields_too() {
        let mut list = ParameterList::new(Encoding::new(EncapsulationKind::PlCdrBe));
        list.push_octets(ParameterId::new(pid::KEY_HASH), &[0xaa_u8, 0xbb][..])
            .expect("short enough");
        assert_eq!(
            list.encode().expect("encode"),
            [
                0x00, 0x02, 0x00, 0x00, // PL_CDR_BE
                0x00, 0x70, // id, big-endian
                0x00, 0x04, // length, big-endian
                0xaa, 0xbb, 0x00, 0x00, // value, padding included
                0x00, 0x01, 0x00, 0x00, // PID_SENTINEL
            ]
        );
    }

    #[test]
    fn decode_is_the_exact_inverse_of_encode() {
        let mut list = ParameterList::new(Encoding::DISCOVERY);
        list.push_value(ParameterId::new(pid::TOPIC_NAME), &"rt/scan".to_owned())
            .expect("encode value");
        list.push_octets(ParameterId::new(pid::PARTICIPANT_GUID), &[1_u8; 16][..])
            .expect("short enough");
        let bytes = list.encode().expect("encode");

        let (decoded, encoding) = ParameterList::decode(&bytes).expect("decode");
        assert_eq!(encoding.kind(), EncapsulationKind::PlCdrLe);
        assert_eq!(decoded, list);
        assert_eq!(decoded.encode().expect("re-encode"), bytes);
    }

    #[test]
    fn typed_values_round_trip_through_a_parameter() {
        let parameter = Parameter::encode_value(
            ParameterId::new(pid::ENTITY_NAME),
            &"laser".to_owned(),
            Encoding::DISCOVERY,
        )
        .expect("encode");
        // Four octets of length plus "laser\0" is ten, padded to twelve so
        // parameterLength is the multiple of four RTPS requires.
        assert_eq!(parameter.declared_len(), 12);
        assert_eq!(parameter.serialized_len(), 4 + 12);
        let text: &str = parameter.decode_value(Encoding::DISCOVERY).expect("decode");
        assert_eq!(text, "laser");
    }

    #[test]
    fn a_padded_declared_length_is_tolerated_when_decoding_a_value() {
        // A peer that declares the padded length: 12 octets for a 10-octet
        // string.
        let mut value = Vec::from([0x06, 0x00, 0x00, 0x00]);
        value.extend_from_slice(b"laser\0");
        value.extend_from_slice(&[0x00, 0x00]);
        let parameter =
            Parameter::verbatim(ParameterId::new(pid::ENTITY_NAME), value).expect("short enough");
        let text: &str = parameter
            .decode_value(Encoding::DISCOVERY)
            .expect("tolerant decode");
        assert_eq!(text, "laser");
    }

    #[test]
    fn pad_entries_are_skipped_and_unknown_ids_survive() {
        let bytes = [
            0x00, 0x03, 0x00, 0x00, // PL_CDR_LE
            0x00, 0x00, 0x04, 0x00, // PID_PAD, length 4
            0xde, 0xad, 0xbe, 0xef, // …its ignorable value
            0x99, 0x12, 0x04, 0x00, // an id nothing here models
            0x01, 0x02, 0x03, 0x04, //
            0x01, 0x00, 0x00, 0x00, // PID_SENTINEL
        ];
        let (list, _) = ParameterList::decode(&bytes).expect("decode");
        assert_eq!(list.len(), 1);
        let entry = list.as_slice().first().expect("one entry");
        assert_eq!(entry.id.raw(), 0x1299);
        assert_eq!(entry.value.as_ref(), &[0x01, 0x02, 0x03, 0x04]);
        // Re-encoding drops the PID_PAD entry, which carries no information.
        assert_eq!(
            list.encode().expect("re-encode"),
            [
                0x00, 0x03, 0x00, 0x00, 0x99, 0x12, 0x04, 0x00, 0x01, 0x02, 0x03, 0x04, 0x01, 0x00,
                0x00, 0x00,
            ]
        );
    }

    #[test]
    fn a_list_without_a_sentinel_is_refused() {
        let bytes = [
            0x00, 0x03, 0x00, 0x00, 0x70, 0x00, 0x04, 0x00, 0x01, 0x02, 0x03, 0x04,
        ];
        assert_eq!(
            ParameterList::decode(&bytes).map(|_| ()),
            Err(CdrError::MissingSentinel)
        );
    }

    #[test]
    fn a_length_past_the_end_of_the_buffer_is_refused() {
        let bytes = [0x00, 0x03, 0x00, 0x00, 0x70, 0x00, 0xff, 0x00, 0x01, 0x02];
        assert_eq!(
            ParameterList::decode(&bytes).map(|_| ()),
            Err(CdrError::LengthOverflow {
                declared: 255,
                available: 2,
                element_size: 1,
                context: "parameter value",
            })
        );
    }

    #[test]
    fn the_extended_escape_is_refused_rather_than_guessed_at() {
        let bytes = [
            0x00, 0x03, 0x00, 0x00, 0x01, 0x3f, 0x08, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0x00,
            0x00, 0x00,
        ];
        assert_eq!(
            ParameterList::decode(&bytes).map(|_| ()),
            Err(CdrError::UnsupportedParameter { id: 0x3f01 })
        );
    }

    #[test]
    fn a_non_parameter_list_encapsulation_is_refused_by_decode() {
        let bytes = [0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00];
        assert_eq!(
            ParameterList::decode(&bytes).map(|_| ()),
            Err(CdrError::UnsupportedEncapsulation(
                "an RTPS ParameterList needs a PL_CDR_BE or PL_CDR_LE encapsulation"
            ))
        );
        // …and accepted by the lenient door.
        let (list, encoding) = ParameterList::decode_any(&bytes).expect("lenient");
        assert!(list.is_empty());
        assert_eq!(encoding.kind(), EncapsulationKind::CdrLe);
    }

    #[test]
    fn pl_cdr2_is_refused_because_it_is_a_different_format() {
        // Identifier 0x000b is XCDR2's mutable encoding: a DHEADER and
        // EMHEADER-tagged members, with no PID_SENTINEL anywhere. Parsing it
        // as a sentinel-terminated list would silently produce nonsense, so
        // the strict door refuses the identifier outright.
        let bytes = [0x00, 0x0b, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00];
        assert_eq!(
            ParameterList::decode(&bytes).map(|_| ()),
            Err(CdrError::UnsupportedEncapsulation(
                "an RTPS ParameterList needs a PL_CDR_BE or PL_CDR_LE encapsulation"
            ))
        );
        assert!(EncapsulationKind::PlCdr2Le.is_parameter_list());
        assert!(!EncapsulationKind::PlCdr2Le.is_rtps_parameter_list());
    }

    #[test]
    fn headerless_lists_are_how_inline_qos_arrives() {
        let bytes = [
            0x71, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x00,
        ];
        let list = ParameterList::decode_headerless(&bytes, Encoding::DISCOVERY).expect("decode");
        assert_eq!(list.len(), 1);
        assert_eq!(
            list.get_by_base(pid::STATUS_INFO)
                .expect("status info")
                .value
                .as_ref(),
            &[0x00, 0x00, 0x00, 0x03]
        );
    }

    #[test]
    fn lookups_can_ignore_the_flags() {
        let mut list = ParameterList::new(Encoding::DISCOVERY);
        let flagged = ParameterId::new(pid::TOPIC_NAME).with_must_understand();
        assert!(flagged.must_understand());
        assert_eq!(flagged.base(), pid::TOPIC_NAME);
        assert_eq!(flagged.name(), Some("PID_TOPIC_NAME"));
        list.push_value(flagged, &"rt/x".to_owned())
            .expect("encode value");

        assert!(list.get(ParameterId::new(pid::TOPIC_NAME)).is_none());
        assert!(list.get(flagged).is_some());
        assert!(list.get_by_base(pid::TOPIC_NAME).is_some());
    }

    #[test]
    fn repeated_parameters_are_all_reachable() {
        let mut list = ParameterList::new(Encoding::DISCOVERY);
        for octet in 0..3_u8 {
            list.push_octets(
                ParameterId::new(pid::METATRAFFIC_UNICAST_LOCATOR),
                vec![octet; 4],
            )
            .expect("short enough");
        }
        assert_eq!(
            list.all_by_base(pid::METATRAFFIC_UNICAST_LOCATOR).count(),
            3
        );
        assert_eq!(list.iter().count(), 3);
        assert_eq!((&list).into_iter().count(), 3);
        assert_eq!(list.clone().into_iter().count(), 3);
    }

    #[test]
    fn a_value_too_long_for_the_length_field_is_refused() {
        let huge = vec![0_u8; 65_536];
        assert_eq!(
            Parameter::new(ParameterId::new(pid::USER_DATA), huge).map(|_| ()),
            Err(CdrError::ParameterTooLong {
                id: pid::USER_DATA,
                length: 65_536,
            })
        );
    }

    #[test]
    fn the_entry_ceiling_bounds_a_crafted_list() {
        // Two entries with a limit of one.
        let bytes = [
            0x00, 0x03, 0x00, 0x00, 0x70, 0x00, 0x00, 0x00, 0x71, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x00, 0x00,
        ];
        let mut reader = CdrReader::new(&bytes).expect("header");
        assert_eq!(
            ParameterList::read_with_limit(&mut reader, 1).map(|_| ()),
            Err(CdrError::SequenceTooLong {
                length: 2,
                maximum: 1,
            })
        );
    }

    #[test]
    fn borrowed_parameters_can_be_detached() {
        let bytes = [
            0x00, 0x03, 0x00, 0x00, 0x70, 0x00, 0x04, 0x00, 1, 2, 3, 4, 0x01, 0x00, 0x00, 0x00,
        ];
        let owned = {
            let (list, _) = ParameterList::decode(&bytes).expect("decode");
            list.into_owned()
        };
        assert_eq!(
            owned.get_by_base(pid::KEY_HASH).expect("entry").value,
            Cow::<[u8]>::Owned(vec![1, 2, 3, 4])
        );
        assert_eq!(owned.encoding().kind(), EncapsulationKind::PlCdrLe);
    }

    #[test]
    fn parameter_ids_expose_their_flags() {
        assert!(ParameterId::SENTINEL.is_sentinel());
        assert!(ParameterId::PAD.is_pad());
        assert!(ParameterId::new(pid::EXTENDED).is_unsupported());
        assert!(ParameterId::new(pid::LIST_END).is_unsupported());
        assert!(ParameterId::new(0x8001).is_vendor_specific());
        assert!(!ParameterId::new(0x0001).is_vendor_specific());
        assert_eq!(ParameterId::from(0x1234_u16).raw(), 0x1234);
        assert_eq!(PID_PAD, 0);
        assert_eq!(PID_SENTINEL, 1);
    }
}
