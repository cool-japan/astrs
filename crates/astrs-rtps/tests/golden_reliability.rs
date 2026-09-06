//! Golden packets: one complete reliable exchange.
//!
//! Three datagrams, in the order they cross the wire, each byte-exact:
//!
//! 1. the writer announces `[1, 5]` with a **HEARTBEAT** and asks for an
//!    answer (§8.3.7.5);
//! 2. the reader answers with an **ACKNACK** — "I have everything below 3,
//!    and I am missing 3 and 5" (§8.3.7.1);
//! 3. the writer answers the repair request with a **GAP** — 3 and 4 no
//!    longer exist — and a final **HEARTBEAT** narrowing its history
//!    (§8.3.7.4).
//!
//! Every message is preceded by an `INFO_DST`, which is how a writer that
//! sends over multicast addresses one specific peer (§8.3.7.7).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use astrs_rtps::messages::{AckNack, Gap, Header, Heartbeat, InfoDestination, Message};
use astrs_rtps::structure::{EntityId, EntityKind, SequenceNumber, SequenceNumberSet};

use common::{PEER, SENDER, SENDER_HEADER, assert_golden, concat};

/// The user-defined writer: counter 1, `USER_WRITER_NO_KEY` (`0x03`).
fn writer_id() -> EntityId {
    EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY)
}

/// The user-defined reader: counter 2, `USER_READER_NO_KEY` (`0x04`).
fn reader_id() -> EntityId {
    EntityId::user_defined(2, EntityKind::USER_READER_NO_KEY)
}

/// `00 00 01 03` — the three key octets, then the kind.
const WRITER_OCTETS: [u8; 4] = [0x00, 0x00, 0x01, 0x03];
/// `00 00 02 04`.
const READER_OCTETS: [u8; 4] = [0x00, 0x00, 0x02, 0x04];

/// A twenty-octet header from [`PEER`].
const PEER_HEADER: [u8; 20] = [
    0x52, 0x54, 0x50, 0x53, 0x02, 0x03, 0x41, 0x53, 0x41, 0x53, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10,
    0x11, 0x12, 0x13, 0x14,
];

#[test]
fn step_one_the_writer_announces_what_it_holds() {
    let message = Message::from_participant(SENDER)
        .with(InfoDestination::new(PEER))
        .with(Heartbeat::new(
            reader_id(),
            writer_id(),
            SequenceNumber::FIRST,
            SequenceNumber::new(5),
            1,
        ));

    let expected = concat(&[
        &SENDER_HEADER,
        // INFO_DST: id 0x0e, flags 0x01 (E), octetsToNextHeader 12. The body
        // is twelve octets of guidPrefix, which have no byte order.
        &[0x0e, 0x01, 0x0c, 0x00],
        &PEER.to_bytes(),
        // HEARTBEAT: id 0x07, flags 0x01 — E set, F clear, so the writer *is*
        // soliciting an ACKNACK. octetsToNextHeader = 28: readerId(4) +
        // writerId(4) + firstSN(8) + lastSN(8) + count(4).
        &[0x07, 0x01, 0x1c, 0x00],
        &READER_OCTETS,
        &WRITER_OCTETS,
        // firstSN = 1: high (long) 0, low (unsigned long) 1, little-endian.
        &[0x00, 0x00, 0x00, 0x00],
        &[0x01, 0x00, 0x00, 0x00],
        // lastSN = 5.
        &[0x00, 0x00, 0x00, 0x00],
        &[0x05, 0x00, 0x00, 0x00],
        // count = 1 (Count_t is a long).
        &[0x01, 0x00, 0x00, 0x00],
    ]);
    assert_eq!(expected.len(), 20 + 16 + 32);
    assert_golden(&message, &expected);

    let heartbeat = message.submessages[1].as_heartbeat().expect("HEARTBEAT");
    assert!(!heartbeat.is_final, "an answer is wanted");
    assert_eq!(heartbeat.available(), 5);
    assert_eq!(heartbeat.validate(), Ok(()));
}

