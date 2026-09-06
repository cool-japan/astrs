//! `DURABILITY`: who is entitled to what a writer wrote before they arrived.
//!
//! DDS 1.4 §2.2.3.4 puts the decision on *both* endpoints and RTPS §8.4.9.1
//! expresses it in one place — the writer's `ReaderProxy` watermarks. The
//! writer's policy says whether the samples are still there; the reader's says
//! whether it wants them. These tests pin down all four combinations, the
//! bound the history policy puts on a replay, and the one interaction that is
//! easy to get wrong: a `KEEP_ALL` + `TRANSIENT_LOCAL` writer must not release
//! history just because every reader alive *today* has acknowledged it.
//!
//! The synchronous half drives [`RtpsWriter`] directly, with no runtime and no
//! socket. The asynchronous half proves the same thing end to end, over real
//! UDP, through SEDP — because the writer can only honour the remote reader's
//! `DURABILITY` if discovery actually carried it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use std::time::{Duration, Instant};

use astrs_rtps::behavior::endpoint::TopicKey;
use astrs_rtps::behavior::{Outbound, ReaderProxy, RtpsWriter, WriterConfig};
use astrs_rtps::discovery::qos::{DurabilityQos, HistoryQos, ReliabilityQos, ResourceLimitsQos};
use astrs_rtps::discovery::{ReaderQos, WriterQos};
use astrs_rtps::messages::Message;
use astrs_rtps::structure::{EntityId, EntityKind, Guid, GuidPrefix, Locator, SequenceNumber};

use harness::{PATIENCE, Pair, await_matched, payload, topic};

// ---------------------------------------------------------------------------
// The synchronous half: one writer, no runtime
// ---------------------------------------------------------------------------

fn writer_guid() -> Guid {
    Guid::new(
        GuidPrefix::new([1; 12]),
        EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
    )
}

fn reader_guid(key: u32) -> Guid {
    Guid::new(
        GuidPrefix::new([2; 12]),
        EntityId::user_defined(key, EntityKind::USER_READER_NO_KEY),
    )
}

fn local_topic() -> TopicKey {
    TopicKey::new("rt/durability", "astrs::msg::dds_::Blob_").expect("the names are valid")
}

/// A reliable reader proxy that either does or does not want history.
fn proxy(key: u32, wants_history: bool) -> ReaderProxy {
    ReaderProxy::new(
        reader_guid(key),
        vec![Locator::udpv4(std::net::Ipv4Addr::LOCALHOST, 7_500)],
        true,
    )
    .wanting_history(wants_history)
}

fn writer_with(qos: WriterQos) -> RtpsWriter {
    RtpsWriter::new(WriterConfig::new(writer_guid(), local_topic()).with_qos(qos))
}

/// Every sequence number a batch of datagrams carries a `DATA` for.
fn data_numbers(outbound: &[Outbound]) -> Vec<i64> {
    let mut numbers = Vec::new();
    for item in outbound {
        let message = Message::decode(&item.datagram).expect("the writer's own datagram decodes");
        for submessage in message.iter() {
            if let Some(data) = submessage.as_data() {
                numbers.push(data.writer_sn.value());
            }
        }
    }
    numbers.sort_unstable();
    numbers.dedup();
    numbers
}

/// The sequence numbers a `GAP` declared irrelevant.
fn gapped_numbers(outbound: &[Outbound]) -> Vec<i64> {
    let mut numbers = Vec::new();
    for item in outbound {
        let message = Message::decode(&item.datagram).expect("decode");
        for submessage in message.iter() {
            if let Some(gap) = submessage.as_gap() {
                numbers.extend(gap.irrelevant().map(SequenceNumber::value));
            }
        }
    }
    numbers.sort_unstable();
    numbers.dedup();
    numbers
}

fn write_n(writer: &mut RtpsWriter, count: u8, now: Instant) {
    for tag in 1..=count {
        writer.write(vec![tag; 8], None, now).expect("write");
    }
}

