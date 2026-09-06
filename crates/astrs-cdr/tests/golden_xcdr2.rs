//! Golden vectors: XCDR2 (OMG DDS-XTypes 1.3 §7.4.3, §7.6.3).
//!
//! XCDR2 differs from XCDR1 in three ways, and there is a vector for each:
//!
//! 1. **The alignment cap.** Every alignment is `min(size, 4)`, so an
//!    eight-octet primitive aligns to four (§7.4.3.4.1). This is the only
//!    change that affects a `@final` type — which is every `rosidl`-generated
//!    ROS 2 message, and therefore the case that matters most for reading
//!    Jazzy-era traffic.
//! 2. **DHEADER delimiting.** An `@appendable` or `@mutable` type, and a
//!    collection of non-primitive elements, is preceded by an `unsigned long`
//!    giving the octet length of what follows (§7.4.3.5.3). An older reader
//!    uses it to skip members it does not know.
//! 3. **EMHEADER tagging.** Each member of a `@mutable` type is preceded by a
//!    32-bit word carrying its id, a length code and a must-understand flag
//!    (§7.4.3.4.2).
//!
//! The spec calls XCDR2 *read* the requirement for AstRS (blueprint §10.1);
//! the writer exists so these vectors can be asserted from both directions
//! and so the read path can be property-tested against an encoder.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_cdr::{
    CdrError, CdrReader, CdrType, CdrWriter, EmHeader, EncapsulationKind, Encoding, Extensibility,
    LengthCode, cdr_struct, from_bytes, from_bytes_headerless, to_vec, to_vec_headerless,
};

fn v1() -> Encoding {
    Encoding::new(EncapsulationKind::CdrLe)
}

fn v2() -> Encoding {
    Encoding::new(EncapsulationKind::Cdr2Le)
}

cdr_struct! {
    /// A `@final` type whose members straddle the alignment cap.
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct Capped {
        /// One octet, leaving position 1.
        pub flag: u8,
        /// Eight octets: aligned to 8 under XCDR1, to 4 under XCDR2.
        pub value: u64,
    }
}

cdr_struct! {
    /// An `@appendable` type, as a newer version of a topic's type would be.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Growing: Appendable {
        /// The only member this reader knows about.
        pub first: u32,
    }
}

cdr_struct! {
    /// The same type after a member was appended.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct GrowingV2: Appendable {
        /// The member the older reader also knows.
        pub first: u32,
        /// The member it does not.
        pub second: u32,
    }
}

cdr_struct! {
    /// A `@final` element type, so a sequence of it needs a DHEADER.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Pair {
        /// First octet.
        pub left: u8,
        /// Second octet.
        pub right: u8,
    }
}

#[test]
fn golden_alignment_cap_on_a_final_struct() {
    // struct Capped { octet flag; unsigned long long value; }
    //
    // XCDR1 (CORBA 3.0 §15.3.1): `value` aligns to 8.
    //   position 0   01                          flag
    //   position 1   00 00 00 00 00 00 00        seven pad octets
    //   position 8   ef be ad de 00 00 00 00     value = 0xdead_beef
    //   body length 16
    //
    // XCDR2 (§7.4.3.4.1): the alignment is min(8, 4) = 4.
    //   position 0   01                          flag
    //   position 1   00 00 00                    three pad octets
    //   position 4   ef be ad de 00 00 00 00     value
    //   body length 12
    let value = Capped {
        flag: 1,
        value: 0xdead_beef,
    };
    assert_eq!(
        to_vec_headerless(&value, v1()).expect("encode"),
        [
            0x01, //
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
            0xef, 0xbe, 0xad, 0xde, 0x00, 0x00, 0x00, 0x00,
        ]
    );
    assert_eq!(
        to_vec_headerless(&value, v2()).expect("encode"),
        [
            0x01, //
            0x00, 0x00, 0x00, //
            0xef, 0xbe, 0xad, 0xde, 0x00, 0x00, 0x00, 0x00,
        ]
    );

    // Both decode back to the same value; only the octet count differs.
    assert_eq!(
        from_bytes::<Capped>(&to_vec(&value, v1()).expect("encode")).expect("decode"),
        value
    );
    assert_eq!(
        from_bytes::<Capped>(&to_vec(&value, v2()).expect("encode")).expect("decode"),
        value
    );
}

