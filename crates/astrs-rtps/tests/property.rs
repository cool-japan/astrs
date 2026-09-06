//! Property tests: `decode(encode(x)) == x`, for every submessage and for
//! whole messages.
//!
//! The golden fixtures in the other test files pin specific octet sequences;
//! these cover the space between them. Three families of property:
//!
//! 1. **Per-submessage round trips.** Every kind, in both byte orders, with
//!    every optional field present and absent, and with the §8.6 extension
//!    fields a future peer might add.
//! 2. **Whole-message round trips.** The framing rules — `octetsToNextHeader`,
//!    four-octet alignment, the two readings of a zero length — only compose
//!    at this level, which is where their bugs live.
//! 3. **Hostile input.** Arbitrary octets must produce a value or a typed
//!    error, never a panic and never an unbounded allocation.
//!
//! # What the generators deliberately do not produce
//!
//! Bodies whose length is not a multiple of four. §8.3.3 makes such a
//! submessage un-followable — see
//! [`RtpsError::UnalignedBody`](astrs_rtps::RtpsError::UnalignedBody) — so a
//! message containing one in a non-final position is not representable at
//! all, and generating them would test the encoder's refusal rather than the
//! round trip. That refusal has its own unit tests.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use astrs_cdr::{CdrWriter, Encoding, Endianness, ParameterId, ParameterList, pid};
use astrs_rtps::messages::{
    AckNack, Data, DataFrag, DataPayload, Extension, FragmentGeometry, Gap, Header, Heartbeat,
    HeartbeatFrag, InfoDestination, InfoReply, InfoSource, InfoTimestamp, Message, NackFrag,
    Opaque, Pad, SerializedPayload, Submessage, SubmessageFlags, SubmessageHeader, SubmessageId,
    body_encoding,
};
use astrs_rtps::structure::{
    DdsDuration, Duration, EntityId, EntityKind, FragmentNumber, FragmentNumberSet, Guid,
    GuidPrefix, Locator, LocatorList, ProtocolVersion, SequenceNumber, SequenceNumberSet, Time,
    VendorId,
};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Strategies for the value types
// ---------------------------------------------------------------------------

fn arb_endianness() -> impl Strategy<Value = Endianness> {
    prop_oneof![Just(Endianness::Little), Just(Endianness::Big)]
}

fn arb_guid_prefix() -> impl Strategy<Value = GuidPrefix> {
    any::<[u8; 12]>().prop_map(GuidPrefix::new)
}

fn arb_entity_id() -> impl Strategy<Value = EntityId> {
    any::<[u8; 4]>().prop_map(EntityId::from_octets)
}

fn arb_sequence_number() -> impl Strategy<Value = SequenceNumber> {
    prop_oneof![
        1..=i64::MAX,
        Just(SequenceNumber::UNKNOWN.value()),
        Just(0_i64),
        any::<i64>(),
    ]
    .prop_map(SequenceNumber::new)
}

/// Bases that exercise both the ordinary range and the ends of it.
///
/// A uniform `1..=i64::MAX` would put every sample within 2^-55 of the top of
/// the range and never test a small base at all; the boundary cases are
/// stated explicitly so `insert`'s overflow handling is actually reached.
fn arb_set_base() -> impl Strategy<Value = i64> {
    prop_oneof![
        4 => 1_i64..=1_000_000,
        4 => 1_i64..=i64::MAX,
        1 => Just(1_i64),
        1 => Just(i64::MAX),
        1 => Just(i64::MAX - 255),
    ]
}

