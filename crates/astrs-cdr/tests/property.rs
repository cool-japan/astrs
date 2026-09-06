//! Property and fuzz tests over the CDR codec (blueprint §20.2).
//!
//! Five properties, asserted over generated values rather than hand-picked
//! ones:
//!
//! 1. **Round trip.** `decode(encode(v)) == v` for every value, in every
//!    encapsulation kind, both byte orders and both CDR versions. Floats are
//!    compared by bit pattern, because CDR transmits bits and `NaN != NaN`
//!    would hide exactly the bug this checks for.
//! 2. **Alignment invariants.** Every primitive starts at a stream position
//!    that is a multiple of `min(size, cap)`, and the padding in between is
//!    zero. Asserted against a generated script of writes, so the property
//!    holds for orderings no hand-written vector would try.
//! 3. **Truncation is fatal.** No proper prefix of a valid encoding decodes.
//! 4. **Trailing octets are fatal.** No suffix may be appended to a valid
//!    encoding and still decode.
//! 5. **No panic, no unbounded allocation.** Arbitrary octets produce a typed
//!    [`CdrError`] or a plausible value — never a panic, and never an
//!    allocation larger than the input can justify.
//!
//! Plus the algebraic facts the codec rests on: size prediction agrees with
//! encoding, byte order changes octets but not values, and a bound is
//! enforced on both sides of the wire.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_cdr::{
    BoundedSequence, BoundedString, CdrError, CdrReader, CdrType, CdrWriter, EncapsulationKind,
    Encoding, Extensibility, ParameterId, ParameterList, WString, cdr_struct, from_bytes,
    from_bytes_headerless, serialized_size, to_vec, to_vec_headerless,
};
use proptest::prelude::*;

cdr_struct! {
    /// A struct whose members span every alignment class, so a generated
    /// value exercises padding at 2, 4 and 8 in one round trip.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Mixed {
        /// Alignment 1.
        pub flag: bool,
        /// Alignment 1.
        pub tag: u8,
        /// Alignment 2.
        pub small: i16,
        /// Alignment 4.
        pub medium: u32,
        /// Alignment 8 (4 under XCDR2).
        pub large: i64,
        /// A length-prefixed member.
        pub name: String,
        /// A sequence of primitives.
        pub samples: Vec<f64>,
    }
}

cdr_struct! {
    /// A struct nested inside a sequence, so the XCDR2 DHEADER rule for
    /// non-primitive elements is exercised.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Entry {
        /// Key.
        pub key: String,
        /// Value.
        pub count: u32,
    }
}

cdr_struct! {
    /// The outer type: a sequence of non-primitives plus a fixed array.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Table {
        /// Rows.
        pub entries: Vec<Entry>,
        /// A fixed-size member, which carries no length of its own.
        pub checksum: [u8; 4],
    }
}

cdr_struct! {
    /// An `@appendable` type, so the delimited identifiers get exercised
    /// against a type whose extensibility they actually announce.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Appending: Appendable {
        /// Rows a newer version might append to.
        pub entries: Vec<Entry>,
    }
}

/// Octets a value occupies without its encapsulation header.
fn len_of<T: astrs_cdr::CdrSerialize + ?Sized>(value: &T, encoding: Encoding) -> usize {
    to_vec_headerless(value, encoding).expect("encode").len()
}

/// The encapsulation kinds a `@final` value may legally be announced with.
///
/// `D_CDR2_*` and `PL_CDR2_*` are deliberately absent: those identifiers
/// declare an `@appendable` or `@mutable` top-level type, and pairing one with
/// a final type would generate payloads no conformant peer emits. The
/// identifier for a given extensibility comes from
/// [`Encoding::for_extensibility`]; a *nested* member's framing follows its
/// own type and is exercised by `Table`, whose element type is a struct.
const FINAL_KINDS: [EncapsulationKind; 4] = [
    EncapsulationKind::CdrBe,
    EncapsulationKind::CdrLe,
    EncapsulationKind::Cdr2Be,
    EncapsulationKind::Cdr2Le,
];

