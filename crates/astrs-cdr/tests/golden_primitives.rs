//! Golden vectors: encapsulation headers and IDL primitives.
//!
//! Every vector here is derived octet by octet from **OMG CDR** (CORBA 3.0
//! §15.3) and **OMG DDS-XTypes 1.3** (§7.4.3, §7.6.3), with the derivation
//! written out above the assertion: which stream position each member lands
//! on, how many pad octets precede it, and why each octet holds the value it
//! does.
//!
//! Nothing in this file was captured from a C or C++ DDS stack. Cross-stack
//! validation lives in a separate out-of-repo project, per blueprint §18.
//!
//! # The two rules every derivation uses
//!
//! 1. **Alignment.** A primitive of size *n* starts at a stream position that
//!    is a multiple of *n*. Stream position is counted from the octet after
//!    the four-octet encapsulation header, so the header itself never enters
//!    the arithmetic. Under XCDR2 the multiple is capped at four.
//! 2. **Byte order.** The `representation_identifier` and
//!    `representation_options` are always big-endian; everything after them
//!    follows the identifier.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_cdr::{
    CdrError, CdrReader, CdrWriter, EncapsulationKind, Encoding, from_bytes, to_vec,
    to_vec_headerless,
};

/// The encapsulation header for a kind, as four octets.
fn header(kind: EncapsulationKind) -> [u8; 4] {
    let id = kind.identifier().to_be_bytes();
    [id[0], id[1], 0x00, 0x00]
}

#[test]
fn golden_encapsulation_headers() {
    // OMG DDS-XTypes 1.3 §7.6.3.1.2, Table 47. The identifier is a big-endian
    // `unsigned short`, so 0x0003 is the octet pair 00 03 — and it stays that
    // way even though 0x0003 (PL_CDR_LE) announces a little-endian body,
    // because the reader has to parse the identifier before it knows the byte
    // order of anything else.
    //
    //   CDR_BE    0x0000 -> 00 00 | options 00 00
    //   CDR_LE    0x0001 -> 00 01 | options 00 00
    //   PL_CDR_BE 0x0002 -> 00 02 | options 00 00
    //   PL_CDR_LE 0x0003 -> 00 03 | options 00 00
    //   CDR2_BE   0x0006 -> 00 06 | options 00 00
    //   CDR2_LE   0x0007 -> 00 07 | options 00 00
    //   D_CDR2_BE 0x0008 -> 00 08 | options 00 00
    //   D_CDR2_LE 0x0009 -> 00 09 | options 00 00
    //   PL_CDR2_BE 0x000a -> 00 0a | options 00 00
    //   PL_CDR2_LE 0x000b -> 00 0b | options 00 00
    let expected: [(EncapsulationKind, [u8; 4]); 10] = [
        (EncapsulationKind::CdrBe, [0x00, 0x00, 0x00, 0x00]),
        (EncapsulationKind::CdrLe, [0x00, 0x01, 0x00, 0x00]),
        (EncapsulationKind::PlCdrBe, [0x00, 0x02, 0x00, 0x00]),
        (EncapsulationKind::PlCdrLe, [0x00, 0x03, 0x00, 0x00]),
        (EncapsulationKind::Cdr2Be, [0x00, 0x06, 0x00, 0x00]),
        (EncapsulationKind::Cdr2Le, [0x00, 0x07, 0x00, 0x00]),
        (EncapsulationKind::DCdr2Be, [0x00, 0x08, 0x00, 0x00]),
        (EncapsulationKind::DCdr2Le, [0x00, 0x09, 0x00, 0x00]),
        (EncapsulationKind::PlCdr2Be, [0x00, 0x0a, 0x00, 0x00]),
        (EncapsulationKind::PlCdr2Le, [0x00, 0x0b, 0x00, 0x00]),
    ];
    for (kind, octets) in expected {
        let mut writer = CdrWriter::new(Encoding::new(kind));
        writer.write_u8(0xff).expect("write");
        let produced = writer.finish();
        assert_eq!(&produced[..4], &octets, "{kind:?} header");
        assert_eq!(produced[4], 0xff, "{kind:?} body");
    }
}