fn arb_sequence_number_set() -> impl Strategy<Value = SequenceNumberSet> {
    (arb_set_base(), 0_u32..=256, any::<[u32; 8]>()).prop_map(|(base, num_bits, words)| {
        let mut set = SequenceNumberSet::new(SequenceNumber::new(base));
        set.set_num_bits(num_bits).expect("at most 256");
        for (index, word) in words.iter().enumerate() {
            for bit in 0..32_u32 {
                if word & (1 << (31 - bit)) != 0 {
                    let offset = (index as u32) * 32 + bit;
                    // A base near i64::MAX has no room for the whole window;
                    // `checked_add` skips the numbers that would not exist.
                    if offset < num_bits
                        && let Some(number) = base.checked_add(i64::from(offset))
                    {
                        set.insert(SequenceNumber::new(number))
                            .expect("inside the window");
                    }
                }
            }
        }
        // `insert` may have widened the window; restore the declared width so
        // the generated value is exactly what a peer would have sent.
        set.set_num_bits(num_bits).expect("at most 256");
        set
    })
}

fn arb_fragment_number_set() -> impl Strategy<Value = FragmentNumberSet> {
    let base = prop_oneof![
        4 => 1_u32..=1_000_000,
        4 => 1_u32..=u32::MAX,
        1 => Just(1_u32),
        1 => Just(u32::MAX),
        1 => Just(u32::MAX - 255),
    ];
    (base, 0_u32..=256, any::<[u32; 8]>()).prop_map(|(base, num_bits, words)| {
        let mut set = FragmentNumberSet::new(FragmentNumber::new(base));
        set.set_num_bits(num_bits).expect("at most 256");
        for (index, word) in words.iter().enumerate() {
            for bit in 0..32_u32 {
                if word & (1 << (31 - bit)) != 0 {
                    let offset = (index as u32) * 32 + bit;
                    if offset < num_bits && base.checked_add(offset).is_some() {
                        set.insert(FragmentNumber::new(base + offset))
                            .expect("inside the window");
                    }
                }
            }
        }
        set.set_num_bits(num_bits).expect("at most 256");
        set
    })
}

fn arb_locator() -> impl Strategy<Value = Locator> {
    (any::<i32>(), any::<u32>(), any::<[u8; 16]>())
        .prop_map(|(kind, port, address)| Locator::from_raw(kind, port, address))
}

fn arb_locator_list() -> impl Strategy<Value = LocatorList> {
    prop::collection::vec(arb_locator(), 0..4).prop_map(LocatorList::from)
}

fn arb_time() -> impl Strategy<Value = Time> {
    (any::<i32>(), any::<u32>()).prop_map(|(seconds, fraction)| Time::new(seconds, fraction))
}

/// A payload whose length is a multiple of four, so the submessage carrying
/// it can be followed by another (§8.3.3).
fn arb_payload() -> impl Strategy<Value = SerializedPayload<'static>> {
    prop::collection::vec(any::<u8>(), 0..8)
        .prop_map(|words| SerializedPayload::new(words.repeat(4)))
}

fn arb_inline_qos(endianness: Endianness) -> impl Strategy<Value = ParameterList<'static>> {
    prop::collection::vec(
        (any::<u16>(), prop::collection::vec(any::<u8>(), 0..12)),
        0..4,
    )
    .prop_map(move |entries| {
        let mut list = ParameterList::new(astrs_rtps::messages::inline_qos_encoding(endianness));
        for (id, value) in entries {
            // PID_PAD and PID_SENTINEL end or vanish from a list, and the
            // two long-form escapes are refused outright, so a generated
            // id avoids all four.
            let id = if matches!(id, pid::PAD | pid::SENTINEL | pid::EXTENDED | pid::LIST_END) {
                pid::USER_DATA
            } else {
                id
            };
            list.push_octets(ParameterId::new(id), value)
                .expect("short enough");
        }
        list
    })
}

/// Flag bits outside `defined`: the ones §8.6 tells a receiver to ignore and
/// this crate preserves.
fn arb_reserved_flags(defined: u8) -> impl Strategy<Value = u8> {
    any::<u8>().prop_map(move |bits| bits & !defined)
}