/// Every identifier, for the paths that must survive whatever arrives.
const ALL_KINDS: [EncapsulationKind; 10] = EncapsulationKind::ALL;

/// Strategies for the generated values.
mod arb {
    use super::*;

    /// A string with no interior NUL — the IDL `string` contract.
    pub fn text() -> impl Strategy<Value = String> {
        proptest::collection::vec(any::<char>().prop_filter("no NUL", |c| *c != '\0'), 0..12)
            .prop_map(|chars| chars.into_iter().collect())
    }

    /// A `wstring`, including unpaired surrogates, which are legal on the
    /// wire and must survive.
    pub fn wide() -> impl Strategy<Value = WString> {
        proptest::collection::vec(any::<u16>(), 0..12).prop_map(WString::from_units)
    }

    pub fn mixed() -> impl Strategy<Value = Mixed> {
        (
            any::<bool>(),
            any::<u8>(),
            any::<i16>(),
            any::<u32>(),
            any::<i64>(),
            text(),
            proptest::collection::vec(any::<f64>(), 0..6),
        )
            .prop_map(|(flag, tag, small, medium, large, name, samples)| Mixed {
                flag,
                tag,
                small,
                medium,
                large,
                name,
                samples,
            })
    }

    pub fn entry() -> impl Strategy<Value = Entry> {
        (text(), any::<u32>()).prop_map(|(key, count)| Entry { key, count })
    }

    pub fn table() -> impl Strategy<Value = Table> {
        (proptest::collection::vec(entry(), 0..5), any::<[u8; 4]>())
            .prop_map(|(entries, checksum)| Table { entries, checksum })
    }

    pub fn kind() -> impl Strategy<Value = EncapsulationKind> {
        proptest::sample::select(FINAL_KINDS.to_vec())
    }

    pub fn encoding() -> impl Strategy<Value = Encoding> {
        kind().prop_map(Encoding::new)
    }

    /// A parameter list with generated ids and values.
    pub fn parameter_list() -> impl Strategy<Value = Vec<(u16, Vec<u8>)>> {
        proptest::collection::vec(
            (
                any::<u16>().prop_filter("not structural or reserved", |id| {
                    *id != 0x0000 && *id != 0x0001 && *id != 0x3f01 && *id != 0x3f02
                }),
                proptest::collection::vec(any::<u8>(), 0..24),
            ),
            0..8,
        )
    }
}

