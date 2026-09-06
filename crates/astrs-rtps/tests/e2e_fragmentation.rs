//! Samples too big for one datagram, over real sockets.
//!
//! Blueprint §10.2 fixes the threshold at 64 KiB. Above it a sample is cut
//! into `DATA_FRAG` submessages, reassembled by the reader, and delivered
//! once — and every octet must survive, which is what these tests check
//! rather than merely counting fragments.
//!
//! The interesting cases are the boundaries: exactly at the threshold (one
//! `DATA`), one octet above it (fragmented), a fragment count that divides
//! evenly, and one that leaves a short last fragment. Each has its own test,
//! because each is a separate off-by-one.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use astrs_rtps::behavior::fragment::FRAGMENTATION_THRESHOLD;
use astrs_rtps::behavior::{Participant, ParticipantConfig};
use astrs_rtps::discovery::{HistoryQos, ReaderQos, RosCompat, SpdpConfig, WriterQos};
use harness::{PATIENCE, Pair, TICK, await_matched, config, large_payload, prefix, topic, wire};

/// A pair whose fragmentation settings are forced, so a small payload can
/// exercise the fragment path without a megabyte of octets.
async fn forced_pair(seed: u8, threshold: usize, fragment_size: u16, budget: usize) -> Pair {
    let left = Participant::new(
        config(seed, None, RosCompat::Jazzy)
            .with_fragmentation(threshold, fragment_size)
            .with_datagram_budget(budget),
    )
    .await
    .expect("bind");
    let right = Participant::new(
        config(
            seed.wrapping_add(1),
            Some(left.metatraffic_locator()),
            RosCompat::Jazzy,
        )
        .with_fragmentation(threshold, fragment_size)
        .with_datagram_budget(budget),
    )
    .await
    .expect("bind");
    Pair::start(left, right)
}

fn reliable_qos() -> (WriterQos, ReaderQos) {
    (
        WriterQos {
            history: HistoryQos::keep_all(),
            ..WriterQos::services_default()
        },
        ReaderQos {
            history: HistoryQos::keep_all(),
            ..ReaderQos::reliable(64)
        },
    )
}

