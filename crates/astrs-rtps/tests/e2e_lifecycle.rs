//! Departure: what a peer learns when something goes away, and how fast.
//!
//! Every entity in RTPS has two ways of disappearing. It can be **disposed
//! of** — an explicit sample on the builtin topic that names it, which a peer
//! acts on immediately (§8.5.3.1, §8.5.4) — or it can simply stop, and be
//! forgotten when its participant's lease runs out.
//!
//! The second always works and is always slow. The first is what these tests
//! check actually happens, because a graceful departure that is only
//! *received* and never *sent* is indistinguishable from no implementation at
//! all: the code path exists, the tests of the receiving half pass, and two
//! AstRS participants still take a full lease to notice each other leaving.
//! Each test here therefore measures the departure against a lease long
//! enough that a lease-timeout fallback would fail it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use std::time::Duration;

use astrs_rtps::behavior::{Participant, ParticipantConfig};
use astrs_rtps::discovery::{DeadlineQos, ReaderQos, RosCompat, SpdpConfig, WriterQos};
use harness::{
    PATIENCE, Pair, TICK, await_condition, await_matched, config, payload, prefix, topic, wire,
};

/// A lease far longer than any of these tests will wait.
///
/// If a departure is only noticed by lease timeout, the test times out
/// instead of passing slowly — which is the point.
const LONG_LEASE: astrs_rtps::structure::Duration = astrs_rtps::structure::Duration::from_secs(60);

/// A pair whose participants ask for [`LONG_LEASE`].
async fn long_lease_pair(seed: u8) -> Pair {
    let left = Participant::new(ParticipantConfig::from_spdp(
        SpdpConfig::new(0, 0, prefix(seed))
            .expect("domain 0")
            .with_multicast(false)
            .with_announce_period(TICK)
            .with_lease(LONG_LEASE),
    ))
    .await
    .expect("bind");
    let right = Participant::new(ParticipantConfig::from_spdp(
        SpdpConfig::new(0, 0, prefix(seed.wrapping_add(1)))
            .expect("domain 0")
            .with_multicast(false)
            .with_initial_peer(left.metatraffic_locator())
            .with_announce_period(TICK)
            .with_lease(LONG_LEASE),
    ))
    .await
    .expect("bind");
    Pair::start(left, right)
}

#[tokio::test]
async fn a_departing_participant_is_forgotten_without_waiting_for_its_lease() {
    let pair = long_lease_pair(101).await;
    pair.await_discovery().await;

    let left = pair.left.clone();
    let right_guid = pair.right.guid();
    assert!(left.knows(right_guid).await);

    // A graceful shutdown announces the departure; the lease is sixty
    // seconds, so nothing but that announcement can make this pass.
    pair.right.shutdown().await;

    tokio::time::timeout(
        Duration::from_secs(5),
        await_condition("the peer to be forgotten", || {
            let left = left.clone();
            async move { !left.knows(right_guid).await }
        }),
    )
    .await
    .expect("the departure must be announced, not waited out");

    pair.left.shutdown().await;
}

#[tokio::test]
async fn a_departing_participant_unmatches_its_endpoints() {
    let pair = long_lease_pair(111).await;
    pair.await_discovery().await;

    let reader = pair
        .right
        .create_reader(topic(), ReaderQos::reliable(8))
        .await
        .expect("reader");
    let writer = pair
        .left
        .create_writer(topic(), WriterQos::services_default())
        .await
        .expect("writer");
    await_matched(&writer).await;
    assert_eq!(writer.matched_readers().await, 1);

    pair.right.shutdown().await;

    let writer_handle = writer.clone();
    await_condition("the writer to unmatch the departed reader", || {
        let writer = writer_handle.clone();
        async move { writer.matched_readers().await == 0 }
    })
    .await;

    drop(reader);
    pair.left.shutdown().await;
}

#[tokio::test]
async fn deleting_a_writer_unmatches_it_at_the_peer() {
    let pair = long_lease_pair(121).await;
    pair.await_discovery().await;

    let reader = pair
        .right
        .create_reader(topic(), ReaderQos::reliable(8))
        .await
        .expect("reader");
    let writer = pair
        .left
        .create_writer(topic(), WriterQos::services_default())
        .await
        .expect("writer");
    await_matched(&writer).await;

    let right = pair.right.clone();
    let reader_id = reader.guid().entity_id;
    await_condition("the reader to match the writer", || {
        let right = right.clone();
        async move { right.matched_writers(reader_id).await > 0 }
    })
    .await;

    assert!(
        pair.left
            .delete_writer(writer.guid())
            .await
            .expect("delete"),
        "the writer was there"
    );
    assert!(
        !pair
            .left
            .delete_writer(writer.guid())
            .await
            .expect("delete"),
        "and is not there twice"
    );

    let right = pair.right.clone();
    await_condition("the reader to unmatch the deleted writer", || {
        let right = right.clone();
        async move { right.matched_writers(reader_id).await == 0 }
    })
    .await;

    pair.shutdown().await;
}