/// An extension whose trailing octets keep the body four-octet aligned.
fn arb_extension(defined: u8) -> impl Strategy<Value = Extension> {
    (arb_reserved_flags(defined), 0_usize..3).prop_map(|(reserved_flags, words)| Extension {
        reserved_flags,
        trailing: vec![0xa5; words * 4],
    })
}

// ---------------------------------------------------------------------------
// Strategies for the submessages
// ---------------------------------------------------------------------------

fn arb_data() -> impl Strategy<Value = Data<'static>> {
    (
        arb_endianness(),
        any::<u16>(),
        arb_entity_id(),
        arb_entity_id(),
        arb_sequence_number(),
        any::<bool>(),
        prop_oneof![Just(0_u8), Just(1_u8), Just(2_u8)],
        arb_payload(),
        any::<bool>(),
        arb_reserved_flags(Data::DEFINED_FLAGS),
    )
        .prop_flat_map(
            |(
                endianness,
                extra_flags,
                reader_id,
                writer_id,
                writer_sn,
                with_qos,
                payload_kind,
                payload,
                non_standard,
                reserved_flags,
            )| {
                arb_inline_qos(endianness).prop_map(move |qos| {
                    let mut data = Data::new(
                        reader_id,
                        writer_id,
                        writer_sn,
                        match payload_kind {
                            1 => DataPayload::Data(payload.clone()),
                            2 => DataPayload::Key(payload.clone()),
                            _ => DataPayload::None,
                        },
                    )
                    .with_endianness(endianness);
                    data.extra_flags = extra_flags;
                    data.non_standard_payload = non_standard;
                    data.extension = Extension::from_reserved_flags(reserved_flags);
                    if with_qos {
                        data = data.with_inline_qos(qos);
                    }
                    data
                })
            },
        )
}

fn arb_data_frag() -> impl Strategy<Value = DataFrag<'static>> {
    (
        arb_endianness(),
        any::<u16>(),
        arb_entity_id(),
        arb_entity_id(),
        arb_sequence_number(),
        (1_u32..1_000, 1_u16..8, 4_u16..2_000, any::<u32>()),
        any::<bool>(),
        arb_payload(),
        any::<bool>(),
        arb_reserved_flags(DataFrag::DEFINED_FLAGS),
    )
        .prop_flat_map(
            |(
                endianness,
                extra_flags,
                reader_id,
                writer_id,
                writer_sn,
                (starting, count, fragment_size, sample_size),
                with_qos,
                payload,
                key,
                reserved_flags,
            )| {
                arb_inline_qos(endianness).prop_map(move |qos| {
                    let mut frag = DataFrag::new(
                        reader_id,
                        writer_id,
                        writer_sn,
                        FragmentGeometry::new(
                            FragmentNumber::new(starting),
                            count,
                            fragment_size,
                            sample_size,
                        ),
                        payload.clone(),
                    )
                    .with_endianness(endianness);
                    frag.extra_flags = extra_flags;
                    frag.key = key;
                    frag.extension = Extension::from_reserved_flags(reserved_flags);
                    if with_qos {
                        frag = frag.with_inline_qos(qos);
                    }
                    frag
                })
            },
        )
}

fn arb_heartbeat() -> impl Strategy<Value = Heartbeat> {
    (
        arb_endianness(),
        any::<bool>(),
        any::<bool>(),
        arb_entity_id(),
        arb_entity_id(),
        arb_sequence_number(),
        arb_sequence_number(),
        any::<i32>(),
        arb_extension(Heartbeat::DEFINED_FLAGS),
    )
        .prop_map(
            |(
                endianness,
                is_final,
                liveliness,
                reader_id,
                writer_id,
                first_sn,
                last_sn,
                count,
                extension,
            )| {
                let mut heartbeat = Heartbeat::new(reader_id, writer_id, first_sn, last_sn, count)
                    .with_endianness(endianness);
                heartbeat.is_final = is_final;
                heartbeat.liveliness = liveliness;
                heartbeat.extension = extension;
                heartbeat
            },
        )
}