#[test]
fn golden_octet_and_boolean() {
    // OMG CDR 15.3.1: `octet` and `boolean` occupy one octet and align to 1,
    // so neither can ever be preceded by padding.
    //
    //   offset 0..4  00 01 00 00   CDR_LE header
    //   position 0   2a            octet == 0x2a
    assert_eq!(
        to_vec(&0x2a_u8, Encoding::ROS2).expect("encode"),
        [0x00, 0x01, 0x00, 0x00, 0x2a]
    );

    // A `boolean` is exactly 0x00 or 0x01 — CDR names no other encoding.
    //
    //   position 0   01            true
    //   position 1   00            false
    let mut writer = CdrWriter::new(Encoding::ROS2);
    writer.write_bool(true).expect("write");
    writer.write_bool(false).expect("write");
    assert_eq!(writer.finish(), [0x00, 0x01, 0x00, 0x00, 0x01, 0x00]);

    // …and 0x02 is not a boolean.
    let hostile = [0x00, 0x01, 0x00, 0x00, 0x02];
    assert_eq!(
        from_bytes::<bool>(&hostile),
        Err(CdrError::InvalidBoolean(2))
    );
}

#[test]
fn golden_short_in_both_byte_orders() {
    // `unsigned short` 0x1234, size 2, alignment 2, at position 0.
    //
    //   CDR_LE: least significant octet first  -> 34 12
    //   CDR_BE: most significant octet first   -> 12 34
    assert_eq!(
        to_vec(&0x1234_u16, Encoding::new(EncapsulationKind::CdrLe)).expect("encode"),
        [0x00, 0x01, 0x00, 0x00, 0x34, 0x12]
    );
    assert_eq!(
        to_vec(&0x1234_u16, Encoding::new(EncapsulationKind::CdrBe)).expect("encode"),
        [0x00, 0x00, 0x00, 0x00, 0x12, 0x34]
    );
}

#[test]
fn golden_long_negative_two() {
    // `long` -2 is the two's-complement pattern 0xffff_fffe.
    //
    //   CDR_LE -> fe ff ff ff
    //   CDR_BE -> ff ff ff fe
    assert_eq!(
        to_vec_headerless(&-2_i32, Encoding::new(EncapsulationKind::CdrLe)).expect("encode"),
        [0xfe, 0xff, 0xff, 0xff]
    );
    assert_eq!(
        to_vec_headerless(&-2_i32, Encoding::new(EncapsulationKind::CdrBe)).expect("encode"),
        [0xff, 0xff, 0xff, 0xfe]
    );
}

