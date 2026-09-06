//! Golden packets: a `DATA_FRAG` series and its repair.
//!
//! A 2 500-octet sample cut into 1 000-octet fragments is three fragments
//! long — 1 000, 1 000, and a short last one of 500 (§8.3.7.3). The four
//! datagrams below are the whole transaction:
//!
//! 1. `DATA_FRAG` carrying fragments 1 and 2;
//! 2. `DATA_FRAG` carrying fragment 3, the short one;
//! 3. `HEARTBEAT_FRAG` announcing that all three exist (§8.3.7.6);
//! 4. `NACK_FRAG` asking for fragment 2 again (§8.3.7.10).
//!
//! The encapsulation header belongs to the *sample*, not to each fragment:
//! `sampleSize` counts the whole `serializedPayload` including it, so it
//! appears once, at the front of fragment 1.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use astrs_rtps::messages::{
    DataFrag, FragmentGeometry, HeartbeatFrag, Message, NackFrag, SerializedPayload,
};
use astrs_rtps::structure::{
    EntityId, EntityKind, FragmentNumber, FragmentNumberSet, SequenceNumber,
};

use common::{SENDER, SENDER_HEADER, assert_golden, concat};

/// Octets the whole sample occupies, encapsulation header included.
const SAMPLE_SIZE: u32 = 2_500;

/// Octets each fragment carries, the last one excepted.
const FRAGMENT_SIZE: u16 = 1_000;

const WRITER_OCTETS: [u8; 4] = [0x00, 0x00, 0x01, 0x03];
const READER_OCTETS: [u8; 4] = [0x00, 0x00, 0x02, 0x04];

fn writer_id() -> EntityId {
    EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY)
}

fn reader_id() -> EntityId {
    EntityId::user_defined(2, EntityKind::USER_READER_NO_KEY)
}

/// The sample's `serializedPayload`: a `CDR_LE` header, then a repeating
/// pattern so a mis-copied fragment shows up as a shifted offset rather than
/// as a run of identical octets.
fn sample() -> Vec<u8> {
    let mut octets = Vec::with_capacity(SAMPLE_SIZE as usize);
    octets.extend_from_slice(&[0x00, 0x01, 0x00, 0x00]);
    for index in 4..SAMPLE_SIZE as usize {
        octets.push((index % 251) as u8);
    }
    octets
}

/// The twelve octets of `DATA_FRAG` geometry, little-endian.
fn geometry_octets(starting: u32, count: u16) -> Vec<u8> {
    concat(&[
        &starting.to_le_bytes(),
        &count.to_le_bytes(),
        &FRAGMENT_SIZE.to_le_bytes(),
        &SAMPLE_SIZE.to_le_bytes(),
    ])
}

/// The whole `DATA_FRAG` prelude: the fixed fields before the payload.
fn data_frag_prelude(body_len: u16, starting: u32, count: u16) -> Vec<u8> {
    concat(&[
        // id 0x16, flags 0x01 (E only — no Q, no K, no N).
        &[0x16, 0x01],
        &body_len.to_le_bytes(),
        // extraFlags — reserved, zero.
        &[0x00, 0x00],
        // octetsToInlineQos = 28: readerId(4) + writerId(4) + writerSN(8) +
        // fragmentStartingNum(4) + fragmentsInSubmessage(2) +
        // fragmentSize(2) + sampleSize(4). Twelve more than a DATA's 16.
        &[0x1c, 0x00],
        &READER_OCTETS,
        &WRITER_OCTETS,
        // writerSN = 1.
        &[0x00, 0x00, 0x00, 0x00],
        &[0x01, 0x00, 0x00, 0x00],
        &geometry_octets(starting, count),
    ])
}

