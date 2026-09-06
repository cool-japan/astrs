//! `SerializedPayload`: the opaque octets a `DATA` or `DATA_FRAG` carries.
//!
//! RTPS does not know what a sample *is* (OMG DDSI-RTPS 2.3 §10). It carries
//! a `SerializedPayload` — an octet sequence whose first four octets are the
//! CDR encapsulation header that says how to read the rest:
//!
//! ```text
//! +--------+--------+--------+--------+
//! | representation  |    options      |
//! |   identifier    |                 |
//! +--------+--------+--------+--------+
//! ~              body                 ~
//! +--------+--------+--------+--------+
//! ```
//!
//! The identifier is `CDR_LE` (`0x0001`) for a ROS 2 topic sample and
//! `PL_CDR_LE` (`0x0003`) for an SPDP or SEDP announcement, and it is read
//! big-endian regardless of the submessage's own byte order — the
//! encapsulation is self-describing precisely so it does not depend on the
//! frame around it.
//!
//! This type is deliberately thin: it holds the octets, tells you what
//! `astrs-cdr` would make of them, and hands them to `astrs-cdr` when asked.
//! Interpreting a payload is the business of `astrs-idl`-generated types and
//! of the discovery half.
//!
//! ```
//! use astrs_cdr::{EncapsulationKind, Encoding, ParameterList, ParameterId, pid};
//! use astrs_rtps::messages::SerializedPayload;
//!
//! let mut announcement = ParameterList::new(Encoding::DISCOVERY);
//! announcement.push_value(ParameterId::new(pid::TOPIC_NAME), &"rt/chatter".to_owned())?;
//! let payload = SerializedPayload::from_parameter_list(&announcement)?;
//!
//! assert_eq!(payload.encapsulation(), Some(EncapsulationKind::PlCdrLe));
//! assert!(payload.is_parameter_list());
//! let (decoded, _) = payload.parameter_list()?;
//! assert_eq!(decoded.len(), 1);
//! # Ok::<(), astrs_rtps::RtpsError>(())
//! ```

use core::fmt;
use std::borrow::Cow;

use astrs_cdr::{
    CdrReader, CdrSerialize, CdrWriter, ENCAPSULATION_HEADER_LEN, EncapsulationHeader,
    EncapsulationKind, Encoding, Endianness, ParameterList,
};

use crate::error::RtpsResult;

/// The encoding an **inline QoS** parameter list carries.
///
/// Inline QoS is a parameter list with no encapsulation header of its own: it
/// sits inside a `DATA` or `DATA_FRAG` body and takes the submessage's byte
/// order (§8.3.7.2.2). There is therefore no `PL_CDR_LE` identifier on the
/// wire to read the encoding from — but the list *is* an RTPS parameter list,
/// so that is what it is labelled with here, and a list decoded from a
/// submessage compares equal to one a caller built with
/// [`Encoding::DISCOVERY`].
///
/// # Examples
///
/// ```
/// use astrs_cdr::{EncapsulationKind, Encoding, Endianness};
/// use astrs_rtps::messages::inline_qos_encoding;
///
/// assert_eq!(inline_qos_encoding(Endianness::Little), Encoding::DISCOVERY);
/// assert_eq!(
///     inline_qos_encoding(Endianness::Big).kind(),
///     EncapsulationKind::PlCdrBe,
/// );
/// ```
#[must_use]
pub const fn inline_qos_encoding(endianness: Endianness) -> Encoding {
    match endianness {
        Endianness::Little => Encoding::new(EncapsulationKind::PlCdrLe),
        Endianness::Big => Encoding::new(EncapsulationKind::PlCdrBe),
    }
}

/// Relabel a parameter list with the encoding [`inline_qos_encoding`] gives
/// for `endianness`, keeping every parameter unchanged.
///
/// The label decides only two things: the byte order
/// [`ParameterList::push_value`] serializes a new value in, and the
/// identifier [`ParameterList::encode`] would emit. Inside a submessage
/// neither is on the wire — the `E` flag is — so keeping the two in step is
/// what makes a list a caller built and a list decoded from a submessage
/// compare equal.
#[must_use]
pub fn relabel_inline_qos(list: ParameterList<'_>, endianness: Endianness) -> ParameterList<'_> {
    if list.encoding() == inline_qos_encoding(endianness) {
        return list;
    }
    let mut relabelled = ParameterList::new(inline_qos_encoding(endianness));
    for parameter in list {
        relabelled.push(parameter);
    }
    relabelled
}

/// Read an inline-QoS parameter list from a submessage body.
///
/// [`ParameterList::read`] labels what it returns with the *reader's*
/// encoding, which for a submessage body is the plain `CDR_LE` or `CDR_BE`
/// the `E` flag selected. This relabels it as the parameter list it is, so
/// that decode and encode are inverses and the behavior half can
/// [`push_value`](ParameterList::push_value) onto a list it received.
///
/// # Errors
///
/// Those of [`ParameterList::read`].
pub fn read_inline_qos<'de>(
    reader: &mut CdrReader<'de>,
    endianness: Endianness,
) -> RtpsResult<ParameterList<'de>> {
    Ok(relabel_inline_qos(ParameterList::read(reader)?, endianness))
}

