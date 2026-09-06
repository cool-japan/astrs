//! Golden vectors: `PL_CDR` parameter lists, in the shapes RTPS discovery
//! uses.
//!
//! A `PL_CDR` body is a run of `(parameterId, parameterLength, value)` entries
//! terminated by `PID_SENTINEL` (OMG DDSI-RTPS 2.3 §9.4.2.11). Both header
//! fields are `unsigned short` in the stream's byte order, and
//! `parameterLength` is a multiple of four so the next entry lands on a
//! four-octet boundary.
//!
//! # What these vectors do and do not assert
//!
//! They assert the **framing**: identifiers, lengths, padding, the sentinel,
//! and the alignment origin each value gets. They deliberately do *not*
//! assert the internal layout of a QoS policy value — whether
//! `ReliabilityQosPolicyKind` is one-based on the wire is a question about
//! RTPS, and it belongs to `astrs-rtps` together with the rest of the
//! discovery data model. This crate carries the parameter *list*; the
//! parameters' meanings live one layer up.
//!
//! Every octet below is derived from the specification, not captured from a
//! C or C++ DDS stack (blueprint §18).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_cdr::{
    CdrError, EncapsulationKind, Encoding, Parameter, ParameterId, ParameterList, pid,
};

fn discovery_le() -> Encoding {
    Encoding::new(EncapsulationKind::PlCdrLe)
}

fn discovery_be() -> Encoding {
    Encoding::new(EncapsulationKind::PlCdrBe)
}

#[test]
fn golden_empty_parameter_list_is_just_a_sentinel() {
    // Even with nothing to say, a parameter list ends with the sentinel: a
    // reader has no other way to know where the body stops.
    //
    //   offset 0..4  00 03 00 00   PL_CDR_LE
    //   position 0   01 00         PID_SENTINEL
    //   position 2   00 00         parameterLength = 0
    let list = ParameterList::new(discovery_le());
    assert_eq!(
        list.encode().expect("encode"),
        [0x00, 0x03, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00]
    );

    // Big-endian: only the two `unsigned short`s swap.
    let list = ParameterList::new(discovery_be());
    assert_eq!(
        list.encode().expect("encode"),
        [0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00]
    );
}