#[test]
fn step_two_the_reader_acknowledges_two_and_asks_for_three_and_five() {
    // bitmapBase = 3 is the positive acknowledgement: 1 and 2 arrived. The
    // bits then name what did not: bit 0 is 3, bit 1 is 4, bit 2 is 5.
    let state = SequenceNumberSet::from_numbers(
        SequenceNumber::new(3),
        [SequenceNumber::new(3), SequenceNumber::new(5)],
    )
    .expect("within the 256-bit window");
    let message = Message::new(Header::new(PEER))
        .with(InfoDestination::new(SENDER))
        .with(AckNack::new(reader_id(), writer_id(), state, 1));

    let expected = concat(&[
        &PEER_HEADER,
        &[0x0e, 0x01, 0x0c, 0x00],
        &SENDER.to_bytes(),
        // ACKNACK: id 0x06, flags 0x01 — E set, F clear, so this is a repair
        // request rather than a status report. octetsToNextHeader = 28:
        // readerId(4) + writerId(4) + readerSNState(16) + count(4).
        &[0x06, 0x01, 0x1c, 0x00],
        &READER_OCTETS,
        &WRITER_OCTETS,
        // readerSNState.bitmapBase = 3.
        &[0x00, 0x00, 0x00, 0x00],
        &[0x03, 0x00, 0x00, 0x00],
        // numBits = 3, so one bitmap word follows: ceil(3/32) = 1.
        &[0x03, 0x00, 0x00, 0x00],
        // Bit i is bitmapBase + i, most significant bit of the word first:
        // bit 0 (=3) is 0x80000000 and bit 2 (=5) is 0x20000000, so the word
        // is 0xa0000000 — little-endian, 00 00 00 a0.
        &[0x00, 0x00, 0x00, 0xa0],
        // count = 1.
        &[0x01, 0x00, 0x00, 0x00],
    ]);
    assert_eq!(expected.len(), 20 + 16 + 32);
    assert_golden(&message, &expected);

    let acknack = message.submessages[1].as_acknack().expect("ACKNACK");
    assert_eq!(acknack.acknowledged_through(), SequenceNumber::new(2));
    let missing: Vec<i64> = acknack.missing().map(SequenceNumber::value).collect();
    assert_eq!(missing, [3, 5]);
    assert!(!acknack.is_pure_ack());
    assert_eq!(acknack.validate(), Ok(()));
}

#[test]
fn step_three_the_writer_declares_the_gap_and_narrows_its_history() {
    let message = Message::from_participant(SENDER)
        .with(InfoDestination::new(PEER))
        .with(Gap::contiguous(
            reader_id(),
            writer_id(),
            SequenceNumber::new(3),
            SequenceNumber::new(4),
        ))
        .with(
            Heartbeat::new(
                reader_id(),
                writer_id(),
                SequenceNumber::new(5),
                SequenceNumber::new(5),
                2,
            )
            .finalized(),
        );

    let expected = concat(&[
        &SENDER_HEADER,
        &[0x0e, 0x01, 0x0c, 0x00],
        &PEER.to_bytes(),
        // GAP: id 0x08, flags 0x01. octetsToNextHeader = 28: readerId(4) +
        // writerId(4) + gapStart(8) + gapList(12).
        &[0x08, 0x01, 0x1c, 0x00],
        &READER_OCTETS,
        &WRITER_OCTETS,
        // gapStart = 3: the run of irrelevant numbers begins here.
        &[0x00, 0x00, 0x00, 0x00],
        &[0x03, 0x00, 0x00, 0x00],
        // gapList.bitmapBase = 5 ends the run at 4, and numBits = 0 means no
        // scattered numbers follow — so the whole message is "3 and 4".
        &[0x00, 0x00, 0x00, 0x00],
        &[0x05, 0x00, 0x00, 0x00],
        &[0x00, 0x00, 0x00, 0x00],
        // HEARTBEAT: flags 0x03 = E | F. The F flag says no further ACKNACK
        // is solicited; the repair is done.
        &[0x07, 0x03, 0x1c, 0x00],
        &READER_OCTETS,
        &WRITER_OCTETS,
        &[0x00, 0x00, 0x00, 0x00],
        &[0x05, 0x00, 0x00, 0x00],
        &[0x00, 0x00, 0x00, 0x00],
        &[0x05, 0x00, 0x00, 0x00],
        &[0x02, 0x00, 0x00, 0x00],
    ]);
    assert_eq!(expected.len(), 20 + 16 + 32 + 32);
    assert_golden(&message, &expected);

    let gap = message.submessages[1].as_gap().expect("GAP");
    let gone: Vec<i64> = gap.irrelevant().map(SequenceNumber::value).collect();
    assert_eq!(gone, [3, 4]);
    assert!(gap.covers(SequenceNumber::new(4)));
    assert!(!gap.covers(SequenceNumber::new(5)));

    let heartbeat = message.submessages[2].as_heartbeat().expect("HEARTBEAT");
    assert!(heartbeat.is_final);
    assert_eq!(heartbeat.available(), 1);
}