#[test]
fn golden_appendable_struct_carries_a_dheader() {
    // A top-level `@appendable` type is announced as DELIMIT_CDR
    // (`D_CDR2_LE`, identifier 0x0009) and its members are preceded by a
    // DHEADER giving the octets that follow it — not including itself.
    //
    //   offset 0..4  00 09 00 00   D_CDR2_LE
    //   position 0   04 00 00 00   DHEADER = 4
    //   position 4   07 00 00 00   first = 7
    let encoding = v2().for_extensibility(Extensibility::Appendable);
    assert_eq!(encoding.kind(), EncapsulationKind::DCdr2Le);

    let bytes = to_vec(&Growing { first: 7 }, encoding).expect("encode");
    assert_eq!(
        bytes,
        [
            0x00, 0x09, 0x00, 0x00, //
            0x04, 0x00, 0x00, 0x00, //
            0x07, 0x00, 0x00, 0x00,
        ]
    );
    assert_eq!(
        from_bytes::<Growing>(&bytes).expect("decode"),
        Growing { first: 7 }
    );

    // Under XCDR1 there is no DHEADER at all: the delimited form does not
    // exist, and an appendable type is written plain.
    assert_eq!(
        to_vec_headerless(&Growing { first: 7 }, v1()).expect("encode"),
        [0x07, 0x00, 0x00, 0x00]
    );
}

#[test]
fn golden_dheader_gives_an_older_reader_forward_compatibility() {
    // The whole point of a DHEADER. A newer sender writes two members:
    //
    //   position 0   08 00 00 00   DHEADER = 8
    //   position 4   07 00 00 00   first = 7
    //   position 8   09 00 00 00   second = 9  (unknown to the old reader)
    let encoding = v2().for_extensibility(Extensibility::Appendable);
    let newer = to_vec(
        &GrowingV2 {
            first: 7,
            second: 9,
        },
        encoding,
    )
    .expect("encode");
    assert_eq!(
        newer,
        [
            0x00, 0x09, 0x00, 0x00, //
            0x08, 0x00, 0x00, 0x00, //
            0x07, 0x00, 0x00, 0x00, //
            0x09, 0x00, 0x00, 0x00,
        ]
    );

    // An older reader decodes `first`, skips to the declared end, and reports
    // no trailing octets — the sample is accepted, not rejected.
    assert_eq!(
        from_bytes::<Growing>(&newer).expect("forward compatible"),
        Growing { first: 7 }
    );

    // Without the DHEADER — that is, under XCDR1 — the same situation is
    // indistinguishable from a type mismatch, and is refused.
    let newer_v1 = to_vec(
        &GrowingV2 {
            first: 7,
            second: 9,
        },
        v1(),
    )
    .expect("encode");
    assert_eq!(
        from_bytes::<Growing>(&newer_v1),
        Err(CdrError::TrailingBytes { remaining: 4 })
    );
}

#[test]
fn golden_dheader_is_refused_when_it_runs_past_the_buffer() {
    // A corrupt DHEADER claiming 255 octets with 4 present.
    let hostile = [
        0x00, 0x09, 0x00, 0x00, // D_CDR2_LE
        0xff, 0x00, 0x00, 0x00, // DHEADER = 255
        0x07, 0x00, 0x00, 0x00, // …and only four octets follow
    ];
    assert_eq!(
        from_bytes::<Growing>(&hostile),
        Err(CdrError::DelimiterOverrun {
            declared: 255,
            available: 4,
        })
    );
}

