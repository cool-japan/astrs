//! Deep-fuzz harness for `astrs-rtps`'s submessage parser (blueprint §10.2,
//! §15).
//!
//! Attack surface: [`Message::decode`] — a twenty-octet header followed by a
//! walk over submessages that must stay four-octet aligned, must stop at
//! [`astrs_rtps::messages::MAX_SUBMESSAGES`], and must treat an unknown `submessageId` as
//! [`astrs_rtps::Submessage::Opaque`] rather than fatal (§8.3.4.1). Every declared
//! length is checked against the bytes actually remaining before anything
//! is allocated (see the message module's own docs) — [`check`] is what
//! hammers that promise.

use astrs_cdr::Endianness;
use astrs_rtps::messages::{
    AckNack, Data, DataPayload, Gap, Header, Heartbeat, InfoDestination, InfoReply, InfoSource,
    InfoTimestamp, Message, MessageBuilder, Opaque, Pad, SerializedPayload, SubmessageFlags,
    SubmessageId,
};
use astrs_rtps::structure::{
    EntityId, EntityKind, GuidPrefix, LocatorList, SequenceNumber, SequenceNumberSet, Time,
};

use crate::support::mutate;
use crate::support::rng::Rng;

/// Per-iteration cap on generated input length — enough room for several
/// submessages, small enough that millions of nightly-lane iterations stay
/// fast.
pub const MAX_INPUT_LEN: usize = 16 * 1024;

/// Where the committed regression corpus for this surface lives.
pub const CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/corpus/rtps");

const PREFIX: GuidPrefix = GuidPrefix::new([0x41, 0x53, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);

fn writer_id() -> EntityId {
    EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY)
}

fn reader_id() -> EntityId {
    EntityId::user_defined(2, EntityKind::USER_READER_NO_KEY)
}

fn sample(sn: i64) -> Data<'static> {
    let payload = SerializedPayload::from_cdr(&(sn as u32))
        .unwrap_or_else(|_| SerializedPayload::new(Vec::new()));
    Data::new(
        EntityId::UNKNOWN,
        writer_id(),
        SequenceNumber::new(sn),
        DataPayload::Data(payload),
    )
}

/// Every message this surface's `seeds()` builds, before encoding.
///
/// A free function (rather than inlined in `seeds()`) so `every_seed_actually_decodes`
/// can walk the same list without re-deriving it from encoded bytes.
fn sample_messages() -> Vec<Message<'static>> {
    vec![
        // A bare header: legal, if useless, per the module docs.
        Message::from_participant(PREFIX),
        // A single sample.
        Message::from_participant(PREFIX).with(sample(1)),
        // A timestamped sample, then a HEARTBEAT — a realistic
        // reliable-writer burst.
        Message::from_participant(PREFIX)
            .with(InfoTimestamp::at(Time::new(7, 0)))
            .with(sample(2))
            .with(Heartbeat::new(
                EntityId::UNKNOWN,
                writer_id(),
                SequenceNumber::FIRST,
                SequenceNumber::new(2),
                1,
            )),
        // An ACKNACK, requesting a repair of an empty base (nothing
        // missing).
        Message::from_participant(PREFIX).with(AckNack::new(
            reader_id(),
            writer_id(),
            SequenceNumberSet::new(SequenceNumber::FIRST),
            1,
        )),
        // A GAP declaring one irrelevant run.
        Message::from_participant(PREFIX).with(Gap::new(
            reader_id(),
            writer_id(),
            SequenceNumber::FIRST,
            SequenceNumberSet::new(SequenceNumber::new(2)),
        )),
        // INFO_SRC + INFO_DST + INFO_REPLY, three of the four INFO_* kinds
        // this build interprets, back to back.
        Message::from_participant(PREFIX)
            .with(InfoSource::new(PREFIX))
            .with(InfoDestination::new(PREFIX))
            .with(InfoReply::new(LocatorList::new())),
        // INFO_TS with the invalidate flag: a zero-length body per
        // §8.3.7.9.4, exercising the "empty body allowed here" exemption.
        Message::from_participant(PREFIX)
            .with(InfoTimestamp::invalidate())
            .with(sample(3)),
        // PAD, then an unknown submessage id carried as Opaque (§8.3.4.1),
        // then a sample — the "forward compatibility" path.
        Message::from_participant(PREFIX)
            .with(Pad::zeros(4))
            .with(Opaque::new(
                SubmessageId::from_raw(0x7a),
                SubmessageFlags::LITTLE_ENDIAN,
                vec![1, 2, 3, 4],
            ))
            .with(sample(4)),
        // The last submessage may declare a body longer than
        // octetsToNextHeader can express and fall back to "runs to the end
        // of the message".
        Message::from_participant(PREFIX).with(Data::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            DataPayload::Data(SerializedPayload::new(vec![0u8; 2_048])),
        )),
        // A big-endian sample, so byte-order handling is exercised by a
        // seed rather than only by mutation.
        Message::from_participant(PREFIX).with(sample(5).with_endianness(Endianness::Big)),
        // DataPayload::None and DataPayload::Key, the two payload shapes
        // that are not DataPayload::Data.
        Message::from_participant(PREFIX).with(Data::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            DataPayload::None,
        )),
        Message::from_participant(PREFIX).with(Data::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            DataPayload::Key(SerializedPayload::new(vec![9u8; 8])),
        )),
    ]
}