#[tokio::test]
async fn deleting_a_reader_unmatches_it_at_the_peer() {
    let pair = long_lease_pair(131).await;
    pair.await_discovery().await;

    let (writer, reader) = wire(&pair, WriterQos::services_default(), ReaderQos::reliable(8)).await;
    assert_eq!(writer.matched_readers().await, 1);

    assert!(
        pair.right
            .delete_reader(reader.guid())
            .await
            .expect("delete")
    );

    let writer_handle = writer.clone();
    await_condition("the writer to unmatch the deleted reader", || {
        let writer = writer_handle.clone();
        async move { writer.matched_readers().await == 0 }
    })
    .await;

    // Writing after the last reader went is not an error; it goes nowhere.
    writer.write(payload(0x21)).await.expect("write");

    pair.shutdown().await;
}

#[tokio::test]
async fn a_deleted_writer_stops_delivering() {
    let pair = long_lease_pair(141).await;
    pair.await_discovery().await;

    let (writer, reader) = wire(&pair, WriterQos::services_default(), ReaderQos::reliable(8)).await;

    writer.write(payload(0x01)).await.expect("write");
    let sample = reader
        .take_within(PATIENCE)
        .await
        .expect("the first sample");
    assert_eq!(sample.as_slice(), payload(0x01).as_slice());

    pair.left
        .delete_writer(writer.guid())
        .await
        .expect("delete");

    // The handle survives the deletion; the write is refused because the
    // writer is gone.
    assert!(writer.write(payload(0x02)).await.is_err());
    assert!(
        reader
            .take_within(Duration::from_millis(100))
            .await
            .is_none(),
        "nothing more can arrive from a writer that no longer exists"
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn a_missed_deadline_is_observable() {
    let pair = long_lease_pair(151).await;
    pair.await_discovery().await;

    // Both sides carry the deadline: a reader asking for 50 ms cannot be
    // matched to a writer that offers nothing, and request-versus-offered
    // refuses that pairing before any of this could be observed.
    let (writer, reader) = wire(
        &pair,
        WriterQos {
            deadline: DeadlineQos::from_millis(50),
            ..WriterQos::services_default()
        },
        ReaderQos {
            deadline: DeadlineQos::from_millis(50),
            ..ReaderQos::reliable(8)
        },
    )
    .await;

    // One sample, then silence. The deadline clock starts at the first
    // sample, so nothing is late until then.
    assert!(
        pair.right.deadline_misses().await.is_empty(),
        "a writer that has never sent is not yet late"
    );
    writer.write(payload(0x31)).await.expect("write");
    reader.take_within(PATIENCE).await.expect("a sample");

    let right = pair.right.clone();
    let writer_guid = writer.guid();
    await_condition("the deadline to be reported as missed", || {
        let right = right.clone();
        async move {
            right
                .deadline_misses()
                .await
                .iter()
                .any(|miss| miss.writer == writer_guid)
        }
    })
    .await;

    let miss = pair
        .right
        .deadline_misses()
        .await
        .into_iter()
        .find(|miss| miss.writer == writer_guid)
        .expect("the miss");
    assert_eq!(miss.period, Duration::from_millis(50));
    assert!(miss.since >= miss.period);

    // …and it stops being true the moment a sample arrives.
    writer.write(payload(0x32)).await.expect("write");
    reader.take_within(PATIENCE).await.expect("a second sample");
    let right = pair.right.clone();
    await_condition("the deadline to stop being missed", || {
        let right = right.clone();
        async move { right.deadline_misses().await.is_empty() }
    })
    .await;

    pair.shutdown().await;
}

#[tokio::test]
async fn an_infinite_deadline_is_never_missed() {
    let pair = long_lease_pair(161).await;
    pair.await_discovery().await;
    let (writer, reader) = wire(&pair, WriterQos::services_default(), ReaderQos::reliable(8)).await;

    writer.write(payload(0x41)).await.expect("write");
    reader.take_within(PATIENCE).await.expect("a sample");
    tokio::time::sleep(TICK * 10).await;
    assert!(
        pair.right.deadline_misses().await.is_empty(),
        "the default deadline is infinite"
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn shutting_down_twice_is_harmless() {
    let participant = Participant::new(config(171, None, RosCompat::Jazzy))
        .await
        .expect("bind");
    participant.shutdown().await;
    participant.shutdown().await;
    assert!(participant.is_shut_down().await);
}