#[test]
fn golden_sequence_of_non_primitives_gains_a_dheader() {
    // OMG DDS-XTypes 1.3 §7.4.3.5.3: under XCDR2 a collection whose element
    // type is not primitive is preceded by a DHEADER.
    //
    // sequence<Pair> with two elements:
    //   position 0   08 00 00 00   DHEADER = 8 (the count plus four octets)
    //   position 4   02 00 00 00   count = 2
    //   position 8   01 02         Pair { 1, 2 }
    //   position 10  03 04         Pair { 3, 4 }
    let pairs = vec![Pair { left: 1, right: 2 }, Pair { left: 3, right: 4 }];
    assert_eq!(
        to_vec_headerless(&pairs, v2()).expect("encode"),
        [
            0x08, 0x00, 0x00, 0x00, //
            0x02, 0x00, 0x00, 0x00, //
            0x01, 0x02, //
            0x03, 0x04,
        ]
    );
    assert_eq!(
        from_bytes_headerless::<Vec<Pair>>(&to_vec_headerless(&pairs, v2()).expect("encode"), v2())
            .expect("decode"),
        pairs
    );

    // A sequence of *primitives* gets none, because the element type is in
    // the specification's primitive list.
    assert_eq!(
        to_vec_headerless(&vec![1_u8, 2, 3, 4], v2()).expect("encode"),
        [0x04, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04]
    );

    // And under XCDR1 neither gets one.
    assert_eq!(
        to_vec_headerless(&pairs, v1()).expect("encode"),
        [0x02, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04]
    );
}

#[test]
fn golden_emheader_bit_layout() {
    // OMG DDS-XTypes 1.3 §7.4.3.4.2:
    //
    //   bit 31      must-understand
    //   bits 30..28 length code
    //   bits 27..0  member id
    //
    // id = 42, LC = 0 (one octet), M = 1:
    //   0x8000_0000 | (0 << 28) | 42 = 0x8000_002a
    let header = EmHeader::new(42, LengthCode::Bytes1, true).expect("id fits");
    assert_eq!(header.to_bits(), 0x8000_002a);

    // id = 0, LC = 2 (four octets), M = 0: 0x2000_0000.
    assert_eq!(
        EmHeader::new(0, LengthCode::Bytes4, false)
            .expect("id fits")
            .to_bits(),
        0x2000_0000
    );

    // The word is written in stream byte order and aligned to four, so
    // 0x2000_0001 is `01 00 00 20` little-endian and `20 00 00 01`
    // big-endian.
    let mut le = CdrWriter::headerless(Encoding::new(EncapsulationKind::PlCdr2Le));
    le.write_emheader(EmHeader::new(1, LengthCode::Bytes4, false).expect("id fits"))
        .expect("write");
    assert_eq!(le.finish(), [0x01, 0x00, 0x00, 0x20]);

    let mut be = CdrWriter::headerless(Encoding::new(EncapsulationKind::PlCdr2Be));
    be.write_emheader(EmHeader::new(1, LengthCode::Bytes4, false).expect("id fits"))
        .expect("write");
    assert_eq!(be.finish(), [0x20, 0x00, 0x00, 0x01]);
}