#[test]
fn golden_double_one_point_zero() {
    // IEEE-754 binary64 for 1.0: sign 0, exponent 0x3ff, mantissa 0, i.e.
    // 0x3ff0_0000_0000_0000. CDR transmits the bit pattern; it does not
    // renormalise it.
    //
    //   CDR_LE -> 00 00 00 00 00 00 f0 3f
    //   CDR_BE -> 3f f0 00 00 00 00 00 00
    assert_eq!(
        to_vec_headerless(&1.0_f64, Encoding::new(EncapsulationKind::CdrLe)).expect("encode"),
        [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f]
    );
    assert_eq!(
        to_vec_headerless(&1.0_f64, Encoding::new(EncapsulationKind::CdrBe)).expect("encode"),
        [0x3f, 0xf0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
    );

    // binary32 for -1.0: sign 1, exponent 0x7f, mantissa 0 -> 0xbf80_0000.
    assert_eq!(
        to_vec_headerless(&-1.0_f32, Encoding::new(EncapsulationKind::CdrLe)).expect("encode"),
        [0x00, 0x00, 0x80, 0xbf]
    );
}

#[test]
fn golden_alignment_ladder_xcdr1() {
    // A struct that touches every alignment CDR has:
    //
    //   struct Ladder {
    //       octet             a;   // size 1, align 1
    //       unsigned short    b;   // size 2, align 2
    //       unsigned long     c;   // size 4, align 4
    //       unsigned long long d;  // size 8, align 8
    //   };
    //
    // position  octets                    what
    //   0       01                        a = 0x01
    //   1       00                        pad: b must start at a multiple of 2
    //   2       02 00                     b = 0x0002
    //   4       03 00 00 00               c = 0x0000_0003 (already 4-aligned)
    //   8       04 00 00 00 00 00 00 00   d = 4 (position 8 is 8-aligned)
    //
    // Body length 16, plus the four-octet header.
    let mut writer = CdrWriter::new(Encoding::ROS2);
    writer.write_u8(1).expect("write");
    writer.write_u16(2).expect("write");
    writer.write_u32(3).expect("write");
    writer.write_u64(4).expect("write");
    let octets = writer.finish();
    assert_eq!(
        octets,
        [
            0x00, 0x01, 0x00, 0x00, // CDR_LE
            0x01, // position 0: a
            0x00, // position 1: pad
            0x02, 0x00, // position 2: b
            0x03, 0x00, 0x00, 0x00, // position 4: c
            0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // position 8: d
        ]
    );
    assert_eq!(octets.len(), 4 + 16);
}

#[test]
fn golden_worst_case_padding_before_a_double() {
    // The vector that separates XCDR1 from XCDR2. Members: `octet a` then
    // `double b`.
    //
    // XCDR1 (OMG CDR 15.3.1) — a `double` aligns to 8:
    //   position 0   ff                          a
    //   position 1   00 00 00 00 00 00 00        seven pad octets
    //   position 8   00 00 00 00 00 00 f0 3f     b = 1.0
    //   body length 16
    let mut v1 = CdrWriter::new(Encoding::new(EncapsulationKind::CdrLe));
    v1.write_u8(0xff).expect("write");
    v1.write_f64(1.0).expect("write");
    assert_eq!(
        v1.finish(),
        [
            0x00, 0x01, 0x00, 0x00, //
            0xff, //
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f,
        ]
    );

    // XCDR2 (OMG DDS-XTypes 1.3 §7.4.3.4.1) — every alignment is capped at 4,
    // so the same `double` needs only three pad octets:
    //   position 0   ff                          a
    //   position 1   00 00 00                    three pad octets
    //   position 4   00 00 00 00 00 00 f0 3f     b = 1.0
    //   body length 12
    let mut v2 = CdrWriter::new(Encoding::new(EncapsulationKind::Cdr2Le));
    v2.write_u8(0xff).expect("write");
    v2.write_f64(1.0).expect("write");
    assert_eq!(
        v2.finish(),
        [
            0x00, 0x07, 0x00, 0x00, //
            0xff, //
            0x00, 0x00, 0x00, //
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f,
        ]
    );
}

#[test]
fn golden_padding_octets_are_zero_on_write_and_ignored_on_read() {
    // A sender must zero its padding. A receiver gains nothing by insisting,
    // so this decoder steps over whatever is there — which is what keeps a
    // peer with a sloppy encoder interoperable.
    let deliberate_garbage = [
        0x00, 0x01, 0x00, 0x00, // CDR_LE
        0x07, // position 0: octet
        0xde, 0xad, 0xbe, // position 1: three non-zero pad octets
        0x2a, 0x00, 0x00, 0x00, // position 4: long == 42
    ];
    let mut reader = CdrReader::new(&deliberate_garbage).expect("header");
    assert_eq!(reader.read_u8().expect("read"), 7);
    assert_eq!(reader.read_u32().expect("read"), 42);
    assert_eq!(reader.finish(), Ok(()));
}

#[test]
fn golden_xcdr2_options_padding_field() {
    // OMG DDS-XTypes 1.3 §7.6.3.1.2: the two least significant bits of the
    // options field carry the number of padding octets the sender appended so
    // the payload is a multiple of four.
    //
    //   00 07                     CDR2_LE
    //   00 03                     options: three trailing pad octets
    //   ff                        position 0: the one real octet
    //   00 00 00                  the padding those two bits announce
    let mut writer = CdrWriter::new(Encoding::new(EncapsulationKind::Cdr2Le));
    writer.write_u8(0xff).expect("write");
    let octets = writer.finish_padded();
    assert_eq!(octets, [0x00, 0x07, 0x00, 0x03, 0xff, 0x00, 0x00, 0x00]);

    // A reader that honours the field sees a one-octet body, not a
    // four-octet one, so the strict trailing-octet rule still holds.
    let mut reader = CdrReader::new(&octets).expect("header");
    assert_eq!(reader.remaining(), 1);
    assert_eq!(reader.read_u8().expect("read"), 0xff);
    assert_eq!(reader.finish(), Ok(()));
}

#[test]
fn golden_every_primitive_at_position_zero() {
    // A table of one-member vectors, each at stream position 0 so no padding
    // enters the picture and the octets are the type's raw representation.
    let cases: [(&str, Vec<u8>, Vec<u8>); 10] = [
        ("boolean true", to_le(&true), vec![0x01]),
        ("octet 0xab", to_le(&0xab_u8), vec![0xab]),
        ("int8 -1", to_le(&-1_i8), vec![0xff]),
        ("uint16 0xbeef", to_le(&0xbeef_u16), vec![0xef, 0xbe]),
        ("int16 -2", to_le(&-2_i16), vec![0xfe, 0xff]),
        (
            "uint32 0xdeadbeef",
            to_le(&0xdead_beef_u32),
            vec![0xef, 0xbe, 0xad, 0xde],
        ),
        ("int32 -3", to_le(&-3_i32), vec![0xfd, 0xff, 0xff, 0xff]),
        (
            "uint64 0x0102030405060708",
            to_le(&0x0102_0304_0506_0708_u64),
            vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01],
        ),
        // binary32 2.0 = sign 0, exponent 0x80, mantissa 0 -> 0x4000_0000.
        ("float 2.0", to_le(&2.0_f32), vec![0x00, 0x00, 0x00, 0x40]),
        // binary64 -0.5 = sign 1, exponent 0x3fe, mantissa 0 -> 0xbfe0…0.
        (
            "double -0.5",
            to_le(&-0.5_f64),
            vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xe0, 0xbf],
        ),
    ];
    for (name, produced, expected) in cases {
        assert_eq!(produced, expected, "{name}");
    }
}