fn arb_heartbeat_frag() -> impl Strategy<Value = HeartbeatFrag> {
    (
        arb_endianness(),
        arb_entity_id(),
        arb_entity_id(),
        arb_sequence_number(),
        any::<u32>(),
        any::<i32>(),
        arb_extension(HeartbeatFrag::DEFINED_FLAGS),
    )
        .prop_map(
            |(endianness, reader_id, writer_id, writer_sn, last, count, extension)| {
                let mut frag = HeartbeatFrag::new(
                    reader_id,
                    writer_id,
                    writer_sn,
                    FragmentNumber::new(last),
                    count,
                )
                .with_endianness(endianness);
                frag.extension = extension;
                frag
            },
        )
}

fn arb_acknack() -> impl Strategy<Value = AckNack> {
    (
        arb_endianness(),
        any::<bool>(),
        arb_entity_id(),
        arb_entity_id(),
        arb_sequence_number_set(),
        any::<i32>(),
        arb_extension(AckNack::DEFINED_FLAGS),
    )
        .prop_map(
            |(endianness, is_final, reader_id, writer_id, state, count, extension)| {
                let mut acknack =
                    AckNack::new(reader_id, writer_id, state, count).with_endianness(endianness);
                acknack.is_final = is_final;
                acknack.extension = extension;
                acknack
            },
        )
}

fn arb_nack_frag() -> impl Strategy<Value = NackFrag> {
    (
        arb_endianness(),
        arb_entity_id(),
        arb_entity_id(),
        arb_sequence_number(),
        arb_fragment_number_set(),
        any::<i32>(),
        arb_extension(NackFrag::DEFINED_FLAGS),
    )
        .prop_map(
            |(endianness, reader_id, writer_id, writer_sn, state, count, extension)| {
                let mut nack = NackFrag::new(reader_id, writer_id, writer_sn, state, count)
                    .with_endianness(endianness);
                nack.extension = extension;
                nack
            },
        )
}

fn arb_gap() -> impl Strategy<Value = Gap> {
    (
        arb_endianness(),
        arb_entity_id(),
        arb_entity_id(),
        arb_sequence_number(),
        arb_sequence_number_set(),
        arb_extension(Gap::DEFINED_FLAGS),
    )
        .prop_map(
            |(endianness, reader_id, writer_id, gap_start, gap_list, extension)| {
                let mut gap =
                    Gap::new(reader_id, writer_id, gap_start, gap_list).with_endianness(endianness);
                gap.extension = extension;
                gap
            },
        )
}

fn arb_info_timestamp() -> impl Strategy<Value = InfoTimestamp> {
    (
        arb_endianness(),
        prop::option::of(arb_time()),
        arb_reserved_flags(InfoTimestamp::DEFINED_FLAGS),
    )
        .prop_map(|(endianness, timestamp, reserved_flags)| {
            let mut stamp = match timestamp {
                Some(time) => InfoTimestamp::at(time),
                None => InfoTimestamp::invalidate(),
            }
            .with_endianness(endianness);
            stamp.reserved_flags = reserved_flags;
            stamp
        })
}

fn arb_info_source() -> impl Strategy<Value = InfoSource> {
    (
        arb_endianness(),
        any::<u32>(),
        any::<[u8; 2]>(),
        any::<[u8; 2]>(),
        arb_guid_prefix(),
        arb_reserved_flags(InfoSource::DEFINED_FLAGS),
    )
        .prop_map(
            |(endianness, unused, version, vendor, guid_prefix, reserved_flags)| {
                let mut source = InfoSource::with_parts(
                    ProtocolVersion::from_bytes(version),
                    VendorId::new(vendor),
                    guid_prefix,
                )
                .with_endianness(endianness);
                source.unused = unused;
                source.reserved_flags = reserved_flags;
                source
            },
        )
}