#[test]
fn the_first_data_frag_carries_two_full_fragments() {
    let sample = sample();
    let payload = &sample[..2 * FRAGMENT_SIZE as usize];
    let message = Message::from_participant(SENDER).with(DataFrag::new(
        reader_id(),
        writer_id(),
        SequenceNumber::FIRST,
        FragmentGeometry::new(FragmentNumber::FIRST, 2, FRAGMENT_SIZE, SAMPLE_SIZE),
        SerializedPayload::new(payload),
    ));

    // Body = 32 fixed octets + 2 000 payload = 2 032 = 0x07f0.
    let body_len = 32 + 2 * FRAGMENT_SIZE;
    assert_eq!(body_len, 2_032);
    let expected = concat(&[&SENDER_HEADER, &data_frag_prelude(body_len, 1, 2), payload]);
    assert_eq!(expected.len(), 20 + 4 + usize::from(body_len));
    assert_golden(&message, &expected);

    let frag = message.submessages[0].as_data_frag().expect("DATA_FRAG");
    assert_eq!(frag.total_fragments(), Some(3));
    assert_eq!(frag.payload_offset(), 0);
    assert_eq!(frag.fragment_end(), 3, "carries 1 and 2, so ends before 3");
    assert_eq!(frag.validate(), Ok(()));
}

#[test]
fn the_last_data_frag_is_short_and_still_valid() {
    let sample = sample();
    let payload = &sample[2 * FRAGMENT_SIZE as usize..];
    assert_eq!(payload.len(), 500, "2500 - 2*1000");

    let message = Message::from_participant(SENDER).with(DataFrag::new(
        reader_id(),
        writer_id(),
        SequenceNumber::FIRST,
        FragmentGeometry::new(FragmentNumber::new(3), 1, FRAGMENT_SIZE, SAMPLE_SIZE),
        SerializedPayload::new(payload),
    ));

    // Body = 32 + 500 = 532 = 0x0214.
    let body_len = 32 + 500_u16;
    let expected = concat(&[&SENDER_HEADER, &data_frag_prelude(body_len, 3, 1), payload]);
    assert_golden(&message, &expected);

    let frag = message.submessages[0].as_data_frag().expect("DATA_FRAG");
    // §8.3.7.3.3 lets only the sample's last fragment be shorter than
    // fragmentSize, and this one is it.
    assert_eq!(frag.fragment_end().saturating_sub(1), 3);
    assert_eq!(frag.total_fragments(), Some(3));
    assert_eq!(frag.payload_offset(), 2_000);
    assert_eq!(frag.validate(), Ok(()));
}

#[test]
fn the_two_data_frags_reassemble_into_the_original_sample() {
    let sample = sample();
    let first = &sample[..2 * FRAGMENT_SIZE as usize];
    let last = &sample[2 * FRAGMENT_SIZE as usize..];

    let mut reassembled = vec![0_u8; SAMPLE_SIZE as usize];
    for (payload, starting, count) in [(first, 1_u32, 2_u16), (last, 3, 1)] {
        let frag = DataFrag::new(
            reader_id(),
            writer_id(),
            SequenceNumber::FIRST,
            FragmentGeometry::new(
                FragmentNumber::new(starting),
                count,
                FRAGMENT_SIZE,
                SAMPLE_SIZE,
            ),
            SerializedPayload::new(payload),
        );
        let datagram = Message::from_participant(SENDER)
            .with(frag)
            .encode()
            .expect("encode");
        let decoded = Message::decode(&datagram).expect("decode");
        let frag = decoded.submessages[0].as_data_frag().expect("DATA_FRAG");
        let offset = frag.payload_offset() as usize;
        let octets = frag.payload.as_slice();
        reassembled[offset..offset + octets.len()].copy_from_slice(octets);
    }
    assert_eq!(reassembled, sample);
}