#[test]
fn the_four_durability_combinations_decide_the_replay() {
    // The writer's policy and the reader's are both gates, and the sample is
    // replayed only when both are open.
    for (writer_durable, reader_durable, expected) in [
        (true, true, 3_usize),
        (true, false, 0),
        (false, true, 0),
        (false, false, 0),
    ] {
        let qos = WriterQos {
            reliability: ReliabilityQos::reliable(),
            durability: if writer_durable {
                DurabilityQos::transient_local()
            } else {
                DurabilityQos::volatile()
            },
            history: HistoryQos::keep_last(10),
            ..WriterQos::default()
        };
        let mut writer = writer_with(qos);
        let now = Instant::now();
        write_n(&mut writer, 3, now);

        let late = proxy(1, reader_durable);
        assert_eq!(
            writer.replays_history_to(&late),
            expected > 0,
            "writer durable {writer_durable}, reader durable {reader_durable}"
        );
        writer.match_reader(late);

        let numbers = data_numbers(&writer.produce(now).expect("produce"));
        assert_eq!(
            numbers.len(),
            expected,
            "writer durable {writer_durable}, reader durable {reader_durable}: \
             got {numbers:?}"
        );

        // Whatever came before, everything written from now on arrives.
        writer.write(vec![0x99; 8], None, now).expect("write");
        assert_eq!(
            data_numbers(&writer.produce(now).expect("produce")),
            vec![4],
            "a matched reader always gets what is written after it matched"
        );
    }
}

#[test]
fn one_volatile_reader_does_not_cost_the_next_reader_its_history() {
    // The interesting asymmetry: a TRANSIENT_LOCAL writer keeps its history
    // even while serving a VOLATILE reader, because the *next* reader may
    // want it.
    let mut writer = writer_with(WriterQos::latched(10));
    let now = Instant::now();
    write_n(&mut writer, 3, now);

    writer.match_reader(proxy(1, false));
    assert!(
        data_numbers(&writer.produce(now).expect("produce")).is_empty(),
        "the VOLATILE reader starts at the present"
    );
    assert_eq!(writer.cache().len(), 3, "the history is untouched");

    writer.match_reader(proxy(2, true));
    assert_eq!(
        data_numbers(&writer.produce(now).expect("produce")),
        vec![1, 2, 3],
        "and the TRANSIENT_LOCAL reader that follows gets all of it"
    );
}

#[test]
fn a_bounded_history_replays_its_depth_and_gaps_the_rest() {
    // KEEP_LAST n is the bound on TRANSIENT_LOCAL: the late joiner gets the
    // last n and is told, once, that the rest is never coming. Without the
    // GAP a reliable reader would nack samples 1..=6 forever.
    let mut writer = writer_with(WriterQos::latched(4));
    let now = Instant::now();
    write_n(&mut writer, 10, now);
    assert_eq!(writer.cache().len(), 4, "KEEP_LAST 4 holds four");

    writer.match_reader(proxy(1, true));
    let outbound = writer.produce(now).expect("produce");
    assert_eq!(
        data_numbers(&outbound),
        vec![7, 8, 9, 10],
        "only what the history still holds can be replayed"
    );
    assert_eq!(
        gapped_numbers(&outbound),
        vec![1, 2, 3, 4, 5, 6],
        "and everything evicted is GAPped rather than left to be nacked"
    );
}

#[test]
fn resource_limits_bound_a_keep_all_transient_local_history() {
    // KEEP_ALL + TRANSIENT_LOCAL grows on purpose. RESOURCE_LIMITS is the
    // bound, and it is a refusal to write rather than a silent eviction: a
    // sample dropped here would be one a late joiner was promised.
    let mut writer = writer_with(WriterQos {
        reliability: ReliabilityQos::reliable(),
        durability: DurabilityQos::transient_local(),
        history: HistoryQos::keep_all(),
        resource_limits: ResourceLimitsQos {
            max_samples: 4,
            ..ResourceLimitsQos::default()
        },
        ..WriterQos::default()
    });
    let now = Instant::now();
    write_n(&mut writer, 4, now);
    let refused = writer.write(vec![5; 8], None, now);
    assert!(
        refused.is_err(),
        "the fifth sample must be refused, not evicted"
    );
    assert_eq!(writer.cache().len(), 4);

    writer.match_reader(proxy(1, true));
    assert_eq!(
        data_numbers(&writer.produce(now).expect("produce")),
        vec![1, 2, 3, 4],
        "and all four are still there for the late joiner"
    );
}

