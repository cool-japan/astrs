//! Golden packets: `DATA` with inline QoS, `INFO_TS` + `DATA`, and the
//! remaining interpreter submessages — plus the malformed datagrams a strict
//! decoder has to refuse.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::net::Ipv4Addr;

use astrs_cdr::{Encoding, ParameterId, ParameterList, pid};
use astrs_rtps::messages::{
    Data, DataPayload, InfoDestination, InfoReply, InfoSource, InfoTimestamp, Message, Pad,
    SerializedPayload,
};
use astrs_rtps::structure::{EntityId, EntityKind, Locator, LocatorList, SequenceNumber, Time};
use astrs_rtps::{RtpsError, SetDefect};

use common::{PEER, SENDER, SENDER_HEADER, assert_golden, concat};

const WRITER_OCTETS: [u8; 4] = [0x00, 0x00, 0x01, 0x03];
const UNKNOWN_OCTETS: [u8; 4] = [0x00, 0x00, 0x00, 0x00];

fn writer_id() -> EntityId {
    EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY)
}

/// The sixteen-octet instance key hash the dispose below carries.
const KEY_HASH: [u8; 16] = [
    0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20,
];

#[test]
fn a_dispose_is_a_data_with_inline_qos_and_a_key_payload() {
    // §8.3.7.2.5: with the K flag the serializedPayload is the instance key
    // rather than the sample, and the statusInfo inline QoS parameter says
    // what happened to the instance.
    let mut inline_qos = ParameterList::new(Encoding::DISCOVERY);
    inline_qos
        .push_octets(
            ParameterId::new(pid::STATUS_INFO),
            &[0x00_u8, 0x00, 0x00, 0x03][..],
        )
        .expect("short enough");
    inline_qos
        .push_octets(ParameterId::new(pid::KEY_HASH), &KEY_HASH[..])
        .expect("short enough");

    let message = Message::from_participant(SENDER).with(
        Data::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::new(9),
            DataPayload::Key(SerializedPayload::from_cdr(&7_u32).expect("encode the key")),
        )
        .with_inline_qos(inline_qos),
    );

    let expected = concat(&[
        &SENDER_HEADER,
        // DATA: flags 0x0b = E | Q | K. octetsToNextHeader = 60:
        // 20 prelude + 32 inline QoS + 8 payload.
        &[0x15, 0x0b, 0x3c, 0x00],
        &[0x00, 0x00], // extraFlags
        &[0x10, 0x00], // octetsToInlineQos = 16
        &UNKNOWN_OCTETS,
        &WRITER_OCTETS,
        &[0x00, 0x00, 0x00, 0x00], // writerSN.high
        &[0x09, 0x00, 0x00, 0x00], // writerSN.low = 9
        // Inline QoS: a ParameterList with *no* encapsulation header of its
        // own — it inherits the submessage's byte order from the E flag
        // (§8.3.7.2.2) — terminated by PID_SENTINEL.
        //
        // PID_STATUS_INFO = 0x0071, four octets of StatusInfo_t. The type is
        // an octet[4], so its octets are not byte-swapped.
        &[0x71, 0x00, 0x04, 0x00],
        &[0x00, 0x00, 0x00, 0x03],
        // PID_KEY_HASH = 0x0070, sixteen octets of KeyHash_t.
        &[0x70, 0x00, 0x10, 0x00],
        &KEY_HASH,
        // PID_SENTINEL.
        &[0x01, 0x00, 0x00, 0x00],
        // serializedPayload: the key, CDR_LE, one unsigned long.
        &[0x00, 0x01, 0x00, 0x00],
        &[0x07, 0x00, 0x00, 0x00],
    ]);
    assert_eq!(expected.len(), 20 + 4 + 60);
    assert_golden(&message, &expected);

    let data = message.submessages[0].as_data().expect("DATA");
    assert!(matches!(data.payload, DataPayload::Key(_)));
    let list = data.inline_qos.as_ref().expect("inline QoS");
    assert_eq!(list.len(), 2);
    assert_eq!(
        list.get_by_base(pid::STATUS_INFO)
            .expect("PID_STATUS_INFO")
            .value
            .as_ref(),
        &[0x00, 0x00, 0x00, 0x03]
    );
    assert_eq!(
        list.get_by_base(pid::KEY_HASH)
            .expect("PID_KEY_HASH")
            .value
            .as_ref(),
        &KEY_HASH
    );
}