/// The octets a `DATA` or `DATA_FRAG` carries, encapsulation header included.
///
/// Borrowed after a decode, owned after a build, exactly like
/// `astrs-cdr`'s [`Parameter`](astrs_cdr::Parameter) — which is what lets a
/// received sample be forwarded without copying it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct SerializedPayload<'a> {
    octets: Cow<'a, [u8]>,
}

impl<'a> SerializedPayload<'a> {
    /// An empty payload.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            octets: Cow::Borrowed(&[]),
        }
    }

    /// Wrap octets that already carry an encapsulation header.
    #[must_use]
    pub fn new(octets: impl Into<Cow<'a, [u8]>>) -> Self {
        Self {
            octets: octets.into(),
        }
    }

    /// Serialize `value` with a `CDR_LE` encapsulation header — the ROS 2
    /// representation for user topic data.
    ///
    /// # Errors
    ///
    /// Whatever `value`'s [`CdrSerialize`] impl returns.
    pub fn from_cdr<T: CdrSerialize + ?Sized>(value: &T) -> RtpsResult<SerializedPayload<'static>> {
        Self::from_cdr_with(value, Encoding::ROS2)
    }

    /// Serialize `value` with a stated encoding.
    ///
    /// The result is **padded to a four-octet multiple**, because §8.3.3 puts
    /// every submessage on a four-octet boundary: a `DATA` whose payload is
    /// not a multiple of four cannot be followed by another submessage at all
    /// (see [`Message::encode`](crate::messages::Message::encode)).
    ///
    /// Where the padding goes is worth knowing. Under XCDR2 the pad count
    /// lands in the two low bits of the encapsulation `options` word, and a
    /// reader recovers the exact body length. Under **XCDR1** — `CDR_LE`, the
    /// ROS 2 default — the options word has no such field, so the padding is
    /// simply appended and a strict decoder would call it trailing data. That
    /// is why [`SerializedPayload::decode`] forgives up to three trailing
    /// octets, and why [`SerializedPayload::decode_exact`] exists for a
    /// caller that wants the strict reading instead.
    ///
    /// # Errors
    ///
    /// Whatever `value`'s [`CdrSerialize`] impl returns.
    pub fn from_cdr_with<T: CdrSerialize + ?Sized>(
        value: &T,
        encoding: Encoding,
    ) -> RtpsResult<SerializedPayload<'static>> {
        let mut writer = CdrWriter::new(encoding);
        writer.serialize(value)?;
        Ok(SerializedPayload {
            octets: Cow::Owned(writer.finish_padded()),
        })
    }

    /// True when the payload's length is a multiple of four.
    ///
    /// A submessage whose body is not four-octet aligned can only be the last
    /// one in a message (§8.3.3), and the payload is the only part of a
    /// `DATA` or `DATA_FRAG` body that can break the alignment.
    #[must_use]
    pub fn is_aligned(&self) -> bool {
        self.octets.len().is_multiple_of(4)
    }

    /// Encode a discovery parameter list, header included.
    ///
    /// # Errors
    ///
    /// Those of [`ParameterList::encode`].
    pub fn from_parameter_list(list: &ParameterList<'_>) -> RtpsResult<SerializedPayload<'static>> {
        Ok(SerializedPayload {
            octets: Cow::Owned(list.encode()?),
        })
    }

    /// The octets, encapsulation header included.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.octets
    }

    /// How many octets the payload occupies inside the submessage.
    #[must_use]
    pub fn len(&self) -> usize {
        self.octets.len()
    }

    /// True when the payload carries no octets at all.
    ///
    /// Distinct from "no payload": a `DATA` with neither the `D` nor the `K`
    /// flag has [`DataPayload::None`](crate::messages::DataPayload::None),
    /// not an empty `SerializedPayload`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.octets.is_empty()
    }

    /// The encapsulation identifier, when the payload is long enough to have
    /// one and it names an encoding `astrs-cdr` knows.
    #[must_use]
    pub fn encapsulation(&self) -> Option<EncapsulationKind> {
        EncapsulationHeader::from_bytes(&self.octets)
            .ok()
            .map(|header| header.kind)
    }

    /// The octets after the four-octet encapsulation header.
    ///
    /// `None` when the payload is shorter than a header.
    #[must_use]
    pub fn body(&self) -> Option<&[u8]> {
        self.octets.get(ENCAPSULATION_HEADER_LEN..)
    }

    /// True when the encapsulation names one of the two RTPS parameter-list
    /// representations, `PL_CDR_BE` or `PL_CDR_LE`.
    ///
    /// The predicate discovery uses to tell an announcement from a topic
    /// sample before it commits to a decoder.
    #[must_use]
    pub fn is_parameter_list(&self) -> bool {
        self.encapsulation()
            .is_some_and(EncapsulationKind::is_rtps_parameter_list)
    }

    /// Decode the payload as an RTPS parameter list, returning it with the
    /// encoding its header selected.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`](crate::RtpsError::Cdr) with the reason: an
    /// identifier that is not `PL_CDR_*`, a missing sentinel, a parameter
    /// whose length runs past the end.
    pub fn parameter_list(&self) -> RtpsResult<(ParameterList<'_>, Encoding)> {
        Ok(ParameterList::decode(&self.octets)?)
    }

    /// Decode the payload as a CDR value of type `T`, forgiving the
    /// four-octet padding RTPS payloads carry.
    ///
    /// Up to three trailing octets are ignored. That is not laxness: a
    /// `serializedPayload` is delimited by `octetsToNextHeader`, which counts
    /// the padding a sender added to keep the next submessage on a four-octet
    /// boundary, and under XCDR1 there is nowhere on the wire to record how
    /// much of it there was. Use [`SerializedPayload::decode_exact`] where
    /// the payload's length is known to be exact.
    ///
    /// # Errors
    ///
    /// Whatever `T`'s [`CdrDeserialize`](astrs_cdr::CdrDeserialize) impl
    /// returns, plus the framing errors of
    /// [`from_bytes_tolerant`](astrs_cdr::from_bytes_tolerant).
    pub fn decode<'de, T: astrs_cdr::CdrDeserialize<'de>>(&'de self) -> RtpsResult<T> {
        Ok(astrs_cdr::from_bytes_tolerant(&self.octets)?)
    }

    /// Decode the payload as a CDR value of type `T`, rejecting **any**
    /// leftover octet.
    ///
    /// The strict door: a payload the declared type did not consume is a type
    /// mismatch, and a schema drift caught here is a schema drift not caught
    /// a week later. Use it when the sender is known not to pad — a payload
    /// this crate built with [`SerializedPayload::from_cdr`] usually is
    /// padded, so this is for what a peer sent.
    ///
    /// # Errors
    ///
    /// Whatever `T`'s [`CdrDeserialize`](astrs_cdr::CdrDeserialize) impl
    /// returns, plus [`CdrError::TrailingBytes`](astrs_cdr::CdrError::TrailingBytes).
    pub fn decode_exact<'de, T: astrs_cdr::CdrDeserialize<'de>>(&'de self) -> RtpsResult<T> {
        Ok(astrs_cdr::from_bytes(&self.octets)?)
    }

    /// Detach from the input buffer, cloning the octets if they are borrowed.
    #[must_use]
    pub fn into_owned(self) -> SerializedPayload<'static> {
        SerializedPayload {
            octets: Cow::Owned(self.octets.into_owned()),
        }
    }

    /// The octets as a `Cow`, for a caller that wants to keep the borrow.
    #[must_use]
    pub fn into_cow(self) -> Cow<'a, [u8]> {
        self.octets
    }
}