#[test]
fn golden_mutable_struct_with_two_members() {
    // A hand-built `@mutable` type:
    //
    //   @mutable struct Mutable {
    //       @id(1) unsigned long a;
    //       @id(2) string        b;
    //   };
    //
    // offset 0..4   00 0b 00 00   PL_CDR2_LE
    // position 0    17 00 00 00   DHEADER = 23, the octets of all members
    // position 4    01 00 00 20   EMHEADER id 1, LC=2 (a fixed four octets)
    // position 8    44 33 22 11   a = 0x1122_3344
    // position 12   02 00 00 40   EMHEADER id 2, LC=4 (NEXTINT follows)
    // position 16   07 00 00 00   NEXTINT = 7, the string's serialized size
    // position 20   03 00 00 00   the string's own length prefix
    // position 24   68 69 00      "hi\0"
    //
    // The body runs from position 4 to position 27, hence DHEADER = 23.
    let mut writer = CdrWriter::new(Encoding::new(EncapsulationKind::PlCdr2Le));
    writer
        .delimited(|body| {
            body.write_member_sized(1, false, LengthCode::Bytes4, |member| {
                member.write_u32(0x1122_3344)
            })?;
            body.write_member(2, false, |member| member.write_str("hi"))
        })
        .expect("write");
    let bytes = writer.finish();
    assert_eq!(
        bytes,
        [
            0x00, 0x0b, 0x00, 0x00, //
            0x17, 0x00, 0x00, 0x00, //
            0x01, 0x00, 0x00, 0x20, //
            0x44, 0x33, 0x22, 0x11, //
            0x02, 0x00, 0x00, 0x40, //
            0x07, 0x00, 0x00, 0x00, //
            0x03, 0x00, 0x00, 0x00, //
            0x68, 0x69, 0x00,
        ]
    );

    // Reading it back the way generated code would: open the delimited body,
    // then walk members until the scope is exhausted.
    let mut reader = CdrReader::new(&bytes).expect("header");
    let (a, b) = reader
        .delimited(|body| {
            let mut a = 0_u32;
            let mut b = String::new();
            while !body.is_empty() {
                let header = body.read_member_header()?;
                match header.member_id() {
                    1 => a = body.read_member_value(&header)?,
                    2 => b = body.read_member_value(&header)?,
                    _ => body.skip_member(&header)?,
                }
            }
            Ok((a, b))
        })
        .expect("read");
    assert_eq!(a, 0x1122_3344);
    assert_eq!(b, "hi");
    assert_eq!(reader.finish(), Ok(()));
}

#[test]
fn golden_length_code_five_makes_the_nextint_the_members_own_prefix() {
    // LC=5 says: the NEXTINT is both the member's length *and* its first four
    // octets. A `string` fits that exactly — its length prefix is the
    // NEXTINT — so the reader must rewind four octets before handing the
    // stream to the string's deserializer.
    //
    // position 0   01 00 00 50   EMHEADER id 1, LC=5, M=0 (0x5000_0001)
    // position 4   03 00 00 00   NEXTINT = 3 …and the string's length
    // position 8   68 69 00      "hi\0"
    //
    // Total member size is 4 + NEXTINT = 7, counted from position 4.
    let octets = [
        0x01, 0x00, 0x00, 0x50, //
        0x03, 0x00, 0x00, 0x00, //
        0x68, 0x69, 0x00,
    ];
    let mut reader = CdrReader::with_encoding(&octets, Encoding::new(EncapsulationKind::PlCdr2Le));
    let header = reader.read_member_header().expect("member header");
    assert_eq!(header.member_id(), 1);
    assert_eq!(header.header.length_code, LengthCode::NextIntPrefixed);
    assert_eq!(header.body, 4, "the cursor rewound onto the NEXTINT");
    assert_eq!(header.len, 7, "4 + NEXTINT");
    let value: &str = reader.read_member_value(&header).expect("member value");
    assert_eq!(value, "hi");
    assert_eq!(reader.position(), 11);
}