fn arb_info_destination() -> impl Strategy<Value = InfoDestination> {
    (
        arb_endianness(),
        arb_guid_prefix(),
        arb_reserved_flags(InfoDestination::DEFINED_FLAGS),
    )
        .prop_map(|(endianness, guid_prefix, reserved_flags)| {
            let mut destination = InfoDestination::new(guid_prefix).with_endianness(endianness);
            destination.reserved_flags = reserved_flags;
            destination
        })
}

fn arb_info_reply() -> impl Strategy<Value = InfoReply> {
    (
        arb_endianness(),
        arb_locator_list(),
        prop::option::of(arb_locator_list()),
        arb_extension(InfoReply::DEFINED_FLAGS),
    )
        .prop_map(|(endianness, unicast, multicast, extension)| {
            let mut reply = InfoReply::new(unicast).with_endianness(endianness);
            reply.multicast_locator_list = multicast;
            reply.extension = extension;
            reply
        })
}

fn arb_pad() -> impl Strategy<Value = Pad> {
    (
        arb_endianness(),
        0_usize..4,
        arb_reserved_flags(Pad::DEFINED_FLAGS),
    )
        .prop_map(|(endianness, words, reserved_flags)| {
            let mut pad = Pad::zeros(words * 4);
            pad.endianness = endianness;
            pad.reserved_flags = reserved_flags;
            pad
        })
}

/// An opaque submessage: an id outside the assigned thirteen, and a body
/// that is a nonzero multiple of four.
///
/// Nonzero because a zero `octetsToNextHeader` on an id that is not `PAD` or
/// `INFO_TS` means "to the end of the message" (§8.3.3.2.3), which is a
/// different value, not a round-trip failure.
fn arb_opaque() -> impl Strategy<Value = Opaque> {
    (any::<u8>(), any::<u8>(), 1_usize..4).prop_map(|(id, flags, words)| {
        let id = SubmessageId::from_raw(id);
        let id = if id.is_known() {
            SubmessageId::from_raw(0x80)
        } else {
            id
        };
        Opaque::new(id, SubmessageFlags::new(flags), vec![0x5a; words * 4])
    })
}

fn arb_submessage() -> impl Strategy<Value = Submessage<'static>> {
    prop_oneof![
        arb_data().prop_map(Submessage::Data),
        arb_data_frag().prop_map(Submessage::DataFrag),
        arb_heartbeat().prop_map(Submessage::Heartbeat),
        arb_heartbeat_frag().prop_map(Submessage::HeartbeatFrag),
        arb_acknack().prop_map(Submessage::AckNack),
        arb_nack_frag().prop_map(Submessage::NackFrag),
        arb_gap().prop_map(Submessage::Gap),
        arb_info_timestamp().prop_map(Submessage::InfoTimestamp),
        arb_info_source().prop_map(Submessage::InfoSource),
        arb_info_destination().prop_map(Submessage::InfoDestination),
        arb_info_reply().prop_map(Submessage::InfoReply),
        arb_pad().prop_map(Submessage::Pad),
        arb_opaque().prop_map(Submessage::Opaque),
    ]
}

fn arb_message() -> impl Strategy<Value = Message<'static>> {
    (
        arb_guid_prefix(),
        any::<[u8; 2]>(),
        prop::collection::vec(arb_submessage(), 0..6),
    )
        .prop_map(|(prefix, vendor, submessages)| Message {
            header: Header::with_parts(ProtocolVersion::V2_3, VendorId::new(vendor), prefix),
            submessages,
        })
}