impl fmt::Display for SerializedPayload<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.encapsulation() {
            Some(kind) => write!(f, "{} octets, {kind:?}", self.octets.len()),
            None => write!(f, "{} octets, no encapsulation header", self.octets.len()),
        }
    }
}

impl<'a> From<&'a [u8]> for SerializedPayload<'a> {
    fn from(octets: &'a [u8]) -> Self {
        Self::new(octets)
    }
}

impl From<Vec<u8>> for SerializedPayload<'static> {
    fn from(octets: Vec<u8>) -> Self {
        SerializedPayload {
            octets: Cow::Owned(octets),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use astrs_cdr::{ParameterId, pid};

    use super::*;

    #[test]
    fn a_cdr_payload_starts_with_its_encapsulation_header() {
        let payload = SerializedPayload::from_cdr(&7_u32).expect("encode");
        assert_eq!(payload.as_slice(), [0x00, 0x01, 0x00, 0x00, 7, 0, 0, 0]);
        assert_eq!(payload.encapsulation(), Some(EncapsulationKind::CdrLe));
        assert_eq!(payload.body(), Some(&[7_u8, 0, 0, 0][..]));
        assert_eq!(payload.len(), 8);
        assert!(!payload.is_empty());
        assert!(!payload.is_parameter_list());
        assert_eq!(payload.decode::<u32>().expect("decode"), 7);
        assert_eq!(payload.to_string(), "8 octets, CdrLe");
    }

    #[test]
    fn a_big_endian_payload_keeps_its_own_identifier() {
        let payload =
            SerializedPayload::from_cdr_with(&7_u32, Encoding::new(EncapsulationKind::CdrBe))
                .expect("encode");
        assert_eq!(payload.as_slice(), [0x00, 0x00, 0x00, 0x00, 0, 0, 0, 7]);
        assert_eq!(payload.encapsulation(), Some(EncapsulationKind::CdrBe));
        assert_eq!(payload.decode::<u32>().expect("decode"), 7);
    }

    #[test]
    fn a_parameter_list_payload_is_recognised_and_decoded() {
        let mut list = ParameterList::new(Encoding::DISCOVERY);
        list.push_octets(ParameterId::new(pid::KEY_HASH), &[1_u8, 2, 3, 4][..])
            .expect("short enough");
        let payload = SerializedPayload::from_parameter_list(&list).expect("encode");

        assert_eq!(payload.encapsulation(), Some(EncapsulationKind::PlCdrLe));
        assert!(payload.is_parameter_list());
        let (decoded, encoding) = payload.parameter_list().expect("decode");
        assert_eq!(encoding.kind(), EncapsulationKind::PlCdrLe);
        assert_eq!(decoded, list);
    }

    #[test]
    fn an_empty_or_short_payload_has_no_encapsulation() {
        let empty = SerializedPayload::empty();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert_eq!(empty.encapsulation(), None);
        assert_eq!(empty.body(), None);
        assert!(!empty.is_parameter_list());
        assert_eq!(empty.to_string(), "0 octets, no encapsulation header");
        assert_eq!(SerializedPayload::default(), empty);

        let stub = SerializedPayload::new(&[0x00, 0x01][..]);
        assert_eq!(stub.encapsulation(), None);
        assert_eq!(stub.body(), None);

        // An identifier outside the XTypes table is not an encapsulation.
        let unknown = SerializedPayload::new(&[0x00, 0x05, 0x00, 0x00][..]);
        assert_eq!(unknown.encapsulation(), None);
        assert_eq!(unknown.body(), Some(&[][..]));
    }

    #[test]
    fn a_padded_payload_still_decodes_back_to_its_value() {
        // A one-octet CDR body is five octets before padding. Padding it to
        // eight is what keeps the DATA followable (§8.3.3), and under XCDR1
        // there is nowhere to record how many octets were added — so decode
        // forgives them and decode_exact does not.
        let payload = SerializedPayload::from_cdr(&0xab_u8).expect("encode");
        assert_eq!(payload.len(), 8);
        assert!(payload.is_aligned());
        assert_eq!(payload.as_slice(), [0x00, 0x01, 0x00, 0x00, 0xab, 0, 0, 0]);
        assert_eq!(payload.decode::<u8>().expect("tolerant decode"), 0xab);
        assert_eq!(
            payload.decode_exact::<u8>(),
            Err(astrs_cdr::CdrError::TrailingBytes { remaining: 3 }.into())
        );

        // A payload that needs no padding decodes either way.
        let exact = SerializedPayload::from_cdr(&0x1234_5678_u32).expect("encode");
        assert_eq!(exact.len(), 8);
        assert_eq!(exact.decode::<u32>().expect("decode"), 0x1234_5678);
        assert_eq!(exact.decode_exact::<u32>().expect("decode"), 0x1234_5678);
    }

    #[test]
    fn a_borrowed_payload_can_be_detached() {
        let octets = Vec::from([0x00_u8, 0x01, 0x00, 0x00, 9, 0, 0, 0]);
        let owned = {
            let borrowed = SerializedPayload::from(&octets[..]);
            assert_eq!(borrowed.len(), 8);
            borrowed.into_owned()
        };
        assert_eq!(owned.decode::<u32>().expect("decode"), 9);
        assert_eq!(
            SerializedPayload::from(octets.clone()).as_slice(),
            &octets[..]
        );
        assert_eq!(
            SerializedPayload::from(octets.clone()).into_cow().len(),
            octets.len()
        );
    }
}