#[test]
fn an_info_ts_stamps_the_data_that_follows_it() {
    // §8.3.7.9: the timestamp applies to every DATA and DATA_FRAG after it in
    // the same message, until another INFO_TS says otherwise.
    let message = Message::from_participant(SENDER)
        .with(InfoTimestamp::at(Time::new(1_234, 1 << 31)))
        .with(Data::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::new(42),
            DataPayload::Data(SerializedPayload::from_cdr(&-7_i32).expect("encode")),
        ));

    let expected = concat(&[
        &SENDER_HEADER,
        // INFO_TS: id 0x09, flags 0x01 — E set, I clear, so a Time_t follows.
        &[0x09, 0x01, 0x08, 0x00],
        // seconds = 1234 = 0x4d2 (long, little-endian).
        &[0xd2, 0x04, 0x00, 0x00],
        // fraction = 2^31, which is exactly half a second: the field counts
        // 2^-32 s, not nanoseconds.
        &[0x00, 0x00, 0x00, 0x80],
        // DATA: flags 0x05 = E | D. octetsToNextHeader = 28 = 20 + 8.
        &[0x15, 0x05, 0x1c, 0x00],
        &[0x00, 0x00],
        &[0x10, 0x00],
        &UNKNOWN_OCTETS,
        &WRITER_OCTETS,
        &[0x00, 0x00, 0x00, 0x00],
        &[0x2a, 0x00, 0x00, 0x00], // writerSN = 42
        // serializedPayload: CDR_LE, one long of value -7 (two's complement).
        &[0x00, 0x01, 0x00, 0x00],
        &[0xf9, 0xff, 0xff, 0xff],
    ]);
    assert_eq!(expected.len(), 20 + 12 + 32);
    assert_golden(&message, &expected);

    let Some(astrs_rtps::messages::Submessage::InfoTimestamp(stamp)) = message.submessages.first()
    else {
        unreachable!("the fixture starts with an INFO_TS")
    };
    assert_eq!(
        stamp.timestamp.expect("a timestamp").subsec_nanos(),
        500_000_000
    );
    assert!(!stamp.invalidates());

    let payload = message.submessages[1]
        .as_data()
        .expect("DATA")
        .payload
        .payload()
        .expect("a payload");
    assert_eq!(payload.decode::<i32>().expect("decode"), -7);
}

#[test]
fn an_invalidating_info_ts_has_an_empty_body() {
    // With the I flag the submessage says the samples that follow carry no
    // source timestamp, and octetsToNextHeader is zero — which for INFO_TS
    // means "empty", not "to the end of the message" (§8.3.3.2.3). The DATA
    // after it proves the distinction.
    let message = Message::from_participant(SENDER)
        .with(InfoTimestamp::invalidate())
        .with(Data::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            DataPayload::None,
        ));

    let expected = concat(&[
        &SENDER_HEADER,
        &[0x09, 0x03, 0x00, 0x00], // flags 0x03 = E | I, length 0
        &[0x15, 0x01, 0x14, 0x00], // flags 0x01: neither D nor K, length 20
        &[0x00, 0x00],
        &[0x10, 0x00],
        &UNKNOWN_OCTETS,
        &WRITER_OCTETS,
        &[0x00, 0x00, 0x00, 0x00],
        &[0x01, 0x00, 0x00, 0x00],
    ]);
    assert_eq!(expected.len(), 20 + 4 + 24);
    assert_golden(&message, &expected);
    assert_eq!(
        message.submessages.len(),
        2,
        "the PAD-like zero did not swallow the DATA"
    );
}