#[test]
fn reclaiming_never_takes_history_a_transient_local_writer_promised() {
    // The bug this test exists for: `reclaim` drops what every matched reader
    // has acknowledged, which is correct for VOLATILE and catastrophic for
    // TRANSIENT_LOCAL — the reader that has not appeared yet has, by
    // definition, acknowledged nothing and asked for nothing.
    let mut durable = writer_with(WriterQos {
        reliability: ReliabilityQos::reliable(),
        durability: DurabilityQos::transient_local(),
        history: HistoryQos::keep_all(),
        ..WriterQos::default()
    });
    let now = Instant::now();
    write_n(&mut durable, 3, now);

    // Reader one joins, is replayed everything, and acknowledges it all.
    durable.match_reader(proxy(1, true));
    assert_eq!(
        data_numbers(&durable.produce(now).expect("produce")),
        vec![1, 2, 3]
    );
    let acknack = astrs_rtps::messages::AckNack::new(
        reader_guid(1).entity_id,
        writer_guid().entity_id,
        astrs_rtps::structure::SequenceNumberSet::new(SequenceNumber::new(4)),
        1,
    );
    assert!(durable.on_acknack(reader_guid(1), &acknack));
    assert!(durable.is_acknowledged());

    assert_eq!(
        durable.reclaim(),
        0,
        "a TRANSIENT_LOCAL writer reclaims nothing"
    );
    assert_eq!(durable.cache().len(), 3);

    // Reader two joins afterwards and is still entitled to all three.
    durable.match_reader(proxy(2, true));
    assert_eq!(
        data_numbers(&durable.produce(now).expect("produce")),
        vec![1, 2, 3],
        "the history survived the first reader's acknowledgement"
    );

    // The same writer with VOLATILE durability does reclaim, which is what
    // makes the guard above a policy decision rather than a dead branch.
    let mut volatile = writer_with(WriterQos {
        reliability: ReliabilityQos::reliable(),
        durability: DurabilityQos::volatile(),
        history: HistoryQos::keep_all(),
        ..WriterQos::default()
    });
    volatile.match_reader(proxy(1, true));
    write_n(&mut volatile, 3, now);
    let _ = volatile.produce(now).expect("produce");
    assert!(volatile.on_acknack(reader_guid(1), &acknack));
    assert_eq!(volatile.reclaim(), 3, "a VOLATILE writer releases them");
    assert_eq!(volatile.cache().len(), 0);
}

#[test]
fn a_best_effort_transient_local_writer_still_replays_once() {
    // TRANSIENT_LOCAL does not require RELIABLE. There is no ACKNACK to drive
    // the replay, so it happens on the first `produce` after matching and
    // never again.
    let mut writer = writer_with(WriterQos {
        reliability: ReliabilityQos::best_effort(),
        durability: DurabilityQos::transient_local(),
        history: HistoryQos::keep_last(10),
        ..WriterQos::default()
    });
    let now = Instant::now();
    write_n(&mut writer, 3, now);
    writer.match_reader(proxy(1, true));

    assert_eq!(
        data_numbers(&writer.produce(now).expect("produce")),
        vec![1, 2, 3]
    );
    assert!(
        writer.produce(now).expect("produce").is_empty(),
        "and it is not replayed a second time"
    );
}

// ---------------------------------------------------------------------------
// The asynchronous half: two participants, real sockets, real SEDP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_late_joining_reliable_reader_receives_every_pre_join_sample() {
    let pair = Pair::new().await;
    pair.await_discovery().await;

    // Ten samples written with nobody subscribed at all.
    let writer = pair
        .left
        .create_writer(topic(), WriterQos::latched(16))
        .await
        .expect("writer");
    for tag in 1..=10_u8 {
        writer.write(payload(tag)).await.expect("write");
    }
    assert_eq!(writer.matched_readers().await, 0, "nobody was listening");

    let reader = pair
        .right
        .create_reader(topic(), ReaderQos::latched(16))
        .await
        .expect("reader");
    await_matched(&writer).await;

    let samples = tokio::time::timeout(PATIENCE, reader.take_at_least(10))
        .await
        .expect("every pre-join sample must reach the late joiner");
    let mut numbers: Vec<i64> = samples
        .iter()
        .map(|sample| sample.sequence_number.value())
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    assert_eq!(numbers, (1..=10).collect::<Vec<i64>>());
    for sample in &samples {
        let tag = u8::try_from(sample.sequence_number.value()).expect("small");
        assert_eq!(
            sample.as_slice(),
            payload(tag).as_slice(),
            "the replayed octets must be the octets that were written"
        );
    }

    pair.shutdown().await;
}