/// Compare two `Mixed` values by bit pattern, so `NaN` round trips are
/// checked rather than skipped.
fn mixed_eq(left: &Mixed, right: &Mixed) -> bool {
    left.flag == right.flag
        && left.tag == right.tag
        && left.small == right.small
        && left.medium == right.medium
        && left.large == right.large
        && left.name == right.name
        && left.samples.len() == right.samples.len()
        && left
            .samples
            .iter()
            .zip(&right.samples)
            .all(|(a, b)| a.to_bits() == b.to_bits())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(192))]

    /// Property 1: every value round trips, in every encoding.
    #[test]
    fn mixed_structs_round_trip(value in arb::mixed(), encoding in arb::encoding()) {
        let bytes = to_vec(&value, encoding).expect("encode");
        let decoded = from_bytes::<Mixed>(&bytes).expect("decode");
        prop_assert!(mixed_eq(&value, &decoded), "{value:?} != {decoded:?}");
    }

    #[test]
    fn tables_round_trip(value in arb::table(), encoding in arb::encoding()) {
        let bytes = to_vec(&value, encoding).expect("encode");
        prop_assert_eq!(from_bytes::<Table>(&bytes).expect("decode"), value);
    }

    #[test]
    fn strings_round_trip(value in arb::text(), encoding in arb::encoding()) {
        let bytes = to_vec(&value, encoding).expect("encode");
        prop_assert_eq!(from_bytes::<String>(&bytes).expect("decode"), value.clone());
        // …and the borrowed accessor sees the same characters without
        // copying them.
        prop_assert_eq!(from_bytes::<&str>(&bytes).expect("decode"), value.as_str());
    }

    #[test]
    fn wstrings_round_trip(value in arb::wide(), encoding in arb::encoding()) {
        let bytes = to_vec(&value, encoding).expect("encode");
        prop_assert_eq!(from_bytes::<WString>(&bytes).expect("decode"), value);
    }

    #[test]
    fn floats_round_trip_bit_for_bit(bits in any::<u64>(), encoding in arb::encoding()) {
        // Generating the *bits* rather than the value reaches every NaN
        // payload, signalling ones included.
        let value = f64::from_bits(bits);
        let bytes = to_vec(&value, encoding).expect("encode");
        let decoded = from_bytes::<f64>(&bytes).expect("decode");
        prop_assert_eq!(decoded.to_bits(), bits);
    }

    #[test]
    fn octet_sequences_round_trip_and_stay_borrowed(
        octets in proptest::collection::vec(any::<u8>(), 0..64),
        encoding in arb::encoding(),
    ) {
        let bytes = to_vec(&octets, encoding).expect("encode");
        let borrowed: &[u8] = from_bytes(&bytes).expect("decode");
        prop_assert_eq!(borrowed, octets.as_slice());
        if !octets.is_empty() {
            prop_assert!(core::ptr::eq(borrowed.as_ptr(), bytes[8..].as_ptr()));
        }
    }

    /// Property 2: alignment. Every primitive starts on a boundary that is a
    /// multiple of its capped alignment, and the padding before it is zero.
    #[test]
    fn writes_land_on_their_alignment_boundaries(
        script in proptest::collection::vec(0_u8..6, 1..40),
        encoding in arb::encoding(),
    ) {
        let cap = encoding.version().max_alignment();
        let mut writer = CdrWriter::headerless(encoding);
        // Positions we expect the reader to visit, and the alignment each
        // one had to satisfy.
        let mut expected: Vec<(usize, usize)> = Vec::new();
        for op in &script {
            let natural = match op {
                0 | 1 => 1_usize,
                2 => 2,
                3 | 4 => 4,
                _ => 8,
            };
            let effective = natural.min(cap);
            // Where the writer *will* put this member.
            let start = writer.position().div_ceil(effective) * effective;
            match op {
                0 => writer.write_u8(0xa5).expect("write"),
                1 => writer.write_bool(true).expect("write"),
                2 => writer.write_u16(0xbeef).expect("write"),
                3 => writer.write_u32(0xdead_beef).expect("write"),
                4 => writer.write_f32(1.5).expect("write"),
                _ => writer.write_u64(u64::MAX).expect("write"),
            }
            prop_assert_eq!(
                writer.position(),
                start + natural,
                "member of natural alignment {} ended in the wrong place",
                natural
            );
            expected.push((start, effective));
            prop_assert_eq!(start % effective, 0);
        }

        let octets = writer.finish();

        // Re-read the same script and check the reader visits the same
        // positions the writer chose — the two must agree or nothing else in
        // this crate is true.
        let mut reader = CdrReader::with_encoding(&octets, encoding);
        for (op, (start, effective)) in script.iter().zip(&expected) {
            prop_assert!(reader.position() <= *start);
            // Every octet skipped as padding must be zero.
            for (offset, octet) in octets
                .iter()
                .enumerate()
                .take(*start)
                .skip(reader.position())
            {
                prop_assert_eq!(*octet, 0, "padding at {} is not zero", offset);
            }
            match op {
                0 => { reader.read_u8().expect("read"); }
                1 => { reader.read_bool().expect("read"); }
                2 => { reader.read_u16().expect("read"); }
                3 => { reader.read_u32().expect("read"); }
                4 => { reader.read_f32().expect("read"); }
                _ => { reader.read_u64().expect("read"); }
            }
            prop_assert_eq!(*start % *effective, 0);
        }
        prop_assert_eq!(reader.finish(), Ok(()));
    }

    /// Property 3: no proper prefix of a valid encoding decodes.
    #[test]
    fn truncated_payloads_are_refused(value in arb::mixed(), encoding in arb::encoding()) {
        let bytes = to_vec(&value, encoding).expect("encode");
        for cut in 0..bytes.len() {
            prop_assert!(
                from_bytes::<Mixed>(&bytes[..cut]).is_err(),
                "a {cut}-octet prefix of a {}-octet payload decoded",
                bytes.len()
            );
        }
        prop_assert!(from_bytes::<Mixed>(&bytes).is_ok());
    }

    #[test]
    fn truncated_tables_are_refused(value in arb::table(), encoding in arb::encoding()) {
        let bytes = to_vec(&value, encoding).expect("encode");
        for cut in 0..bytes.len() {
            prop_assert!(from_bytes::<Table>(&bytes[..cut]).is_err(), "prefix {cut} decoded");
        }
    }

    /// Property 4: nothing may be appended to a valid encoding.
    #[test]
    fn trailing_octets_are_refused(
        value in arb::mixed(),
        encoding in arb::encoding(),
        tail in proptest::collection::vec(any::<u8>(), 1..8),
    ) {
        let mut bytes = to_vec(&value, encoding).expect("encode");
        let appended = tail.len();
        bytes.extend_from_slice(&tail);
        prop_assert_eq!(
            from_bytes::<Mixed>(&bytes),
            Err(CdrError::TrailingBytes { remaining: appended })
        );
    }

    /// Property 5: hostile octets never panic and never over-allocate.
    #[test]
    fn arbitrary_octets_never_panic_a_decoder(
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
    ) {
        // Every decoder this crate exposes, pointed at the same garbage.
        let _ = from_bytes::<Mixed>(&bytes);
        let _ = from_bytes::<Table>(&bytes);
        let _ = from_bytes::<String>(&bytes);
        let _ = from_bytes::<WString>(&bytes);
        let _ = from_bytes::<Vec<u8>>(&bytes);
        let _ = from_bytes::<Vec<f64>>(&bytes);
        let _ = from_bytes::<Vec<String>>(&bytes);
        let _ = from_bytes::<Vec<Vec<u32>>>(&bytes);
        let _ = from_bytes::<[u64; 4]>(&bytes);
        let _ = from_bytes::<BoundedString<8>>(&bytes);
        let _ = from_bytes::<BoundedSequence<u32, 8>>(&bytes);
        let _ = ParameterList::decode(&bytes);
        let _ = ParameterList::decode_any(&bytes);
        for kind in ALL_KINDS {
            let encoding = Encoding::new(kind);
            let _ = from_bytes_headerless::<Mixed>(&bytes, encoding);
            let _ = from_bytes_headerless::<Table>(&bytes, encoding);
            let _ = ParameterList::decode_headerless(&bytes, encoding);
        }
    }

    #[test]
    fn a_corrupted_valid_payload_never_panics(
        value in arb::mixed(),
        encoding in arb::encoding(),
        index in 0_usize..64,
        replacement in any::<u8>(),
    ) {
        // Corruption of a *valid* payload reaches decoder states that pure
        // noise rarely does: plausible lengths, plausible offsets.
        let mut bytes = to_vec(&value, encoding).expect("encode");
        if bytes.is_empty() {
            return Ok(());
        }
        let at = index % bytes.len();
        bytes[at] = replacement;
        let _ = from_bytes::<Mixed>(&bytes);
    }

    /// Size prediction agrees with the encoder, for every value and encoding.
    #[test]
    fn predicted_size_matches_the_encoding(
        value in arb::mixed(),
        encoding in arb::encoding(),
    ) {
        let predicted = serialized_size(&value, encoding).expect("size");
        let actual = to_vec(&value, encoding).expect("encode").len();
        prop_assert_eq!(predicted, actual);
    }

    /// Byte order changes the octets but never the value, and the two
    /// encodings have the same length.
    #[test]
    fn byte_order_preserves_the_value_and_the_length(value in arb::mixed()) {
        for (little, big) in [
            (EncapsulationKind::CdrLe, EncapsulationKind::CdrBe),
            (EncapsulationKind::Cdr2Le, EncapsulationKind::Cdr2Be),
        ] {
            let le = to_vec(&value, Encoding::new(little)).expect("encode");
            let be = to_vec(&value, Encoding::new(big)).expect("encode");
            prop_assert_eq!(le.len(), be.len());
            let from_le = from_bytes::<Mixed>(&le).expect("decode");
            let from_be = from_bytes::<Mixed>(&be).expect("decode");
            prop_assert!(mixed_eq(&from_le, &from_be));
        }
    }

    /// The extensibility of the top-level type picks the identifier, and the
    /// pairing round trips. This is where `D_CDR2_*` and `PL_CDR2_*` belong:
    /// with a type whose extensibility they actually announce.
    #[test]
    fn appendable_types_round_trip_under_their_own_identifier(
        entries in proptest::collection::vec(arb::entry(), 0..5),
        base in arb::encoding(),
    ) {
        let value = Appending { entries };
        let encoding = base.for_extensibility(Appending::EXTENSIBILITY);
        // Under XCDR2 that is DELIMIT_CDR; under XCDR1 there is no delimited
        // form, so the plain identifier is correct.
        if encoding.is_v2() {
            prop_assert_eq!(
                encoding.kind().declared_extensibility(),
                Extensibility::Appendable
            );
        }
        let bytes = to_vec(&value, encoding).expect("encode");
        prop_assert_eq!(from_bytes::<Appending>(&bytes).expect("decode"), value);
    }

    /// `MIN_SERIALIZED_SIZE` is what the reader multiplies a sequence count
    /// by before allocating, so an over-report silently rejects legal
    /// traffic. It must be a true lower bound for *every* value, not only the
    /// hand-picked ones.
    #[test]
    fn min_serialized_size_is_a_true_lower_bound(
        value in arb::mixed(),
        table in arb::table(),
        entry in arb::entry(),
        text in arb::text(),
        wide in arb::wide(),
        encoding in arb::encoding(),
    ) {
        let checks: [(usize, usize); 7] = [
            (Mixed::MIN_SERIALIZED_SIZE, len_of(&value, encoding)),
            (Table::MIN_SERIALIZED_SIZE, len_of(&table, encoding)),
            (Entry::MIN_SERIALIZED_SIZE, len_of(&entry, encoding)),
            (<String as CdrType>::MIN_SERIALIZED_SIZE, len_of(&text, encoding)),
            (<WString as CdrType>::MIN_SERIALIZED_SIZE, len_of(&wide, encoding)),
            (
                <Vec<f64> as CdrType>::MIN_SERIALIZED_SIZE,
                len_of(&value.samples, encoding),
            ),
            (
                <[u8; 4] as CdrType>::MIN_SERIALIZED_SIZE,
                len_of(&table.checksum, encoding),
            ),
        ];
        for (declared, actual) in checks {
            prop_assert!(
                declared <= actual,
                "declared minimum {} exceeds the {} octets the value occupies",
                declared,
                actual
            );
        }
    }

    /// XCDR2 never produces a longer encoding than XCDR1 for the same value:
    /// the only difference is a smaller alignment cap.
    #[test]
    fn xcdr2_is_never_longer_than_xcdr1(value in arb::mixed()) {
        let v1 = to_vec(&value, Encoding::new(EncapsulationKind::CdrLe)).expect("encode");
        let v2 = to_vec(&value, Encoding::new(EncapsulationKind::Cdr2Le)).expect("encode");
        prop_assert!(v2.len() <= v1.len(), "{} > {}", v2.len(), v1.len());
    }

    /// A parameter list survives a round trip exactly, including ids this
    /// crate does not model.
    #[test]
    fn parameter_lists_round_trip(entries in arb::parameter_list()) {
        for kind in [EncapsulationKind::PlCdrLe, EncapsulationKind::PlCdrBe] {
            let encoding = Encoding::new(kind);
            let mut list = ParameterList::new(encoding);
            for (id, value) in &entries {
                list.push_octets(ParameterId::new(*id), value.clone())
                    .expect("short enough");
            }
            let bytes = list.encode().expect("encode");
            let (decoded, seen) = ParameterList::decode(&bytes).expect("decode");
            prop_assert_eq!(seen.kind(), kind);
            prop_assert_eq!(&decoded, &list);
            // …and re-encoding reproduces the octets exactly.
            prop_assert_eq!(decoded.encode().expect("re-encode"), bytes);
        }
    }

    #[test]
    fn parameter_list_bodies_stay_four_octet_aligned(entries in arb::parameter_list()) {
        let mut list = ParameterList::new(Encoding::DISCOVERY);
        for (id, value) in &entries {
            list.push_octets(ParameterId::new(*id), value.clone())
                .expect("short enough");
        }
        let bytes = list.encode().expect("encode");
        // Header, then a whole number of four-octet groups: entry headers
        // and padded values alike.
        prop_assert_eq!(bytes.len() % 4, 0);
        prop_assert_eq!(bytes.len(), 4 + list.serialized_len());
        for parameter in list.iter() {
            prop_assert_eq!(parameter.value.len() % 4, 0);
        }
    }

    /// Typed parameter values survive the padding their list adds.
    #[test]
    fn typed_parameter_values_round_trip(text in arb::text()) {
        let mut list = ParameterList::new(Encoding::DISCOVERY);
        list.push_value(ParameterId::new(0x0005), &text).expect("encode value");
        let bytes = list.encode().expect("encode");
        let (decoded, _) = ParameterList::decode(&bytes).expect("decode");
        let read_back: String = decoded
            .as_slice()
            .first()
            .expect("one entry")
            .decode_value(Encoding::DISCOVERY)
            .expect("decode value");
        prop_assert_eq!(read_back, text);
    }

    /// A bound is enforced on both sides: an over-long value cannot be built,
    /// and an over-long encoding cannot be accepted.
    #[test]
    fn bounds_are_enforced_symmetrically(text in arb::text()) {
        const BOUND: usize = 8;
        let within = text.len() <= BOUND;
        let built = BoundedString::<BOUND>::new(text.clone());
        prop_assert_eq!(built.is_ok(), within);

        // Whatever this side can build, the other side accepts; whatever it
        // cannot, the other side refuses.
        let wire = to_vec(&text, Encoding::ROS2).expect("encode");
        let decoded = from_bytes::<BoundedString<BOUND>>(&wire);
        prop_assert_eq!(decoded.is_ok(), within);
        if !within {
            prop_assert_eq!(
                decoded.unwrap_err(),
                CdrError::BoundExceeded {
                    bound: BOUND,
                    actual: text.len(),
                    context: "string<N>",
                }
            );
        }
    }

    #[test]
    fn bounded_sequences_are_enforced_symmetrically(
        values in proptest::collection::vec(any::<u32>(), 0..12),
    ) {
        const BOUND: usize = 6;
        let within = values.len() <= BOUND;
        prop_assert_eq!(
            BoundedSequence::<u32, BOUND>::from_vec(values.clone()).is_ok(),
            within
        );
        let wire = to_vec(&values, Encoding::ROS2).expect("encode");
        prop_assert_eq!(
            from_bytes::<BoundedSequence<u32, BOUND>>(&wire).is_ok(),
            within
        );
    }

    /// A headerless encoding is exactly the body of the header-bearing one.
    #[test]
    fn headerless_encoding_is_the_body(value in arb::mixed(), encoding in arb::encoding()) {
        let with = to_vec(&value, encoding).expect("encode");
        let without = to_vec_headerless(&value, encoding).expect("encode");
        prop_assert_eq!(&with[4..], without.as_slice());
        let decoded = from_bytes_headerless::<Mixed>(&without, encoding).expect("decode");
        prop_assert!(mixed_eq(&value, &decoded));
    }

    /// An interior NUL is refused on the way out as well as the way in, so
    /// decode-then-encode is total for everything this crate accepts.
    #[test]
    fn interior_nuls_are_refused_on_encode(
        prefix in "[a-z]{0,4}",
        suffix in "[a-z]{0,4}",
    ) {
        let text = format!("{prefix}\0{suffix}");
        prop_assert_eq!(
            to_vec(&text, Encoding::ROS2),
            Err(CdrError::InteriorNul { index: prefix.len() })
        );
    }

    /// Decoding anything this crate produced, then re-encoding it, is the
    /// identity on octets — the property a bridge relies on when it forwards
    /// a sample it only partly understands.
    #[test]
    fn decode_then_encode_is_the_identity(value in arb::table(), encoding in arb::encoding()) {
        let bytes = to_vec(&value, encoding).expect("encode");
        let decoded = from_bytes::<Table>(&bytes).expect("decode");
        prop_assert_eq!(to_vec(&decoded, encoding).expect("re-encode"), bytes);
    }
}