#[test]
fn golden_spdp_shaped_participant_announcement() {
    // The framing of an SPDP `SPDPdiscoveredParticipantData` sample
    // (OMG DDSI-RTPS 2.3 §8.5.3.2). Field widths come from the RTPS
    // structure definitions:
    //
    //   ProtocolVersion_t     octet major, octet minor            2 octets
    //   VendorId_t            octet[2]                            2 octets
    //   GUID_t                octet[12] prefix + octet[4] entity 16 octets
    //   Duration_t            long seconds, unsigned long fraction 8 octets
    //   BuiltinEndpointSet_t  unsigned long                        4 octets
    //
    // …and each is rounded up to a multiple of four by `parameterLength`.
    //
    // offset 0..4   00 03 00 00                PL_CDR_LE
    //
    // position 0    15 00                      PID_PROTOCOL_VERSION
    // position 2    04 00                      length 4 (2 octets, padded)
    // position 4    02 03 00 00                RTPS 2.3, then two pad octets
    //
    // position 8    16 00                      PID_VENDORID
    // position 10   04 00                      length 4 (2 octets, padded)
    // position 12   01 20 00 00                a placeholder vendor id
    //
    // position 16   50 00                      PID_PARTICIPANT_GUID
    // position 18   10 00                      length 16, already a multiple
    // position 20   …12 prefix octets…         the participant's GUID prefix
    // position 32   00 00 01 c1                ENTITYID_PARTICIPANT
    //
    // position 36   02 00                      PID_PARTICIPANT_LEASE_DURATION
    // position 38   08 00                      length 8
    // position 40   0c 00 00 00                seconds = 12
    // position 44   00 00 00 00                fraction = 0
    //
    // position 48   58 00                      PID_BUILTIN_ENDPOINT_SET
    // position 50   04 00                      length 4
    // position 52   3f 00 00 00                a bitmask of builtin endpoints
    //
    // position 56   01 00 00 00                PID_SENTINEL
    //
    // Body length 60, total 64.
    let guid_prefix: [u8; 12] = [
        0x01, 0x0f, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa,
    ];
    let mut participant_guid = guid_prefix.to_vec();
    participant_guid.extend_from_slice(&[0x00, 0x00, 0x01, 0xc1]);

    let mut list = ParameterList::new(discovery_le());
    list.push_octets(ParameterId::new(pid::PROTOCOL_VERSION), vec![0x02, 0x03])
        .expect("short enough");
    list.push_octets(ParameterId::new(pid::VENDORID), vec![0x01, 0x20])
        .expect("short enough");
    list.push_octets(ParameterId::new(pid::PARTICIPANT_GUID), participant_guid)
        .expect("short enough");
    list.push_octets(
        ParameterId::new(pid::PARTICIPANT_LEASE_DURATION),
        vec![0x0c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
    )
    .expect("short enough");
    list.push_octets(
        ParameterId::new(pid::BUILTIN_ENDPOINT_SET),
        vec![0x3f, 0x00, 0x00, 0x00],
    )
    .expect("short enough");

    let bytes = list.encode().expect("encode");
    assert_eq!(
        bytes,
        [
            0x00, 0x03, 0x00, 0x00, //
            0x15, 0x00, 0x04, 0x00, 0x02, 0x03, 0x00, 0x00, //
            0x16, 0x00, 0x04, 0x00, 0x01, 0x20, 0x00, 0x00, //
            0x50, 0x00, 0x10, 0x00, //
            0x01, 0x0f, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, //
            0x77, 0x88, 0x99, 0xaa, 0x00, 0x00, 0x01, 0xc1, //
            0x02, 0x00, 0x08, 0x00, //
            0x0c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
            0x58, 0x00, 0x04, 0x00, 0x3f, 0x00, 0x00, 0x00, //
            0x01, 0x00, 0x00, 0x00,
        ]
    );
    assert_eq!(bytes.len(), 64);
    assert_eq!(list.serialized_len(), 60);

    // Decoding is the exact inverse, and every value is borrowed from
    // `bytes` rather than copied.
    let (decoded, encoding) = ParameterList::decode(&bytes).expect("decode");
    assert_eq!(encoding.kind(), EncapsulationKind::PlCdrLe);
    assert_eq!(decoded, list);
    assert_eq!(decoded.encode().expect("re-encode"), bytes);
    assert_eq!(decoded.len(), 5);
    assert_eq!(
        decoded
            .get_by_base(pid::PARTICIPANT_GUID)
            .expect("guid")
            .value
            .len(),
        16
    );
}

#[test]
fn golden_sedp_topic_and_type_names() {
    // A SEDP publication announcement carries the topic and type names as
    // CDR `string`s inside parameter values. The two lengths in play are
    // easy to conflate, so this vector spells both out.
    //
    // "rt/chatter" is ten octets, so the *string's* length prefix is 11
    // (ten plus its NUL). The value is 4 + 11 = 15 octets, which is not a
    // multiple of four, so `parameterLength` is 16 and one pad octet
    // follows the terminator.
    //
    // position 0    05 00                      PID_TOPIC_NAME
    // position 2    10 00                      parameterLength = 16
    // position 4    0b 00 00 00                the string's length = 11
    // position 8    72 74 2f 63 68 61 74 74    "rt/chatt"
    // position 16   65 72                      "er"
    // position 18   00                         the string's terminator
    // position 19   00                         the parameter's pad octet
    let mut list = ParameterList::new(discovery_le());
    list.push_value(ParameterId::new(pid::TOPIC_NAME), &"rt/chatter".to_owned())
        .expect("encode value");

    let bytes = list.encode().expect("encode");
    assert_eq!(
        bytes,
        [
            0x00, 0x03, 0x00, 0x00, //
            0x05, 0x00, 0x10, 0x00, //
            0x0b, 0x00, 0x00, 0x00, //
            0x72, 0x74, 0x2f, 0x63, 0x68, 0x61, 0x74, 0x74, //
            0x65, 0x72, 0x00, 0x00, //
            0x01, 0x00, 0x00, 0x00,
        ]
    );

    // Reading the value back: the pad octet is forgiven, because the value
    // carries it and the string does not.
    let (decoded, _) = ParameterList::decode(&bytes).expect("decode");
    let topic: &str = decoded
        .get_by_base(pid::TOPIC_NAME)
        .expect("topic name")
        .decode_value(discovery_le())
        .expect("decode value");
    assert_eq!(topic, "rt/chatter");
}

#[test]
fn golden_sedp_type_name_with_three_pad_octets() {
    // "std_msgs::msg::dds_::String_" is 28 octets, so the string's length is
    // 29 and the value is 4 + 29 = 33 octets. `parameterLength` rounds that
    // to 36, adding three pad octets after the terminator.
    //
    // position 0    07 00                      PID_TYPE_NAME
    // position 2    24 00                      parameterLength = 36
    // position 4    1d 00 00 00                the string's length = 29
    // position 8    …28 octets of name…
    // position 36   00                         the terminator
    // position 37   00 00 00                   three pad octets
    let mut list = ParameterList::new(discovery_le());
    list.push_value(
        ParameterId::new(pid::TYPE_NAME),
        &"std_msgs::msg::dds_::String_".to_owned(),
    )
    .expect("encode value");

    let bytes = list.encode().expect("encode");
    assert_eq!(
        bytes,
        [
            0x00, 0x03, 0x00, 0x00, // PL_CDR_LE
            0x07, 0x00, 0x24, 0x00, // PID_TYPE_NAME, length 36
            0x1d, 0x00, 0x00, 0x00, // string length 29
            0x73, 0x74, 0x64, 0x5f, // "std_"
            0x6d, 0x73, 0x67, 0x73, // "msgs"
            0x3a, 0x3a, 0x6d, 0x73, // "::ms"
            0x67, 0x3a, 0x3a, 0x64, // "g::d"
            0x64, 0x73, 0x5f, 0x3a, // "ds_:"
            0x3a, 0x53, 0x74, 0x72, // ":Str"
            0x69, 0x6e, 0x67, 0x5f, // "ing_"
            0x00, 0x00, 0x00, 0x00, // terminator plus three pad octets
            0x01, 0x00, 0x00, 0x00, // PID_SENTINEL
        ]
    );

    let (decoded, _) = ParameterList::decode(&bytes).expect("decode");
    let type_name: &str = decoded
        .get_by_base(pid::TYPE_NAME)
        .expect("type name")
        .decode_value(discovery_le())
        .expect("decode value");
    assert_eq!(type_name, "std_msgs::msg::dds_::String_");
}

#[test]
fn golden_parameter_value_gets_a_fresh_alignment_origin() {
    // Each value is a CDR stream in its own right. A `double` inside a
    // parameter therefore starts at *value* position 0 and needs no padding,
    // even though it sits at body position 4 — which is not a multiple of
    // eight.
    //
    // position 0    2c 00                      PID_USER_DATA
    // position 2    08 00                      parameterLength = 8
    // position 4    00 00 00 00 00 00 f0 3f    1.0, unpadded
    let mut list = ParameterList::new(discovery_le());
    list.push_value(ParameterId::new(pid::USER_DATA), &1.0_f64)
        .expect("encode value");
    let bytes = list.encode().expect("encode");
    assert_eq!(
        bytes,
        [
            0x00, 0x03, 0x00, 0x00, //
            0x2c, 0x00, 0x08, 0x00, //
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf0, 0x3f, //
            0x01, 0x00, 0x00, 0x00,
        ]
    );

    let (decoded, _) = ParameterList::decode(&bytes).expect("decode");
    let value: f64 = decoded
        .get_by_base(pid::USER_DATA)
        .expect("user data")
        .decode_value(discovery_le())
        .expect("decode value");
    assert_eq!(value, 1.0);

    // Because every entry starts on a four-octet boundary, the two readings
    // of "position" agree for every alignment up to four; only an eight-octet
    // member could tell them apart, and this is that member.
}

#[test]
fn golden_big_endian_parameter_list() {
    // The identifier says PL_CDR_BE, so the `unsigned short`s of every entry
    // header are big-endian too.
    //
    // offset 0..4   00 02 00 00   PL_CDR_BE
    // position 0    00 15         PID_PROTOCOL_VERSION
    // position 2    00 04         parameterLength = 4
    // position 4    02 03 00 00   the value's octets are octets, unswapped
    // position 8    00 01 00 00   PID_SENTINEL
    let mut list = ParameterList::new(discovery_be());
    list.push_octets(ParameterId::new(pid::PROTOCOL_VERSION), vec![0x02, 0x03])
        .expect("short enough");
    let bytes = list.encode().expect("encode");
    assert_eq!(
        bytes,
        [
            0x00, 0x02, 0x00, 0x00, //
            0x00, 0x15, 0x00, 0x04, //
            0x02, 0x03, 0x00, 0x00, //
            0x00, 0x01, 0x00, 0x00,
        ]
    );

    let (decoded, encoding) = ParameterList::decode(&bytes).expect("decode");
    assert_eq!(encoding.kind(), EncapsulationKind::PlCdrBe);
    assert_eq!(decoded, list);
}

#[test]
fn golden_pad_entries_are_skipped() {
    // `PID_PAD` is filler: a sender uses it to reserve space it later did not
    // need. It carries no information, so it is dropped on decode and does
    // not reappear on re-encode.
    //
    // position 0    00 00 04 00   PID_PAD, length 4
    // position 4    de ad be ef   …whose value means nothing
    // position 8    58 00 04 00   PID_BUILTIN_ENDPOINT_SET, length 4
    // position 12   3f 00 00 00
    // position 16   01 00 00 00   PID_SENTINEL
    let bytes = [
        0x00, 0x03, 0x00, 0x00, //
        0x00, 0x00, 0x04, 0x00, //
        0xde, 0xad, 0xbe, 0xef, //
        0x58, 0x00, 0x04, 0x00, //
        0x3f, 0x00, 0x00, 0x00, //
        0x01, 0x00, 0x00, 0x00,
    ];
    let (list, _) = ParameterList::decode(&bytes).expect("decode");
    assert_eq!(list.len(), 1);
    assert_eq!(
        list.encode().expect("re-encode"),
        [
            0x00, 0x03, 0x00, 0x00, //
            0x58, 0x00, 0x04, 0x00, //
            0x3f, 0x00, 0x00, 0x00, //
            0x01, 0x00, 0x00, 0x00,
        ]
    );
}

#[test]
fn golden_unknown_parameters_survive_a_round_trip() {
    // The property `astrs-rtps` depends on when it forwards a vendor's
    // discovery sample: an id this crate has never heard of is carried
    // through unchanged, value and all.
    let bytes = [
        0x00, 0x03, 0x00, 0x00, //
        0x34, 0x12, 0x08, 0x00, // id 0x1234, length 8
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, //
        0x01, 0x00, 0x00, 0x00, // PID_SENTINEL
    ];
    let (list, _) = ParameterList::decode(&bytes).expect("decode");
    let entry = list.as_slice().first().expect("one entry");
    assert_eq!(entry.id.raw(), 0x1234);
    assert!(entry.id.name().is_none(), "not a standard id");
    assert_eq!(entry.value.as_ref(), &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(list.encode().expect("re-encode"), bytes);
}

#[test]
fn golden_a_missing_sentinel_is_fatal() {
    // A list that simply stops is not an empty list: the reader cannot know
    // whether the sender meant to send more, so the sample is refused.
    let truncated = [
        0x00, 0x03, 0x00, 0x00, //
        0x58, 0x00, 0x04, 0x00, //
        0x3f, 0x00, 0x00, 0x00,
    ];
    assert_eq!(
        ParameterList::decode(&truncated).map(|_| ()),
        Err(CdrError::MissingSentinel)
    );

    // …and neither is a list that ends mid-header.
    let mid_header = [0x00, 0x03, 0x00, 0x00, 0x58, 0x00];
    assert_eq!(
        ParameterList::decode(&mid_header).map(|_| ()),
        Err(CdrError::MissingSentinel)
    );
}

#[test]
fn golden_a_length_past_the_end_is_refused_before_allocation() {
    // A crafted `parameterLength` of 0xff00 with eight octets present.
    let hostile = [
        0x00, 0x03, 0x00, 0x00, //
        0x2c, 0x00, 0x00, 0xff, // PID_USER_DATA, length 0xff00
        0x01, 0x02, 0x03, 0x04,
    ];
    assert_eq!(
        ParameterList::decode(&hostile).map(|_| ()),
        Err(CdrError::LengthOverflow {
            declared: 0xff00,
            available: 4,
            element_size: 1,
            context: "parameter value",
        })
    );
}

#[test]
fn golden_extended_parameter_is_refused_rather_than_guessed_at() {
    // `PID_EXTENDED` introduces a long-form entry whose layout this
    // repository has no C/C++-free source for. Guessing would produce
    // plausible-looking garbage; refusing produces a log line that names the
    // gap.
    let bytes = [
        0x00, 0x03, 0x00, 0x00, //
        0x01, 0x3f, 0x08, 0x00, // PID_EXTENDED, length 8
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        0x01, 0x00, 0x00, 0x00,
    ];
    assert_eq!(
        ParameterList::decode(&bytes).map(|_| ()),
        Err(CdrError::UnsupportedParameter { id: 0x3f01 })
    );
}

#[test]
fn golden_inline_qos_is_a_headerless_parameter_list() {
    // RTPS carries inline QoS on a `DATA` submessage as a bare parameter
    // list: no encapsulation header, because the submessage flags already
    // fixed the byte order.
    //
    // position 0    71 00 04 00   PID_STATUS_INFO, length 4
    // position 4    00 00 00 03   the four status octets (unregistered +
    //                             disposed), written most-significant first
    //                             as RTPS defines them
    // position 8    01 00 00 00   PID_SENTINEL
    let bytes = [
        0x71, 0x00, 0x04, 0x00, //
        0x00, 0x00, 0x00, 0x03, //
        0x01, 0x00, 0x00, 0x00,
    ];
    let list = ParameterList::decode_headerless(&bytes, discovery_le()).expect("decode");
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
fn golden_must_understand_flag_does_not_change_the_framing() {
    // OMG DDSI-RTPS 2.3 §9.6.2.2.1 puts two flags in the top bits of a
    // parameter id: bit 15 marks it vendor-specific, bit 14 marks it
    // must-understand. Neither changes a single octet of framing — they are
    // part of the id, and travel with it.
    //
    // PID_TOPIC_NAME | 0x4000 = 0x4005, little-endian `05 40`.
    let flagged = ParameterId::new(pid::TOPIC_NAME).with_must_understand();
    assert_eq!(flagged.raw(), 0x4005);
    assert!(flagged.must_understand());
    assert!(!flagged.is_vendor_specific());
    assert_eq!(flagged.base(), pid::TOPIC_NAME);

    let mut list = ParameterList::new(discovery_le());
    list.push_octets(flagged, vec![0x00, 0x00, 0x00, 0x00])
        .expect("short enough");
    assert_eq!(
        list.encode().expect("encode"),
        [
            0x00, 0x03, 0x00, 0x00, //
            0x05, 0x40, 0x04, 0x00, //
            0x00, 0x00, 0x00, 0x00, //
            0x01, 0x00, 0x00, 0x00,
        ]
    );

    // A vendor-specific id sets bit 15 instead: 0x8005 -> `05 80`.
    let vendor = ParameterId::new(0x8005);
    assert!(vendor.is_vendor_specific());
    assert!(!vendor.must_understand());
}

#[test]
fn golden_parameter_values_are_padded_on_construction() {
    // RTPS requires `parameterLength` to be a multiple of four, so a value
    // that is not gets padded when the parameter is built — never at encode
    // time, where a caller could not see it happen.
    let parameter =
        Parameter::new(ParameterId::new(pid::ENTITY_NAME), vec![0x01, 0x02, 0x03]).expect("ok");
    assert_eq!(parameter.value.as_ref(), &[0x01, 0x02, 0x03, 0x00]);
    assert_eq!(parameter.declared_len(), 4);
    assert_eq!(parameter.serialized_len(), 8);

    // A value that already fits stays exactly as it was, and stays borrowed.
    let already: &[u8] = &[0x01, 0x02, 0x03, 0x04];
    let parameter = Parameter::new(ParameterId::new(pid::ENTITY_NAME), already).expect("ok");
    assert_eq!(parameter.value.as_ref(), already);
    assert!(core::ptr::eq(parameter.value.as_ptr(), already.as_ptr()));
}

#[test]
fn golden_a_value_too_long_for_the_length_field_is_refused() {
    // `parameterLength` is an `unsigned short`, so 65 536 octets cannot be
    // described however the caller pads them.
    assert_eq!(
        Parameter::new(ParameterId::new(pid::USER_DATA), vec![0_u8; 65_536]).map(|_| ()),
        Err(CdrError::ParameterTooLong {
            id: pid::USER_DATA,
            length: 65_536,
        })
    );

    // 65 533 octets pad to 65 536 and are refused for the same reason;
    // 65 532 fit exactly.
    assert!(Parameter::new(ParameterId::new(pid::USER_DATA), vec![0_u8; 65_533]).is_err());
    assert!(Parameter::new(ParameterId::new(pid::USER_DATA), vec![0_u8; 65_532]).is_ok());
}

#[test]
fn golden_repeated_locator_parameters_are_all_preserved() {
    // Locator parameters repeat — a participant with two network interfaces
    // announces two `PID_METATRAFFIC_UNICAST_LOCATOR` entries — so lookups
    // must not collapse them.
    //
    // A `Locator_t` is `long kind; unsigned long port; octet address[16]`,
    // 24 octets, already a multiple of four.
    let mut list = ParameterList::new(discovery_le());
    for port in [7410_u32, 7412] {
        let mut locator = Vec::new();
        locator.extend_from_slice(&1_u32.to_le_bytes()); // LOCATOR_KIND_UDPv4
        locator.extend_from_slice(&port.to_le_bytes());
        locator.extend_from_slice(&[0_u8; 12]);
        locator.extend_from_slice(&[127, 0, 0, 1]);
        assert_eq!(locator.len(), 24);
        list.push_octets(ParameterId::new(pid::METATRAFFIC_UNICAST_LOCATOR), locator)
            .expect("short enough");
    }

    let bytes = list.encode().expect("encode");
    // Two 28-octet entries plus the sentinel plus the header.
    assert_eq!(bytes.len(), 4 + 28 + 28 + 4);

    let (decoded, _) = ParameterList::decode(&bytes).expect("decode");
    let locators: Vec<_> = decoded
        .all_by_base(pid::METATRAFFIC_UNICAST_LOCATOR)
        .collect();
    assert_eq!(locators.len(), 2);
    assert_eq!(&locators[0].value[4..8], &7410_u32.to_le_bytes());
    assert_eq!(&locators[1].value[4..8], &7412_u32.to_le_bytes());
}

#[test]
fn golden_a_plain_encapsulation_is_refused_for_a_parameter_list() {
    // A discovery payload labelled `CDR_LE` is not a parameter list, whatever
    // its body looks like. The strict door refuses it and the lenient one
    // accepts it, because a few stacks do mislabel inline QoS.
    let mislabelled = [0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00];
    assert_eq!(
        ParameterList::decode(&mislabelled).map(|_| ()),
        Err(CdrError::UnsupportedEncapsulation(
            "an RTPS ParameterList needs a PL_CDR_BE or PL_CDR_LE encapsulation"
        ))
    );
    let (list, encoding) = ParameterList::decode_any(&mislabelled).expect("lenient");
    assert!(list.is_empty());
    assert_eq!(encoding.kind(), EncapsulationKind::CdrLe);
}

#[test]
fn golden_pl_cdr2_is_a_different_format_and_is_refused_here() {
    // Two wire formats are called "parameter list", and only one of them is
    // this module's:
    //
    //   PL_CDR   (0x0002 / 0x0003)  16-bit (id, length) entries, PID_SENTINEL
    //                               — OMG DDSI-RTPS 2.3 §9.4.2.11, what SPDP
    //                               and SEDP use.
    //   PL_CDR2  (0x000a / 0x000b)  a DHEADER bounding EMHEADER-tagged
    //                               members, *no sentinel at all*
    //                               — OMG DDS-XTypes 1.3, the XCDR2 encoding
    //                               of a `@mutable` type.
    //
    // The second is read with `CdrReader::read_member_header`; see
    // `golden_mutable_struct_with_two_members` in `golden_xcdr2.rs` for its
    // derivation. Handing one of its payloads to a sentinel parser would
    // silently produce nonsense, so the identifier is refused outright.
    assert!(EncapsulationKind::PlCdrLe.is_rtps_parameter_list());
    assert!(!EncapsulationKind::PlCdr2Le.is_rtps_parameter_list());
    assert!(
        EncapsulationKind::PlCdr2Le.is_parameter_list(),
        "it is still a member-tagged representation, just a different one"
    );

    // A PL_CDR2 payload: DHEADER = 8, one EMHEADER-tagged four-octet member,
    // and nothing that resembles a sentinel.
    let pl_cdr2 = [
        0x00, 0x0b, 0x00, 0x00, // PL_CDR2_LE
        0x08, 0x00, 0x00, 0x00, // DHEADER = 8
        0x01, 0x00, 0x00, 0x20, // EMHEADER id 1, LC=2
        0x3f, 0x00, 0x00, 0x00, // the member
    ];
    assert_eq!(
        ParameterList::decode(&pl_cdr2).map(|_| ()),
        Err(CdrError::UnsupportedEncapsulation(
            "an RTPS ParameterList needs a PL_CDR_BE or PL_CDR_LE encapsulation"
        ))
    );
}