#[test]
fn the_heartbeat_frag_announces_the_highest_fragment_available() {
    let message = Message::from_participant(SENDER).with(HeartbeatFrag::new(
        reader_id(),
        writer_id(),
        SequenceNumber::FIRST,
        FragmentNumber::new(3),
        1,
    ));

    let expected = concat(&[
        &SENDER_HEADER,
        // id 0x13, flags 0x01. §8.3.7.6.1 defines no flag but E.
        // octetsToNextHeader = 24: readerId(4) + writerId(4) + writerSN(8) +
        // lastFragmentNum(4) + count(4).
        &[0x13, 0x01, 0x18, 0x00],
        &READER_OCTETS,
        &WRITER_OCTETS,
        &[0x00, 0x00, 0x00, 0x00],
        &[0x01, 0x00, 0x00, 0x00],
        // lastFragmentNum = 3 (FragmentNumber_t is an unsigned long).
        &[0x03, 0x00, 0x00, 0x00],
        // count = 1.
        &[0x01, 0x00, 0x00, 0x00],
    ]);
    assert_eq!(expected.len(), 20 + 4 + 24);
    assert_golden(&message, &expected);
}

#[test]
fn the_nack_frag_asks_for_one_fragment_back() {
    let state = FragmentNumberSet::from_numbers(FragmentNumber::new(2), [FragmentNumber::new(2)])
        .expect("within the window");
    let message = Message::from_participant(SENDER).with(NackFrag::new(
        reader_id(),
        writer_id(),
        SequenceNumber::FIRST,
        state,
        1,
    ));

    let expected = concat(&[
        &SENDER_HEADER,
        // id 0x12, flags 0x01. octetsToNextHeader = 32: readerId(4) +
        // writerId(4) + writerSN(8) + fragmentNumberState(12) + count(4).
        &[0x12, 0x01, 0x20, 0x00],
        &READER_OCTETS,
        &WRITER_OCTETS,
        &[0x00, 0x00, 0x00, 0x00],
        &[0x01, 0x00, 0x00, 0x00],
        // The set: bitmapBase is a FragmentNumber_t — four octets, not the
        // eight a SequenceNumberSet's base takes.
        &[0x02, 0x00, 0x00, 0x00],
        // numBits = 1, so one bitmap word follows.
        &[0x01, 0x00, 0x00, 0x00],
        // Bit 0 is the most significant bit of the word: 0x80000000.
        &[0x00, 0x00, 0x00, 0x80],
        // count = 1.
        &[0x01, 0x00, 0x00, 0x00],
    ]);
    assert_eq!(expected.len(), 20 + 4 + 32);
    assert_golden(&message, &expected);

    let Some(astrs_rtps::messages::Submessage::NackFrag(nack)) = message.submessages.first() else {
        unreachable!("the fixture holds one NACK_FRAG")
    };
    let missing: Vec<u32> = nack.missing().map(FragmentNumber::value).collect();
    assert_eq!(missing, [2]);
    assert_eq!(nack.validate(), Ok(()));
}

#[test]
fn a_sample_that_divides_evenly_has_no_short_fragment() {
    // 2 000 octets in 1 000-octet fragments: exactly two, both full.
    let payload = vec![0x5a_u8; 1_000];
    let last = DataFrag::new(
        reader_id(),
        writer_id(),
        SequenceNumber::FIRST,
        FragmentGeometry::new(FragmentNumber::new(2), 1, FRAGMENT_SIZE, 2_000),
        SerializedPayload::new(payload.clone()),
    );
    assert_eq!(last.total_fragments(), Some(2));
    assert_eq!(last.payload_offset(), 1_000);
    assert_eq!(last.validate(), Ok(()));

    // A *non-final* fragment must be exactly fragmentSize octets, so one
    // octet short is refused …
    let mut first = last.clone();
    first.fragment_starting_num = FragmentNumber::FIRST;
    first.payload = SerializedPayload::new(vec![0x5a_u8; 999]);
    assert_eq!(
        first.validate(),
        Err(astrs_rtps::RtpsError::InvalidFragmentGeometry {
            reason: astrs_rtps::FragmentDefect::PayloadTooShort {
                needed: 1_000,
                available: 999,
            },
        })
    );

    // … while the sample's last fragment may be anything from one octet up,
    // because §8.3.7.3.3 bounds only the fragments that precede it.
    let mut short_last = last;
    short_last.payload = SerializedPayload::new(vec![0x5a_u8; 4]);
    assert_eq!(short_last.validate(), Ok(()));
}