#[test]
fn the_remaining_interpreter_submessages_are_byte_exact() {
    let message = Message::from_participant(SENDER)
        .with(InfoSource::new(PEER))
        .with(
            InfoReply::new(LocatorList::single(Locator::udpv4(
                Ipv4Addr::LOCALHOST,
                7_410,
            )))
            .with_multicast(LocatorList::single(Locator::udpv4(
                Ipv4Addr::new(239, 255, 0, 1),
                7_400,
            ))),
        )
        .with(Pad::zeros(4));

    let expected = concat(&[
        &SENDER_HEADER,
        // INFO_SRC: id 0x0c, flags 0x01, octetsToNextHeader 20. The body
        // starts with four unused octets (§8.3.7.8.2), then restates the
        // version, vendor and prefix the message header carries.
        &[0x0c, 0x01, 0x14, 0x00],
        &[0x00, 0x00, 0x00, 0x00],
        &[0x02, 0x03],
        &[0x41, 0x53],
        &PEER.to_bytes(),
        // INFO_REPLY: id 0x0f, flags 0x03 = E | M, so a multicast list
        // follows the unicast one. octetsToNextHeader = 56 = 2 * (4 + 24).
        &[0x0f, 0x03, 0x38, 0x00],
        // unicastLocatorList: a sequence, so a four-octet count first.
        &[0x01, 0x00, 0x00, 0x00],
        &[0x01, 0x00, 0x00, 0x00], // kind = LOCATOR_KIND_UDPv4
        &[0xf2, 0x1c, 0x00, 0x00], // port 7410
        &[0x00; 12],
        &[0x7f, 0x00, 0x00, 0x01], // 127.0.0.1
        // multicastLocatorList.
        &[0x01, 0x00, 0x00, 0x00],
        &[0x01, 0x00, 0x00, 0x00],
        &[0xe8, 0x1c, 0x00, 0x00], // port 7400
        &[0x00; 12],
        &[0xef, 0xff, 0x00, 0x01], // 239.255.0.1
        // PAD: id 0x01, four octets of filler.
        &[0x01, 0x01, 0x04, 0x00],
        &[0x00, 0x00, 0x00, 0x00],
    ]);
    assert_eq!(expected.len(), 20 + 24 + 60 + 8);
    assert_golden(&message, &expected);
}

#[test]
fn an_info_dst_of_all_zeros_readdresses_the_rest_to_everyone() {
    let message = Message::from_participant(SENDER).with(InfoDestination::to_everyone());
    let expected = concat(&[&SENDER_HEADER, &[0x0e, 0x01, 0x0c, 0x00], &[0x00; 12]]);
    assert_golden(&message, &expected);
}

#[test]
fn a_datagram_that_is_not_rtps_is_refused_at_its_first_four_octets() {
    let mut bytes = Vec::from(*b"HTTP/1.1 200 OK\r\n\r\nx");
    bytes.truncate(20);
    assert_eq!(
        Message::decode(&bytes),
        Err(RtpsError::BadProtocolId { found: *b"HTTP" })
    );

    let mut wrong_major = SENDER_HEADER;
    wrong_major[4] = 1;
    assert_eq!(
        Message::decode(&wrong_major),
        Err(RtpsError::UnsupportedProtocolVersion { major: 1, minor: 3 })
    );

    // …but any 2.x minor is processed, per §8.6.
    let mut future_minor = SENDER_HEADER;
    future_minor[5] = 9;
    assert!(Message::decode(&future_minor).is_ok());
}