#[test]
fn the_reader_then_acknowledges_everything_with_an_empty_set() {
    // The steady state: bitmapBase one past the last sample received, no bits
    // set, F flag on. Twelve octets of readerSNState, no bitmap words.
    let message = Message::new(Header::new(PEER))
        .with(InfoDestination::new(SENDER))
        .with(
            AckNack::new(
                reader_id(),
                writer_id(),
                SequenceNumberSet::new(SequenceNumber::new(6)),
                2,
            )
            .finalized(),
        );

    let expected = concat(&[
        &PEER_HEADER,
        &[0x0e, 0x01, 0x0c, 0x00],
        &SENDER.to_bytes(),
        // octetsToNextHeader = 24: readerSNState is only twelve octets when
        // numBits is zero, because ceil(0/32) = 0 bitmap words follow.
        &[0x06, 0x03, 0x18, 0x00],
        &READER_OCTETS,
        &WRITER_OCTETS,
        &[0x00, 0x00, 0x00, 0x00],
        &[0x06, 0x00, 0x00, 0x00],
        &[0x00, 0x00, 0x00, 0x00],
        &[0x02, 0x00, 0x00, 0x00],
    ]);
    assert_eq!(expected.len(), 20 + 16 + 28);
    assert_golden(&message, &expected);

    let acknack = message.submessages[1].as_acknack().expect("ACKNACK");
    assert!(acknack.is_pure_ack());
    assert!(acknack.is_final);
    assert_eq!(acknack.acknowledged_through(), SequenceNumber::new(5));
}

#[test]
fn a_liveliness_heartbeat_sets_the_l_flag_and_nothing_else_changes() {
    // §8.3.7.5: the L flag says the heartbeat exists to assert that the
    // writer is alive, not to announce history. The body is unchanged.
    let message = Message::from_participant(SENDER).with(
        Heartbeat::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            SequenceNumber::ZERO,
            9,
        )
        .finalized()
        .asserting_liveliness(),
    );

    let expected = concat(&[
        &SENDER_HEADER,
        // flags 0x07 = E | F | L.
        &[0x07, 0x07, 0x1c, 0x00],
        &[0x00, 0x00, 0x00, 0x00], // ENTITYID_UNKNOWN: every reader
        &WRITER_OCTETS,
        // firstSN = 1, lastSN = 0: the empty history §8.3.7.5.3 allows.
        &[0x00, 0x00, 0x00, 0x00],
        &[0x01, 0x00, 0x00, 0x00],
        &[0x00, 0x00, 0x00, 0x00],
        &[0x00, 0x00, 0x00, 0x00],
        &[0x09, 0x00, 0x00, 0x00],
    ]);
    assert_golden(&message, &expected);

    let heartbeat = message.submessages[0].as_heartbeat().expect("HEARTBEAT");
    assert!(heartbeat.liveliness);
    assert!(heartbeat.is_empty_history());
    assert_eq!(heartbeat.available(), 0);
    assert_eq!(heartbeat.validate(), Ok(()));
}