#[tokio::test]
async fn sedp_carries_the_readers_durability_to_the_writer() {
    // The end-to-end form of `the_four_durability_combinations_decide_the_replay`:
    // the writer is TRANSIENT_LOCAL and still holds its history, but this
    // reader asked for VOLATILE and must be started at the present. Nothing
    // but SEDP tells the writer that.
    let pair = Pair::new().await;
    pair.await_discovery().await;

    let writer = pair
        .left
        .create_writer(topic(), WriterQos::latched(16))
        .await
        .expect("writer");
    for tag in 1..=5_u8 {
        writer.write(payload(tag)).await.expect("write");
    }

    let reader = pair
        .right
        .create_reader(topic(), ReaderQos::reliable(16))
        .await
        .expect("reader");
    await_matched(&writer).await;

    writer.write(payload(0x42)).await.expect("write");
    let sample = reader
        .take_within(PATIENCE)
        .await
        .expect("what is written after matching must arrive");
    assert_eq!(
        sample.sequence_number,
        SequenceNumber::new(6),
        "a VOLATILE reader starts at the present even against a latched writer"
    );
    assert_eq!(sample.as_slice(), payload(0x42).as_slice());
    assert!(
        reader
            .take_within(Duration::from_millis(150))
            .await
            .is_none(),
        "samples 1..=5 must never be replayed to a VOLATILE reader"
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn a_bounded_latched_writer_replays_only_its_depth_to_a_late_joiner() {
    let pair = Pair::new().await;
    pair.await_discovery().await;

    let writer = pair
        .left
        .create_writer(topic(), WriterQos::latched(3))
        .await
        .expect("writer");
    for tag in 1..=9_u8 {
        writer.write(payload(tag)).await.expect("write");
    }

    let reader = pair
        .right
        .create_reader(topic(), ReaderQos::latched(16))
        .await
        .expect("reader");
    await_matched(&writer).await;

    let samples = tokio::time::timeout(PATIENCE, reader.take_at_least(3))
        .await
        .expect("the surviving history must be replayed");
    let mut numbers: Vec<i64> = samples
        .iter()
        .map(|sample| sample.sequence_number.value())
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    assert_eq!(
        numbers,
        vec![7, 8, 9],
        "KEEP_LAST 3 bounds what TRANSIENT_LOCAL can promise"
    );
    assert!(
        reader
            .take_within(Duration::from_millis(150))
            .await
            .is_none(),
        "and the reader does not stall nacking the six that are gone"
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn a_second_late_joiner_gets_the_same_history_as_the_first() {
    // Three participants: the replay is not consumed by whoever asks first.
    let pair = Pair::new().await;
    pair.await_discovery().await;

    let writer = pair
        .left
        .create_writer(topic(), WriterQos::latched(8))
        .await
        .expect("writer");
    for tag in 1..=4_u8 {
        writer.write(payload(tag)).await.expect("write");
    }

    let first = pair
        .right
        .create_reader(topic(), ReaderQos::latched(8))
        .await
        .expect("first reader");
    await_matched(&writer).await;
    let replayed = tokio::time::timeout(PATIENCE, first.take_at_least(4))
        .await
        .expect("the first reader is replayed");
    assert_eq!(replayed.len(), 4);

    let second = pair
        .right
        .create_reader(topic(), ReaderQos::latched(8))
        .await
        .expect("second reader");
    let again = tokio::time::timeout(PATIENCE, second.take_at_least(4))
        .await
        .expect("and so is the second, from the same history");
    let mut numbers: Vec<i64> = again
        .iter()
        .map(|sample| sample.sequence_number.value())
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    assert_eq!(numbers, vec![1, 2, 3, 4]);

    pair.shutdown().await;
}