#[test]
fn a_data_with_both_the_d_and_k_flags_is_refused() {
    // §8.3.7.2.5 makes the two exclusive: the payload is the sample or the
    // key, and a decoder that picked one by precedence would silently deliver
    // a key as a sample.
    let bytes = concat(&[
        &SENDER_HEADER,
        &[0x15, 0x0d, 0x18, 0x00], // flags E | D | K
        &[0x00, 0x00],
        &[0x10, 0x00],
        &UNKNOWN_OCTETS,
        &WRITER_OCTETS,
        &[0x00, 0x00, 0x00, 0x00],
        &[0x01, 0x00, 0x00, 0x00],
        &[0x00, 0x01, 0x00, 0x00],
    ]);
    assert_eq!(
        Message::decode(&bytes).map(|_| ()),
        Err(RtpsError::ConflictingDataFlags)
    );
}

#[test]
fn an_octets_to_inline_qos_below_sixteen_is_refused() {
    // Eight would place inlineQos inside writerSN.
    let bytes = concat(&[
        &SENDER_HEADER,
        &[0x15, 0x03, 0x18, 0x00], // flags E | Q
        &[0x00, 0x00],
        &[0x08, 0x00], // octetsToInlineQos = 8
        &UNKNOWN_OCTETS,
        &WRITER_OCTETS,
        &[0x00, 0x00, 0x00, 0x00],
        &[0x01, 0x00, 0x00, 0x00],
        &[0x01, 0x00, 0x00, 0x00],
    ]);
    assert_eq!(
        Message::decode(&bytes).map(|_| ()),
        Err(RtpsError::InvalidOctetsToInlineQos {
            id: 0x15,
            declared: 8,
            minimum: 16,
        })
    );
}

#[test]
fn a_sequence_number_set_wider_than_the_ceiling_is_refused() {
    // §9.4.2.6 caps numBits at 256; 0x2000 would make a reader loop over
    // 256 bitmap words that are not there.
    let bytes = concat(&[
        &SENDER_HEADER,
        &[0x06, 0x01, 0x1c, 0x00],
        &UNKNOWN_OCTETS,
        &WRITER_OCTETS,
        &[0x00, 0x00, 0x00, 0x00],
        &[0x01, 0x00, 0x00, 0x00],
        &[0x00, 0x20, 0x00, 0x00], // numBits = 0x2000
        &[0x00, 0x00, 0x00, 0x00],
        &[0x00, 0x00, 0x00, 0x00],
    ]);
    assert_eq!(
        Message::decode(&bytes).map(|_| ()),
        Err(RtpsError::InvalidSequenceNumberSet {
            reason: SetDefect::NumBitsTooLarge { declared: 0x2000 },
            context: "ACKNACK readerSNState",
        })
    );
}

#[test]
fn a_submessage_that_would_start_off_the_four_octet_grid_stops_the_parse() {
    // §8.3.3. A PAD declaring a two-octet body puts the next header at
    // offset 26.
    let bytes = concat(&[
        &SENDER_HEADER,
        &[0x01, 0x01, 0x02, 0x00],
        &[0xaa, 0xbb],
        &[0x01, 0x01, 0x00, 0x00],
    ]);
    assert_eq!(
        Message::decode(&bytes).map(|_| ()),
        Err(RtpsError::MisalignedSubmessage { offset: 26 })
    );
}

#[test]
fn an_unknown_submessage_id_is_skipped_and_the_rest_still_parses() {
    // §8.3.4.1: a receiver ignores a submessage id it does not know and
    // continues with the next. AstRS keeps the octets so the message can be
    // forwarded unchanged.
    let bytes = concat(&[
        &SENDER_HEADER,
        &[0x88, 0x01, 0x04, 0x00], // a vendor-specific id
        &[0xde, 0xad, 0xbe, 0xef],
        &[0x0e, 0x01, 0x0c, 0x00], // INFO_DST, still parsed
        &PEER.to_bytes(),
    ]);
    let message = Message::decode(&bytes).expect("the unknown id is not fatal");
    assert_eq!(message.submessages.len(), 2);
    assert_eq!(message.submessages[0].id().raw(), 0x88);
    assert!(message.submessages[0].id().is_vendor_specific());
    assert_eq!(message.submessages[1].id().name(), "INFO_DST");
    assert_eq!(message.encode().expect("re-encode"), bytes);
}