/// Valid datagrams from [`Message::encode`], plus one built through
/// [`MessageBuilder`] with a tight budget (the packing logic the behavior
/// half relies on).
#[must_use]
pub fn seeds() -> Vec<Vec<u8>> {
    let mut seeds = Vec::new();
    for message in sample_messages() {
        if let Ok(bytes) = message.encode() {
            seeds.push(bytes);
        }
    }

    let mut builder = MessageBuilder::with_budget(Header::new(PREFIX), 128);
    builder.try_push(Pad::zeros(8));
    builder.try_push(Pad::zeros(16));
    if let Ok(bytes) = builder.encode() {
        seeds.push(bytes);
    }
    seeds
}

/// Unstructured bytes about a quarter of the time; a structure-aware
/// mutation of a randomly chosen seed otherwise.
#[must_use]
pub fn generate(rng: &mut Rng, seeds: &[Vec<u8>]) -> Vec<u8> {
    if rng.one_in(4) {
        return mutate::random_bytes(rng, MAX_INPUT_LEN);
    }
    match rng.pick(seeds) {
        Some(seed) => mutate::mutate(rng, seed, MAX_INPUT_LEN),
        None => mutate::random_bytes(rng, MAX_INPUT_LEN),
    }
}

/// The invariant: [`Message::decode`] returns a value or a typed
/// [`RtpsError`](astrs_rtps::RtpsError) for whatever `bytes` are — never a
/// panic — and a decoded message's submessage count never exceeds what the
/// input could hold (each submessage costs at least a four-octet header).
pub fn check(bytes: &[u8]) {
    match Message::decode(bytes) {
        Ok(message) => {
            assert!(
                message.submessages.len() * 4 <= bytes.len(),
                "decoded more submessages than the input could justify"
            );
            // A decoded message either re-encodes or names why it cannot;
            // either way this must not panic.
            let _ = message.encode();
        }
        Err(error) => {
            assert!(
                !error.to_string().is_empty(),
                "typed errors must have a message"
            );
        }
    }
    // The lazy scan is a second entry point over the same bytes, and must
    // survive the walk even when it stops at the first framing fault.
    if let Ok((_, submessages)) = Message::scan(bytes) {
        for result in submessages {
            if result.is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn every_seed_actually_decodes() {
        let messages = sample_messages();
        assert!(!messages.is_empty());
        for message in &messages {
            let bytes = message.encode().expect("generated messages stay aligned");
            let decoded = Message::decode(&bytes).expect("a rtps seed did not decode");
            assert_eq!(decoded.into_owned(), message.clone());
        }
    }

    #[test]
    fn seeds_is_non_empty() {
        assert!(!seeds().is_empty());
    }

    #[test]
    fn generate_never_exceeds_the_cap() {
        let seeds = seeds();
        let mut rng = Rng::new(321);
        for _ in 0..500 {
            assert!(generate(&mut rng, &seeds).len() <= MAX_INPUT_LEN);
        }
    }

    #[test]
    fn check_never_panics_on_a_handful_of_hand_picked_edge_cases() {
        let header = Header::new(PREFIX).to_bytes();
        let mut misaligned = header.to_vec();
        misaligned.extend_from_slice(&[0x01, 0x01, 0x02, 0x00, 0xaa, 0xbb]);
        misaligned.extend_from_slice(&[0x01, 0x01, 0x00, 0x00]);

        let mut overrun = header.to_vec();
        overrun.extend_from_slice(&[0x07, 0x01, 0xff, 0x00]);
        overrun.extend_from_slice(&[0u8; 8]);

        for case in [
            Vec::new(),
            header.to_vec(),
            misaligned,
            overrun,
            vec![0xFFu8; 64],
        ] {
            check(&case);
        }
    }
}