fn to_le<T: astrs_cdr::CdrSerialize + ?Sized>(value: &T) -> Vec<u8> {
    to_vec_headerless(value, Encoding::new(EncapsulationKind::CdrLe)).expect("encode")
}

#[test]
fn golden_big_endian_body_is_the_reverse_of_the_little_endian_one() {
    // The identifier's low bit is the only thing that differs between a BE
    // and an LE encoding of the same value, and the body is byte-reversed
    // member by member — not as a whole, which is why the two `unsigned
    // short`s here reverse independently.
    let mut le = CdrWriter::new(Encoding::new(EncapsulationKind::CdrLe));
    le.write_u16(0x1122).expect("write");
    le.write_u16(0x3344).expect("write");
    let mut be = CdrWriter::new(Encoding::new(EncapsulationKind::CdrBe));
    be.write_u16(0x1122).expect("write");
    be.write_u16(0x3344).expect("write");

    assert_eq!(
        le.finish(),
        [0x00, 0x01, 0x00, 0x00, 0x22, 0x11, 0x44, 0x33]
    );
    assert_eq!(
        be.finish(),
        [0x00, 0x00, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44]
    );
}

#[test]
fn golden_appending_a_payload_into_a_larger_buffer_restarts_the_origin() {
    // RTPS builds a `DATA` submessage by appending the serialized payload
    // after headers that are already in the buffer. CDR alignment is counted
    // from the payload's own origin, not from the front of the datagram, so
    // the `double` below lands at payload position 0 with no padding even
    // though it sits at buffer offset 9.
    let mut writer = CdrWriter::append_to(vec![0xaa; 5], Encoding::ROS2);
    writer.write_f64(1.0).expect("write");
    let octets = writer.finish();
    assert_eq!(
        octets,
        [
            0xaa, 0xaa, 0xaa, 0xaa, 0xaa, // pretend RTPS headers
            0x00, 0x01, 0x00, 0x00, // the payload's own CDR_LE header
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f, // 1.0, unpadded
        ]
    );
    assert_eq!(octets.len(), 5 + 4 + 8);
}

#[test]
fn golden_trailing_octets_are_refused() {
    // A `long` payload with one octet too many. The value decodes, and the
    // reader still refuses the sample: a payload the declared type does not
    // consume is a type disagreement, not a curiosity.
    let extra = [0x00, 0x01, 0x00, 0x00, 0x2a, 0x00, 0x00, 0x00, 0xff];
    assert_eq!(
        from_bytes::<u32>(&extra),
        Err(CdrError::TrailingBytes { remaining: 1 })
    );

    // The same octets with the tail removed decode cleanly.
    assert_eq!(from_bytes::<u32>(&extra[..8]).expect("decode"), 42);
}

#[test]
fn golden_truncated_payloads_are_refused_at_every_width() {
    // One octet short, for each primitive width. The reported `needed` is the
    // width itself once alignment has been satisfied.
    let head = header(EncapsulationKind::CdrLe);
    for (width, name) in [(2_usize, "u16"), (4, "u32"), (8, "u64")] {
        let mut bytes = head.to_vec();
        bytes.resize(4 + width - 1, 0);
        let error = match width {
            2 => from_bytes::<u16>(&bytes).map(|_| ()),
            4 => from_bytes::<u32>(&bytes).map(|_| ()),
            _ => from_bytes::<u64>(&bytes).map(|_| ()),
        };
        assert_eq!(
            error,
            Err(CdrError::Truncated {
                needed: width,
                available: width - 1,
                context: name,
            }),
            "{name}"
        );
    }
}
