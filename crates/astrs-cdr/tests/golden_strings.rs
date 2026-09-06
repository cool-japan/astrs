//! Golden vectors: IDL `string` and `wstring`.
//!
//! The two types are encoded by different rules, and mixing them up is the
//! classic CDR interoperability bug. Each vector below states which rule it
//! demonstrates and derives the octets from it.
//!
//! **`string`** — OMG CDR (CORBA 3.0 §15.3.2.6). An `unsigned long` giving
//! the length of the value **including** its terminating NUL, then the
//! octets, then the NUL. The empty string is length `1` and one NUL octet;
//! a length of `0` is malformed.
//!
//! **`wstring`** — OMG DDS-XTypes 1.3 §7.4.3.5.1. An `unsigned long` giving
//! the number of `wchar` **elements**, then the elements, and **no**
//! terminator. `wchar` is two octets — a UTF-16 code unit — with alignment 2
//! (Table 41). The empty `wstring` is length `0` and nothing else.
//!
//! Both length prefixes are `unsigned long`s and therefore align to four.
//!
//! # A note on the `wstring` element width
//!
//! Two octets per `wchar` is what the specification assigns, and it is what
//! every vector here encodes. Some C++ stacks historically serialized a
//! 32-bit `wchar_t`, producing four octets per element; this crate can be
//! configured for that
//! ([`WCharWidth::Four`](astrs_cdr::WCharWidth::Four)) and the round trip is
//! tested, but that mode gets **no golden vector**, because no specification
//! text supports it and labelling it "spec-derived" would be false.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_cdr::{
    CdrError, CdrWriter, EncapsulationKind, Encoding, WString, from_bytes, to_vec,
    to_vec_headerless,
};

fn le() -> Encoding {
    Encoding::new(EncapsulationKind::CdrLe)
}

fn be() -> Encoding {
    Encoding::new(EncapsulationKind::CdrBe)
}

#[test]
fn golden_string_hello() {
    // "hello" is five octets of UTF-8, so the length is 5 + 1 = 6.
    //
    //   position 0   06 00 00 00   unsigned long 6 (five octets plus the NUL)
    //   position 4   68            'h'
    //   position 5   65            'e'
    //   position 6   6c            'l'
    //   position 7   6c            'l'
    //   position 8   6f            'o'
    //   position 9   00            the terminator the length counted
    //
    // Body length 10.
    assert_eq!(
        to_vec_headerless(&"hello".to_owned(), le()).expect("encode"),
        [0x06, 0x00, 0x00, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x00]
    );
}