#[test]
fn golden_length_codes_six_and_seven_count_elements() {
    // LC=6: NEXTINT is a count of four-octet elements and is the member's own
    // first four octets — a `sequence<long>`, whose count prefix is exactly
    // that. Total member size is 4 + 4 * NEXTINT.
    //
    // position 0   01 00 00 60   EMHEADER id 1, LC=6 (0x6000_0001)
    // position 4   02 00 00 00   NEXTINT = 2 …and the sequence's count
    // position 8   0a 00 00 00   element 0
    // position 12  0b 00 00 00   element 1
    let octets = [
        0x01, 0x00, 0x00, 0x60, //
        0x02, 0x00, 0x00, 0x00, //
        0x0a, 0x00, 0x00, 0x00, //
        0x0b, 0x00, 0x00, 0x00,
    ];
    let mut reader = CdrReader::with_encoding(&octets, Encoding::new(EncapsulationKind::PlCdr2Le));
    let header = reader.read_member_header().expect("member header");
    assert_eq!(header.header.length_code, LengthCode::NextIntCount4);
    assert_eq!(header.len, 4 + 4 * 2);
    let value: Vec<u32> = reader.read_member_value(&header).expect("member value");
    assert_eq!(value, vec![10, 11]);

    // LC=7 is the same with eight-octet elements: 4 + 8 * NEXTINT.
    assert_eq!(LengthCode::NextIntCount8.member_len(3), Ok(4 + 8 * 3));
    assert_eq!(LengthCode::NextIntCount4.member_len(3), Ok(4 + 4 * 3));
}

#[test]
fn golden_must_understand_member_forces_the_sample_to_be_discarded() {
    // OMG DDS-XTypes 1.3 §7.4.3.4.2: a reader that does not recognise a
    // member flagged must-understand rejects the whole sample, not just the
    // member.
    //
    // position 0   63 00 00 a0   EMHEADER id 0x63, LC=2, M=1 (0xa000_0063)
    // position 4   01 00 00 00   the member's four octets
    let octets = [0x63, 0x00, 0x00, 0xa0, 0x01, 0x00, 0x00, 0x00];
    let mut reader = CdrReader::with_encoding(&octets, Encoding::new(EncapsulationKind::PlCdr2Le));
    let header = reader.read_member_header().expect("member header");
    assert!(header.must_understand());
    assert_eq!(
        reader.skip_member(&header),
        Err(CdrError::UnknownMustUnderstand { member_id: 0x63 })
    );

    // Without the flag the same member is skippable.
    let optional = [0x63, 0x00, 0x00, 0x20, 0x01, 0x00, 0x00, 0x00];
    let mut reader =
        CdrReader::with_encoding(&optional, Encoding::new(EncapsulationKind::PlCdr2Le));
    let header = reader.read_member_header().expect("member header");
    assert!(!header.must_understand());
    assert_eq!(reader.skip_member(&header), Ok(()));
    assert_eq!(reader.finish(), Ok(()));
}

#[test]
fn golden_xcdr1_streams_have_no_dheaders_or_emheaders() {
    // Both structures are XCDR2-only; asking an XCDR1 reader for either is a
    // programming error rather than a decode failure, and says so.
    let octets = [0x04, 0x00, 0x00, 0x00];
    let mut reader = CdrReader::with_encoding(&octets, v1());
    assert_eq!(
        reader.read_dheader(),
        Err(CdrError::UnsupportedEncapsulation(
            "a DHEADER requires an XCDR2 stream"
        ))
    );

    let mut writer = CdrWriter::headerless(v1());
    assert_eq!(
        writer.write_emheader(EmHeader::new(1, LengthCode::Bytes4, false).expect("id fits")),
        Err(CdrError::UnsupportedEncapsulation(
            "an EMHEADER requires an XCDR2 stream"
        ))
    );
}

#[test]
fn golden_extensibility_constants_reach_generated_types() {
    // `cdr_struct!` puts the extensibility on `CdrType`, which is what the
    // writer and reader consult; nothing about the framing is decided at the
    // call site.
    assert_eq!(Capped::EXTENSIBILITY, Extensibility::Final);
    assert_eq!(Growing::EXTENSIBILITY, Extensibility::Appendable);
    assert_eq!(Pair::EXTENSIBILITY, Extensibility::Final);
    // A struct is never an IDL primitive, so a sequence of one carries a
    // DHEADER under XCDR2.
    const { assert!(!Pair::IS_PRIMITIVE) };
}