#[tokio::test]
async fn a_one_megabyte_sample_round_trips() {
    let pair = Pair::new().await;
    pair.await_discovery().await;
    let (writer_qos, reader_qos) = reliable_qos();
    let (writer, reader) = wire(&pair, writer_qos, reader_qos).await;

    let sample = large_payload(1024 * 1024);
    assert_eq!(sample.len(), 1024 * 1024);
    writer.write(sample.clone()).await.expect("write");

    let received = reader
        .take_within(PATIENCE)
        .await
        .expect("a megabyte must be reassembled and delivered");
    assert_eq!(received.len(), sample.len(), "the reassembled length");
    assert_eq!(
        received.as_slice(),
        sample.as_slice(),
        "every octet must survive the round trip"
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn a_sample_at_the_threshold_is_not_fragmented_and_one_above_it_is() {
    // The effective threshold is the smaller of the configured policy and
    // what one datagram holds; here the policy binds.
    const THRESHOLD: usize = 1_000;
    let pair = forced_pair(161, THRESHOLD, 500, 4_000).await;
    pair.await_discovery().await;
    let (writer_qos, reader_qos) = reliable_qos();
    let (writer, reader) = wire(&pair, writer_qos, reader_qos).await;

    // Exactly at the threshold: one DATA, no fragmentation.
    let at = large_payload(THRESHOLD);
    writer.write(at.clone()).await.expect("write");
    let received = reader
        .take_within(PATIENCE)
        .await
        .expect("the unfragmented sample");
    assert_eq!(received.as_slice(), at.as_slice());

    // One octet above it: fragmented, and still identical on arrival.
    let above = large_payload(THRESHOLD + 1);
    writer.write(above.clone()).await.expect("write");
    let received = reader
        .take_within(PATIENCE)
        .await
        .expect("the fragmented sample");
    assert_eq!(received.len(), above.len());
    assert_eq!(received.as_slice(), above.as_slice());

    pair.shutdown().await;
}

#[tokio::test]
async fn the_sixty_four_kilobyte_policy_is_what_the_blueprint_fixes() {
    // Blueprint §10.2 fixes the *policy* at 64 KiB. It is the default a
    // participant carries; the datagram budget is what lowers it in practice,
    // and the two are separately visible.
    let writer_config = astrs_rtps::behavior::writer::WriterConfig::new(
        astrs_rtps::structure::Guid::new(
            prefix(1),
            astrs_rtps::structure::EntityId::user_defined(
                1,
                astrs_rtps::structure::EntityKind::USER_WRITER_NO_KEY,
            ),
        ),
        topic(),
    );
    assert_eq!(
        writer_config.fragmentation_threshold,
        FRAGMENTATION_THRESHOLD
    );
    assert_eq!(FRAGMENTATION_THRESHOLD, 64 * 1024);
    assert!(
        writer_config.effective_threshold() <= FRAGMENTATION_THRESHOLD,
        "the physical limit can only lower it, never raise it"
    );
}

#[tokio::test]
async fn an_exact_multiple_of_the_fragment_size_round_trips() {
    // 4000 octets at 500 per fragment: eight fragments, none short.
    let pair = forced_pair(101, 1_000, 500, 1_400).await;
    pair.await_discovery().await;
    let (writer_qos, reader_qos) = reliable_qos();
    let (writer, reader) = wire(&pair, writer_qos, reader_qos).await;

    let sample = large_payload(4_000);
    writer.write(sample.clone()).await.expect("write");
    let received = reader.take_within(PATIENCE).await.expect("a sample");
    assert_eq!(received.as_slice(), sample.as_slice());

    pair.shutdown().await;
}

#[tokio::test]
async fn a_short_last_fragment_round_trips() {
    // 4001 octets at 500 per fragment: nine fragments, the last one octet.
    let pair = forced_pair(111, 1_000, 500, 1_400).await;
    pair.await_discovery().await;
    let (writer_qos, reader_qos) = reliable_qos();
    let (writer, reader) = wire(&pair, writer_qos, reader_qos).await;

    let sample = large_payload(4_001);
    writer.write(sample.clone()).await.expect("write");
    let received = reader.take_within(PATIENCE).await.expect("a sample");
    assert_eq!(received.len(), 4_001);
    assert_eq!(received.as_slice(), sample.as_slice());

    pair.shutdown().await;
}

#[tokio::test]
async fn several_fragmented_samples_arrive_in_order_and_intact() {
    let pair = forced_pair(121, 1_000, 400, 1_400).await;
    pair.await_discovery().await;
    let (writer_qos, reader_qos) = reliable_qos();
    let (writer, reader) = wire(&pair, writer_qos, reader_qos).await;

    let samples: Vec<Vec<u8>> = (0..5)
        .map(|index| large_payload(2_000 + index * 137))
        .collect();
    for sample in &samples {
        writer.write(sample.clone()).await.expect("write");
    }

    let received = tokio::time::timeout(PATIENCE, reader.take_at_least(samples.len()))
        .await
        .expect("every fragmented sample must arrive");
    assert_eq!(received.len(), samples.len());
    for (index, sample) in samples.iter().enumerate() {
        assert_eq!(
            received[index].as_slice(),
            sample.as_slice(),
            "sample {index} did not survive"
        );
    }

    pair.shutdown().await;
}

#[tokio::test]
async fn a_fragmented_sample_survives_a_small_datagram_budget() {
    // A budget that fits exactly one fragment per datagram, so every
    // fragment is its own datagram and the reassembler sees the maximum
    // number of arrivals.
    let pair = forced_pair(131, 512, 256, 400).await;
    pair.await_discovery().await;
    let (writer_qos, reader_qos) = reliable_qos();
    let (writer, reader) = wire(&pair, writer_qos, reader_qos).await;

    let sample = large_payload(8_192);
    writer.write(sample.clone()).await.expect("write");
    let received = reader
        .take_within(PATIENCE)
        .await
        .expect("thirty-two fragments must reassemble");
    assert_eq!(received.as_slice(), sample.as_slice());

    pair.shutdown().await;
}

#[tokio::test]
async fn a_best_effort_writer_fragments_too() {
    let pair = forced_pair(141, 1_000, 500, 1_400).await;
    pair.await_discovery().await;
    let (writer, reader) = wire(&pair, WriterQos::sensor_data(), ReaderQos::sensor_data()).await;

    let sample = large_payload(3_000);
    writer.write(sample.clone()).await.expect("write");
    let received = reader
        .take_within(PATIENCE)
        .await
        .expect("best-effort fragments arrive too on a loss-free link");
    assert_eq!(received.as_slice(), sample.as_slice());

    pair.shutdown().await;
}

#[tokio::test]
async fn a_late_reader_gets_a_replayed_fragmented_sample() {
    let left = Participant::new(
        ParticipantConfig::from_spdp(
            SpdpConfig::new(0, 0, prefix(151))
                .expect("domain 0")
                .with_multicast(false)
                .with_announce_period(TICK),
        )
        .with_fragmentation(1_000, 500)
        .with_heartbeat_period(TICK)
        .with_tick_period(TICK),
    )
    .await
    .expect("bind");
    let right = Participant::new(
        config(152, Some(left.metatraffic_locator()), RosCompat::Jazzy)
            .with_fragmentation(1_000, 500),
    )
    .await
    .expect("bind");
    let pair = Pair::start(left, right);
    pair.await_discovery().await;

    let writer = pair
        .left
        .create_writer(topic(), WriterQos::latched(4))
        .await
        .expect("writer");
    let sample = large_payload(5_000);
    writer.write(sample.clone()).await.expect("write");

    // The reader arrives afterwards; TRANSIENT_LOCAL replays the whole
    // fragmented sample to it.
    let reader = pair
        .right
        .create_reader(topic(), ReaderQos::latched(4))
        .await
        .expect("reader");
    await_matched(&writer).await;

    let received = reader
        .take_within(PATIENCE)
        .await
        .expect("the replayed sample must reassemble");
    assert_eq!(received.as_slice(), sample.as_slice());

    pair.shutdown().await;
}