#[test]
fn a_hostile_sequence_length_costs_one_comparison() {
    // Not a property but the case the properties exist to protect: a count
    // of four billion eight-octet elements, with four octets present. If the
    // check were missing this test would exhaust memory rather than fail.
    let hostile = [
        0x00, 0x01, 0x00, 0x00, // CDR_LE
        0xff, 0xff, 0xff, 0xff, // count = 0xffff_ffff
        0x00, 0x00, 0x00, 0x00, // …and four octets of payload
    ];
    assert_eq!(
        from_bytes::<Vec<f64>>(&hostile),
        Err(CdrError::LengthOverflow {
            declared: 0xffff_ffff,
            available: 4,
            element_size: 8,
            context: "sequence",
        })
    );
    assert!(from_bytes::<Vec<Vec<String>>>(&hostile).is_err());
    assert!(from_bytes::<String>(&hostile).is_err());
    assert!(from_bytes::<WString>(&hostile).is_err());
}

#[test]
fn a_hostile_parameter_list_cannot_grow_without_bound() {
    // Every entry costs at least four octets, so the entry count is bounded
    // by the datagram size; the explicit ceiling makes that bound visible.
    let mut bytes = vec![0x00, 0x03, 0x00, 0x00];
    for _ in 0..64 {
        bytes.extend_from_slice(&[0x70, 0x00, 0x00, 0x00]);
    }
    bytes.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]);
    let (list, _) = ParameterList::decode(&bytes).expect("decode");
    assert_eq!(list.len(), 64);

    let mut reader = CdrReader::new(&bytes).expect("header");
    assert_eq!(
        ParameterList::read_with_limit(&mut reader, 8).map(|_| ()),
        Err(CdrError::SequenceTooLong {
            length: 9,
            maximum: 8,
        })
    );
}

#[test]
fn every_encapsulation_identifier_either_parses_or_is_named() {
    // The decoder's front door, over the whole 16-bit identifier space: an
    // identifier is either one of the ten this crate knows or is refused by
    // number. Nothing in between, and nothing panics.
    let mut recognised = 0_usize;
    for identifier in 0..=u16::MAX {
        let mut bytes = identifier.to_be_bytes().to_vec();
        bytes.extend_from_slice(&[0x00, 0x00]);
        match CdrReader::new(&bytes) {
            Ok(reader) => {
                recognised += 1;
                assert_eq!(reader.encoding().kind().identifier(), identifier);
            }
            Err(CdrError::UnknownEncapsulation { identifier: seen }) => {
                assert_eq!(seen, identifier);
            }
            Err(other) => panic!("identifier 0x{identifier:04x} produced {other:?}"),
        }
    }
    assert_eq!(recognised, 10, "exactly the XTypes Table 47 identifiers");
}