#[test]
fn golden_string_empty_is_length_one() {
    // The trap: the empty string is *not* four zero octets. Its length counts
    // the terminator, so it is 1.
    //
    //   position 0   01 00 00 00   unsigned long 1
    //   position 4   00            the terminator, and the whole value
    assert_eq!(
        to_vec_headerless(&String::new(), le()).expect("encode"),
        [0x01, 0x00, 0x00, 0x00, 0x00]
    );

    // A declared length of zero has no terminator to point at, so it is
    // refused rather than read as the empty string.
    let malformed = [0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    assert_eq!(
        from_bytes::<String>(&malformed),
        Err(CdrError::MissingNulTerminator)
    );
}

#[test]
fn golden_string_big_endian() {
    // Only the length prefix changes byte order; the octets of the value are
    // octets and have no order of their own.
    //
    //   position 0   00 00 00 03   unsigned long 3, big-endian
    //   position 4   68 69 00      "hi" and its terminator
    assert_eq!(
        to_vec_headerless(&"hi".to_owned(), be()).expect("encode"),
        [0x00, 0x00, 0x00, 0x03, 0x68, 0x69, 0x00]
    );
}

#[test]
fn golden_string_length_counts_octets_not_characters() {
    // "é" is U+00E9, two octets in UTF-8: c3 a9. The length is therefore
    // 2 + 1 = 3, not 1 + 1 = 2 — CDR counts octets.
    //
    //   position 0   03 00 00 00
    //   position 4   c3 a9 00
    assert_eq!(
        to_vec_headerless(&"é".to_owned(), le()).expect("encode"),
        [0x03, 0x00, 0x00, 0x00, 0xc3, 0xa9, 0x00]
    );

    // "日" is U+65E5, three octets: e6 97 a5. Length 4.
    assert_eq!(
        to_vec_headerless(&"日".to_owned(), le()).expect("encode"),
        [0x04, 0x00, 0x00, 0x00, 0xe6, 0x97, 0xa5, 0x00]
    );
}

#[test]
fn golden_string_after_an_octet_pads_to_four() {
    // The length prefix is an `unsigned long`, so it aligns to four however
    // odd the position before it was.
    //
    //   position 0   07            octet
    //   position 1   00 00 00      three pad octets
    //   position 4   04 00 00 00   length 4
    //   position 8   6d 61 70 00   "map" and its terminator
    //
    // Body length 12.
    let mut writer = CdrWriter::new(le());
    writer.write_u8(7).expect("write");
    writer.write_str("map").expect("write");
    assert_eq!(
        writer.finish(),
        [
            0x00, 0x01, 0x00, 0x00, //
            0x07, //
            0x00, 0x00, 0x00, //
            0x04, 0x00, 0x00, 0x00, //
            0x6d, 0x61, 0x70, 0x00,
        ]
    );
}

#[test]
fn golden_two_strings_in_a_row() {
    // The second string's length prefix re-aligns to four after the first
    // string's odd tail.
    //
    //   position 0   03 00 00 00   length 3
    //   position 4   61 62 00      "ab" and its terminator
    //   position 7   00            pad: the next length must be 4-aligned
    //   position 8   02 00 00 00   length 2
    //   position 12  63 00         "c" and its terminator
    //
    // Body length 14.
    let mut writer = CdrWriter::new(le());
    writer.write_str("ab").expect("write");
    writer.write_str("c").expect("write");
    assert_eq!(
        writer.finish(),
        [
            0x00, 0x01, 0x00, 0x00, //
            0x03, 0x00, 0x00, 0x00, //
            0x61, 0x62, 0x00, //
            0x00, //
            0x02, 0x00, 0x00, 0x00, //
            0x63, 0x00,
        ]
    );
}

#[test]
fn golden_wstring_counts_elements_and_omits_the_terminator() {
    // "hi" is two UTF-16 code units: U+0068 and U+0069.
    //
    //   position 0   02 00 00 00   unsigned long 2 — *elements*, not octets
    //   position 4   68 00         U+0068, two octets, little-endian
    //   position 6   69 00         U+0069
    //
    // Body length 8. Note what is absent: no NUL, and no count of octets.
    assert_eq!(
        to_vec_headerless(&WString::from("hi"), le()).expect("encode"),
        [0x02, 0x00, 0x00, 0x00, 0x68, 0x00, 0x69, 0x00]
    );

    // Big-endian: only the two-octet fields swap.
    //
    //   position 0   00 00 00 02
    //   position 4   00 68 00 69
    assert_eq!(
        to_vec_headerless(&WString::from("hi"), be()).expect("encode"),
        [0x00, 0x00, 0x00, 0x02, 0x00, 0x68, 0x00, 0x69]
    );
}

#[test]
fn golden_wstring_empty_is_length_zero() {
    // The mirror image of the `string` trap: with no terminator to count, the
    // empty `wstring` really is four zero octets.
    assert_eq!(
        to_vec_headerless(&WString::new(), le()).expect("encode"),
        [0x00, 0x00, 0x00, 0x00]
    );
}

#[test]
fn golden_wstring_astral_plane_costs_two_elements() {
    // U+1F680 ROCKET is outside the BMP, so UTF-16 encodes it as the
    // surrogate pair D83D DE80. The length prefix counts code units, so it is
    // 2 for one character.
    //
    //   position 0   02 00 00 00   two code units
    //   position 4   3d d8         high surrogate D83D, little-endian
    //   position 6   80 de         low surrogate DE80
    let value = WString::from("\u{1f680}");
    assert_eq!(value.len(), 2);
    assert_eq!(
        to_vec_headerless(&value, le()).expect("encode"),
        [0x02, 0x00, 0x00, 0x00, 0x3d, 0xd8, 0x80, 0xde]
    );
}

#[test]
fn golden_wstring_after_an_octet_pads_the_length_to_four() {
    //   position 0   09            octet
    //   position 1   00 00 00      three pad octets
    //   position 4   01 00 00 00   one code unit
    //   position 8   41 00         U+0041 'A'
    let mut writer = CdrWriter::new(le());
    writer.write_u8(9).expect("write");
    writer.write_wstr(&[0x0041]).expect("write");
    assert_eq!(
        writer.finish(),
        [
            0x00, 0x01, 0x00, 0x00, //
            0x09, 0x00, 0x00, 0x00, //
            0x01, 0x00, 0x00, 0x00, //
            0x41, 0x00,
        ]
    );
}

#[test]
fn golden_string_refuses_an_interior_nul_both_ways() {
    // OMG CDR strings are C strings: the terminator is the only NUL. A Rust
    // `String` may hold others, so encoding checks…
    let mut writer = CdrWriter::new(le());
    assert_eq!(
        writer.write_str("a\0b"),
        Err(CdrError::InteriorNul { index: 1 })
    );

    // …and so does decoding, which keeps decode-then-encode total.
    //
    //   04 00 00 00   length 4
    //   61 00 62 00   "a", a NUL, "b", the real terminator
    let hostile = [
        0x00, 0x01, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x61, 0x00, 0x62, 0x00,
    ];
    assert_eq!(
        from_bytes::<String>(&hostile),
        Err(CdrError::InteriorNul { index: 1 })
    );
}

#[test]
fn golden_string_requires_its_terminator_to_be_present() {
    // A length of 3 promises the third octet is NUL. Here it is '!'.
    let hostile = [
        0x00, 0x01, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x68, 0x69, 0x21,
    ];
    assert_eq!(
        from_bytes::<String>(&hostile),
        Err(CdrError::MissingNulTerminator)
    );
}

#[test]
fn golden_string_body_must_be_utf8() {
    // ROS 2 defines `string` as UTF-8, so 0xff 0xfe is a content error rather
    // than an invitation to decode lossily.
    let hostile = [
        0x00, 0x01, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0xff, 0xfe, 0x00,
    ];
    assert!(matches!(
        from_bytes::<String>(&hostile),
        Err(CdrError::InvalidUtf8(_))
    ));
}

#[test]
fn golden_hostile_string_length_is_refused_before_allocation() {
    // 0xffff_ffff octets are promised and one is delivered. The check is a
    // comparison against the octets that remain, so nothing is allocated.
    let hostile = [0x00, 0x01, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0x78];
    assert_eq!(
        from_bytes::<String>(&hostile),
        Err(CdrError::LengthOverflow {
            declared: 0xffff_ffff,
            available: 1,
            element_size: 1,
            context: "string",
        })
    );

    // The same for a `wstring`, where the element size is two.
    assert_eq!(
        from_bytes::<WString>(&hostile),
        Err(CdrError::LengthOverflow {
            declared: 0xffff_ffff,
            available: 1,
            element_size: 2,
            context: "wstring",
        })
    );
}

#[test]
fn golden_strings_are_borrowed_from_the_input() {
    // The zero-copy accessor `astrs-idl` codegen targets: the `&str` points
    // into the caller's buffer, six octets past the encapsulation header and
    // the length prefix.
    let bytes = to_vec(&"/scan".to_owned(), le()).expect("encode");
    let borrowed: &str = from_bytes(&bytes).expect("decode");
    assert_eq!(borrowed, "/scan");
    assert!(core::ptr::eq(borrowed.as_ptr(), bytes[8..].as_ptr()));
}

#[test]
fn golden_wstring_preserves_an_unpaired_surrogate() {
    // 0xd800 with no low surrogate is not valid UTF-16, but it is valid on
    // the wire. Decoding it into a `String` would either fail or silently
    // substitute U+FFFD, so `WString` keeps the code units and the round trip
    // stays exact.
    let value = WString::from_units(vec![0x0041, 0xd800]);
    let bytes = to_vec(&value, le()).expect("encode");
    assert_eq!(
        bytes,
        [
            0x00, 0x01, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x41, 0x00, 0x00, 0xd8
        ]
    );
    assert_eq!(from_bytes::<WString>(&bytes).expect("decode"), value);
    assert_eq!(
        value.try_to_string(),
        Err(CdrError::InvalidUtf16 { index: 1 })
    );
    assert_eq!(value.to_string_lossy(), "A\u{fffd}");
}