/// Round-trip one submessage through its own body writer and reader.
fn round_trip(submessage: &Submessage<'static>) -> Submessage<'static> {
    let mut writer = CdrWriter::headerless(body_encoding(submessage.endianness()));
    submessage
        .write_body(&mut writer)
        .expect("every generated submessage encodes");
    let body = writer.finish();
    assert_eq!(
        body.len(),
        submessage.body_len(),
        "body_len disagrees with the octets written for {submessage}"
    );
    let header = SubmessageHeader::new(
        submessage.id(),
        submessage.flags(),
        u16::try_from(body.len()).expect("generated bodies are small"),
    );
    Submessage::read(&header, &body)
        .expect("what we wrote must read back")
        .into_owned()
}

proptest! {
    #[test]
    fn every_submessage_round_trips(submessage in arb_submessage()) {
        prop_assert_eq!(round_trip(&submessage), submessage);
    }

    #[test]
    fn a_submessage_reports_the_flags_it_encodes_with(submessage in arb_submessage()) {
        // The flags octet is derived from the field values, never stored, so
        // decoding must reconstruct exactly the same octet.
        let decoded = round_trip(&submessage);
        prop_assert_eq!(decoded.flags(), submessage.flags());
        prop_assert_eq!(decoded.id(), submessage.id());
        prop_assert_eq!(decoded.endianness(), submessage.endianness());
    }

    #[test]
    fn every_message_round_trips(message in arb_message()) {
        let datagram = message.encode().expect("generated bodies stay aligned");
        prop_assert_eq!(datagram.len(), message.serialized_len());
        let decoded = Message::decode(&datagram).expect("what we wrote must read back");
        prop_assert_eq!(decoded.into_owned(), message);
    }

    #[test]
    fn re_encoding_a_decoded_message_reproduces_its_octets(message in arb_message()) {
        let datagram = message.encode().expect("encode");
        let decoded = Message::decode(&datagram).expect("decode");
        prop_assert_eq!(decoded.encode().expect("re-encode"), datagram);
    }

    #[test]
    fn arbitrary_octets_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        // Whatever the input, decode returns a value or a typed error. The
        // `Vec` inside never grows past what the input could justify, because
        // every declared length is checked against the octets that remain
        // before anything is allocated.
        match Message::decode(&bytes) {
            Ok(message) => {
                prop_assert!(message.submessages.len() * 4 <= bytes.len());
                // A decoded message either re-encodes or names why it cannot.
                let _ = message.encode();
            }
            Err(error) => {
                prop_assert!(!error.to_string().is_empty());
            }
        }
    }

    #[test]
    fn arbitrary_octets_behind_a_valid_header_never_panic(
        tail in prop::collection::vec(any::<u8>(), 0..256),
    ) {
        // The same, but past the cheap protocol-id reject, so the submessage
        // loop itself is what is being driven.
        let mut bytes = Vec::from(common::SENDER_HEADER);
        bytes.extend_from_slice(&tail);
        let _ = Message::decode(&bytes);
        if let Ok((_, submessages)) = Message::scan(&bytes) {
            for result in submessages {
                if result.is_err() {
                    break;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The value types
// ---------------------------------------------------------------------------

/// Round-trip a CDR value through both byte orders.
fn cdr_round_trip<T>(value: &T) -> (T, T)
where
    T: astrs_cdr::CdrSerialize + for<'de> astrs_cdr::CdrDeserialize<'de>,
{
    let little = astrs_cdr::to_vec_headerless(value, Encoding::ROS2).expect("encode");
    let big =
        astrs_cdr::to_vec_headerless(value, Encoding::new(astrs_cdr::EncapsulationKind::CdrBe))
            .expect("encode");
    (
        astrs_cdr::from_bytes_headerless(&little, Encoding::ROS2).expect("decode"),
        astrs_cdr::from_bytes_headerless(&big, Encoding::new(astrs_cdr::EncapsulationKind::CdrBe))
            .expect("decode"),
    )
}

proptest! {
    #[test]
    fn identifiers_round_trip_through_cdr(
        prefix in arb_guid_prefix(),
        entity in arb_entity_id(),
        version in any::<[u8; 2]>(),
        vendor in any::<[u8; 2]>(),
    ) {
        let guid = Guid::new(prefix, entity);
        prop_assert_eq!(cdr_round_trip(&guid), (guid, guid));
        prop_assert_eq!(cdr_round_trip(&prefix), (prefix, prefix));
        prop_assert_eq!(cdr_round_trip(&entity), (entity, entity));
        prop_assert_eq!(Guid::from_bytes(guid.to_bytes()), guid);

        let version = ProtocolVersion::from_bytes(version);
        prop_assert_eq!(cdr_round_trip(&version), (version, version));
        let vendor = VendorId::new(vendor);
        prop_assert_eq!(cdr_round_trip(&vendor), (vendor, vendor));
    }

    #[test]
    fn numbers_and_times_round_trip_through_cdr(
        sequence in arb_sequence_number(),
        fragment in any::<u32>(),
        time in arb_time(),
        seconds in any::<i32>(),
        fraction in any::<u32>(),
    ) {
        prop_assert_eq!(cdr_round_trip(&sequence), (sequence, sequence));
        prop_assert_eq!(
            SequenceNumber::from_halves(sequence.high(), sequence.low()),
            sequence
        );

        let fragment = FragmentNumber::new(fragment);
        prop_assert_eq!(cdr_round_trip(&fragment), (fragment, fragment));

        prop_assert_eq!(cdr_round_trip(&time), (time, time));
        let duration = Duration::new(seconds, fraction);
        prop_assert_eq!(cdr_round_trip(&duration), (duration, duration));
        let dds = DdsDuration::new(seconds, fraction % 1_000_000_000);
        prop_assert_eq!(cdr_round_trip(&dds), (dds, dds));
    }

    #[test]
    fn locators_round_trip_through_cdr(locator in arb_locator()) {
        prop_assert_eq!(cdr_round_trip(&locator), (locator, locator));
        // The classification never loses the raw value.
        prop_assert_eq!(locator.kind().raw(), locator.kind_raw());
    }

    #[test]
    fn a_locator_list_round_trips_through_a_submessage_body(list in arb_locator_list()) {
        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        list.write(&mut writer).expect("write");
        let body = writer.finish();
        prop_assert_eq!(body.len(), list.serialized_len());
        let mut reader = astrs_cdr::CdrReader::with_encoding(&body, Encoding::ROS2);
        prop_assert_eq!(LocatorList::read(&mut reader).expect("read"), list);
    }

    #[test]
    fn sequence_number_sets_round_trip_and_agree_with_their_members(
        set in arb_sequence_number_set(),
    ) {
        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        set.write(&mut writer).expect("write");
        let body = writer.finish();
        prop_assert_eq!(body.len(), set.serialized_len());
        let mut reader = astrs_cdr::CdrReader::with_encoding(&body, Encoding::ROS2);
        let decoded = SequenceNumberSet::read(&mut reader, "test").expect("read");
        prop_assert_eq!(decoded, set);
        prop_assert_eq!(decoded.len(), set.iter().count());
        for number in &set {
            prop_assert!(set.contains(number));
        }
    }

    #[test]
    fn fragment_number_sets_round_trip_and_agree_with_their_members(
        set in arb_fragment_number_set(),
    ) {
        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        set.write(&mut writer).expect("write");
        let body = writer.finish();
        prop_assert_eq!(body.len(), set.serialized_len());
        let mut reader = astrs_cdr::CdrReader::with_encoding(&body, Encoding::ROS2);
        let decoded = FragmentNumberSet::read(&mut reader, "test").expect("read");
        prop_assert_eq!(decoded, set);
        prop_assert_eq!(decoded.len(), set.iter().count());
        for number in &set {
            prop_assert!(set.contains(number));
        }
    }

    #[test]
    fn a_message_header_round_trips(prefix in arb_guid_prefix(), vendor in any::<[u8; 2]>()) {
        let header = Header::with_parts(ProtocolVersion::V2_3, VendorId::new(vendor), prefix);
        prop_assert_eq!(Header::decode(&header.to_bytes()).expect("decode"), header);
        prop_assert!(Header::looks_like_rtps(&header.to_bytes()));
    }

    #[test]
    fn a_submessage_header_round_trips(
        id in any::<u8>(),
        flags in any::<u8>(),
        length in any::<u16>(),
    ) {
        let header = SubmessageHeader::new(
            SubmessageId::from_raw(id),
            SubmessageFlags::new(flags),
            length,
        );
        prop_assert_eq!(
            SubmessageHeader::decode(&header.to_bytes()).expect("decode"),
            header
        );
    }
}

// ---------------------------------------------------------------------------
// The generators themselves
// ---------------------------------------------------------------------------

/// Guard against a silently degenerate generator.
///
/// The value of `every_submessage_round_trips` depends entirely on what comes
/// out of these strategies. A refactor that made `arb_sequence_number_set`
/// produce only empty sets would leave every test green while quietly
/// dropping the `ACKNACK` and `GAP` bitmaps — the highest-value round trips in
/// the crate — from the covered space. This samples each generator and asserts
/// a coverage floor, so that failure mode is loud.
#[test]
fn the_generators_cover_the_interesting_shapes() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;

    const SAMPLES: usize = 512;
    let mut runner = TestRunner::deterministic();

    let sets = arb_sequence_number_set();
    let (mut non_empty, mut wide, mut small_base, mut large_base) = (0, 0, 0, 0);
    for _ in 0..SAMPLES {
        let set = sets.new_tree(&mut runner).expect("a value tree").current();
        non_empty += usize::from(!set.is_empty());
        wide += usize::from(set.num_bits() > 32);
        small_base += usize::from(set.base().value() < 1_000_000);
        large_base += usize::from(set.base().value() > i64::MAX / 2);
        // Whatever the base, the invariant holds.
        assert!(set.num_bits() <= 256);
        assert_eq!(set.len(), set.iter().count());
    }
    assert!(non_empty > SAMPLES / 2, "only {non_empty} non-empty sets");
    assert!(wide > SAMPLES / 8, "only {wide} sets wider than one word");
    assert!(small_base > SAMPLES / 16, "only {small_base} small bases");
    assert!(large_base > SAMPLES / 16, "only {large_base} large bases");

    let frags = arb_fragment_number_set();
    let mut non_empty = 0;
    for _ in 0..SAMPLES {
        let set = frags.new_tree(&mut runner).expect("a value tree").current();
        non_empty += usize::from(!set.is_empty());
        assert_eq!(set.len(), set.iter().count());
    }
    assert!(non_empty > SAMPLES / 2, "only {non_empty} non-empty sets");

    let submessages = arb_submessage();
    let mut kinds = std::collections::BTreeSet::new();
    for _ in 0..SAMPLES {
        let submessage = submessages
            .new_tree(&mut runner)
            .expect("a value tree")
            .current();
        kinds.insert(submessage.id().raw());
    }
    assert!(
        kinds.len() >= 13,
        "only {} distinct submessage kinds generated",
        kinds.len()
    );

    let payloads = arb_payload();
    for _ in 0..SAMPLES {
        let payload = payloads
            .new_tree(&mut runner)
            .expect("a value tree")
            .current();
        assert!(payload.is_aligned(), "generated payloads stay followable");
    }
}

#[test]
fn the_entity_kind_table_is_total_over_the_octet() {
    // Not a property test, but the same idea: every one of the 256 possible
    // kind octets classifies and round-trips.
    for raw in 0..=u8::MAX {
        let kind = EntityKind::new(raw);
        assert_eq!(kind.raw(), raw);
        let id = EntityId::new([0, 0, 0], kind);
        assert_eq!(EntityId::from_octets(id.to_octets()), id);
        // A kind is at most one of writer, reader, group and participant.
        let roles = usize::from(kind.is_writer())
            + usize::from(kind.is_reader())
            + usize::from(kind.is_group())
            + usize::from(kind.is_participant());
        assert!(roles <= 1, "0x{raw:02x} claims {roles} roles");
    }
}
