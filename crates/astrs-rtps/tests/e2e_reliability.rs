//! Reliable and best-effort delivery over real sockets, with real loss.
//!
//! The reliability protocol is only interesting when datagrams go missing,
//! and the honest way to lose one is to drop it at the socket — which is what
//! [`LossySocket`](harness::LossySocket) does, through the public
//! [`DatagramSocket`](astrs_rtps::behavior::DatagramSocket) seam. The
//! participant above cannot tell a dropped datagram from a lost one, so what
//! these tests exercise is the actual HEARTBEAT/ACKNACK/retransmit cycle
//! rather than a simulation of it.
//!
//! Repair latency is one heartbeat period, because a reliable reader nacks
//! when a HEARTBEAT prompts it (§8.4.2.3.4) and not before. The fixtures run
//! at a twenty-millisecond cadence so a repair completes in tens of
//! milliseconds; every wait is bounded by
//! [`PATIENCE`](harness::PATIENCE) and none is a sleep.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use std::sync::Arc;
use std::time::Duration;

use astrs_rtps::behavior::transport::UdpTransport;
use astrs_rtps::behavior::{Participant, ParticipantConfig};
use astrs_rtps::discovery::{ReaderQos, RosCompat, SpdpConfig, WriterQos};
use astrs_rtps::structure::SequenceNumber;
use harness::{
    LossySocket, PATIENCE, Pair, TICK, await_condition, await_matched, config, payload, topic, wire,
};

/// Build a participant whose *user-traffic* socket drops datagrams.
///
/// Discovery runs on a clean metatraffic socket, so the loss is confined to
/// the samples themselves — which is what the reliability protocol is for.
async fn lossy_participant(
    seed: u8,
    peer: Option<astrs_rtps::structure::Locator>,
    drop_every: u64,
) -> (Participant, Arc<LossySocket>) {
    let metatraffic = UdpTransport::bind_loopback()
        .await
        .expect("the metatraffic socket must bind");
    let user_data = LossySocket::bind_loopback(drop_every).await;
    let participant = Participant::with_transport(
        config(seed, peer, RosCompat::Jazzy),
        Arc::new(metatraffic),
        user_data.clone(),
        None,
    )
    .await
    .expect("the participant must build on the supplied sockets");
    (participant, user_data)
}

#[tokio::test]
async fn best_effort_delivers_every_sample_when_nothing_is_lost() {
    let pair = Pair::new().await;
    pair.await_discovery().await;
    let (writer, reader) = wire(&pair, WriterQos::sensor_data(), ReaderQos::sensor_data()).await;

    for tag in 1..=10_u8 {
        writer.write(payload(tag)).await.expect("write");
    }

    let samples = tokio::time::timeout(PATIENCE, reader.take_at_least(10))
        .await
        .expect("ten samples must arrive on a loss-free loopback");
    assert_eq!(samples.len(), 10);
    for (index, sample) in samples.iter().enumerate() {
        assert_eq!(
            sample.sequence_number,
            SequenceNumber::new(index as i64 + 1),
            "best-effort delivers in order when nothing is lost"
        );
    }

    pair.shutdown().await;
}

#[tokio::test]
async fn a_reliable_exchange_survives_induced_loss() {
    let (left, lossy) = lossy_participant(61, None, 0).await;
    let right = Participant::new(config(
        62,
        Some(left.metatraffic_locator()),
        RosCompat::Jazzy,
    ))
    .await
    .expect("bind");
    let pair = Pair::start(left, right);
    pair.await_discovery().await;

    // KEEP_ALL on the writer, because a repair can only come from history
    // the writer still holds: under KEEP_LAST n, a sample evicted before its
    // reader nacked it is answered with a GAP, not a retransmission. That is
    // correct DDS behaviour and it is what
    // `a_keep_last_writer_gaps_what_it_has_already_evicted` asserts; a test
    // of *retransmission* has to keep the history that makes it possible.
    let (writer, reader) = wire(
        &pair,
        WriterQos {
            history: astrs_rtps::discovery::HistoryQos::keep_all(),
            ..WriterQos::services_default()
        },
        ReaderQos {
            history: astrs_rtps::discovery::HistoryQos::keep_all(),
            ..ReaderQos::reliable(64)
        },
    )
    .await;

    // Now start losing every third user datagram. Discovery is already done
    // and runs on a different socket, so only the samples are affected.
    lossy.set_drop_every(3);

    for tag in 1..=20_u8 {
        writer.write(payload(tag)).await.expect("write");
    }

    let samples = tokio::time::timeout(PATIENCE, reader.take_at_least(20))
        .await
        .expect("every sample must arrive, however many datagrams were lost");
    assert!(
        lossy.dropped() > 0,
        "the test proves nothing if nothing was dropped"
    );

    let mut numbers: Vec<i64> = samples
        .iter()
        .map(|sample| sample.sequence_number.value())
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    assert_eq!(
        numbers,
        (1..=20).collect::<Vec<i64>>(),
        "reliability means every sequence number arrives exactly once, dropped {} datagram(s)",
        lossy.dropped()
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn a_reliable_writer_knows_when_everything_is_acknowledged() {
    let pair = Pair::new().await;
    pair.await_discovery().await;
    let (writer, reader) = wire(
        &pair,
        WriterQos::services_default(),
        ReaderQos::reliable(16),
    )
    .await;

    for tag in 1..=5_u8 {
        writer.write(payload(tag)).await.expect("write");
    }
    let _ = tokio::time::timeout(PATIENCE, reader.take_at_least(5))
        .await
        .expect("five samples");

    let writer_handle = writer.clone();
    await_condition("every sample to be acknowledged", || {
        let writer = writer_handle.clone();
        async move { writer.is_acknowledged().await }
    })
    .await;

    pair.shutdown().await;
}

#[tokio::test]
async fn a_totally_black_hole_delivers_nothing_and_recovers_when_it_lifts() {
    let (left, lossy) = lossy_participant(71, None, 0).await;
    let right = Participant::new(config(
        72,
        Some(left.metatraffic_locator()),
        RosCompat::Jazzy,
    ))
    .await
    .expect("bind");
    let pair = Pair::start(left, right);
    pair.await_discovery().await;

    let (writer, reader) = wire(
        &pair,
        WriterQos {
            history: astrs_rtps::discovery::HistoryQos::keep_all(),
            ..WriterQos::services_default()
        },
        ReaderQos::reliable(64),
    )
    .await;

    lossy.set_blackhole(true);
    for tag in 1..=5_u8 {
        writer.write(payload(tag)).await.expect("write");
    }
    // Several heartbeat periods with the link down: nothing can arrive.
    tokio::time::sleep(TICK * 5).await;
    assert_eq!(
        reader.len().await,
        0,
        "the user-traffic link is a black hole"
    );

    // Lift it; the reliability protocol repairs everything.
    lossy.set_blackhole(false);
    let samples = tokio::time::timeout(PATIENCE, reader.take_at_least(5))
        .await
        .expect("the backlog must be repaired once the link comes back");
    let mut numbers: Vec<i64> = samples
        .iter()
        .map(|sample| sample.sequence_number.value())
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    assert_eq!(numbers, vec![1, 2, 3, 4, 5]);

    pair.shutdown().await;
}

#[tokio::test]
async fn a_transient_local_writer_replays_history_to_a_late_reader() {
    let pair = Pair::new().await;
    pair.await_discovery().await;

    let writer = pair
        .left
        .create_writer(topic(), WriterQos::latched(8))
        .await
        .expect("writer");
    // Nothing is subscribed yet.
    for tag in 1..=3_u8 {
        writer.write(payload(tag)).await.expect("write");
    }

    let reader = pair
        .right
        .create_reader(topic(), ReaderQos::latched(8))
        .await
        .expect("reader");
    await_matched(&writer).await;

    let samples = tokio::time::timeout(PATIENCE, reader.take_at_least(3))
        .await
        .expect("TRANSIENT_LOCAL must replay to a late joiner");
    assert_eq!(samples.len(), 3);
    assert_eq!(samples[0].sequence_number, SequenceNumber::FIRST);

    pair.shutdown().await;
}

#[tokio::test]
async fn a_volatile_writer_gives_a_late_reader_nothing_that_came_before_it() {
    let pair = Pair::new().await;
    pair.await_discovery().await;

    let writer = pair
        .left
        .create_writer(topic(), WriterQos::services_default())
        .await
        .expect("writer");
    for tag in 1..=3_u8 {
        writer.write(payload(tag)).await.expect("write");
    }

    let reader = pair
        .right
        .create_reader(topic(), ReaderQos::reliable(8))
        .await
        .expect("reader");
    await_matched(&writer).await;

    // Whatever the writer already had is gone; what it writes now arrives.
    writer.write(payload(0x99)).await.expect("write");
    let sample = reader
        .take_within(PATIENCE)
        .await
        .expect("the sample written after matching must arrive");
    assert_eq!(
        sample.sequence_number,
        SequenceNumber::new(4),
        "a VOLATILE writer starts a late joiner at the present, not at sample one"
    );
    assert_eq!(sample.as_slice(), payload(0x99).as_slice());

    // …and nothing older ever shows up.
    assert!(
        reader
            .take_within(Duration::from_millis(100))
            .await
            .is_none(),
        "samples 1..=3 must never be replayed"
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn many_samples_arrive_in_order_on_a_reliable_link() {
    let pair = Pair::new().await;
    pair.await_discovery().await;
    let (writer, reader) = wire(
        &pair,
        WriterQos {
            history: astrs_rtps::discovery::HistoryQos::keep_all(),
            ..WriterQos::services_default()
        },
        ReaderQos {
            history: astrs_rtps::discovery::HistoryQos::keep_all(),
            ..ReaderQos::reliable(256)
        },
    )
    .await;

    const COUNT: usize = 100;
    for tag in 0..COUNT {
        writer.write(payload(tag as u8)).await.expect("write");
    }

    let samples = tokio::time::timeout(PATIENCE, reader.take_at_least(COUNT))
        .await
        .expect("every sample must arrive");
    let numbers: Vec<i64> = samples
        .iter()
        .take(COUNT)
        .map(|sample| sample.sequence_number.value())
        .collect();
    assert_eq!(
        numbers,
        (1..=COUNT as i64).collect::<Vec<i64>>(),
        "a reliable reader delivers in sequence-number order"
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn a_reader_created_on_a_passive_participant_still_receives() {
    // The writer's participant is the one with no initial peers: it is found
    // rather than finding, which is the configuration a fixed-port node has.
    let listener = Participant::new(ParticipantConfig::from_spdp(
        SpdpConfig::new(0, 0, harness::prefix(81))
            .expect("domain 0")
            .with_multicast(false)
            .with_announce_period(TICK),
    ))
    .await
    .expect("bind");
    let caller = Participant::new(config(
        82,
        Some(listener.metatraffic_locator()),
        RosCompat::Jazzy,
    ))
    .await
    .expect("bind");

    let pair = Pair::start(listener, caller);
    pair.await_discovery().await;

    let writer = pair
        .left
        .create_writer(topic(), WriterQos::services_default())
        .await
        .expect("writer");
    let reader = pair
        .right
        .create_reader(topic(), ReaderQos::reliable(8))
        .await
        .expect("reader");
    await_matched(&writer).await;

    writer.write(payload(0x42)).await.expect("write");
    let sample = reader.take_within(PATIENCE).await.expect("a sample");
    assert_eq!(sample.as_slice(), payload(0x42).as_slice());

    pair.shutdown().await;
}

#[tokio::test]
async fn a_keep_last_writer_gaps_what_it_has_already_evicted() {
    // The complement of `a_reliable_exchange_survives_induced_loss`: a
    // reliable writer repairs only what its history still holds. A sample
    // pushed out by KEEP_LAST is answered with a GAP, the reader stops asking
    // for it, and delivery moves on rather than wedging.
    let (left, lossy) = lossy_participant(91, None, 0).await;
    let right = Participant::new(config(
        92,
        Some(left.metatraffic_locator()),
        RosCompat::Jazzy,
    ))
    .await
    .expect("bind");
    let pair = Pair::start(left, right);
    pair.await_discovery().await;

    let (writer, reader) = wire(
        &pair,
        WriterQos::latched(4),
        ReaderQos {
            durability: astrs_rtps::discovery::DurabilityQos::transient_local(),
            ..ReaderQos::reliable(64)
        },
    )
    .await;

    lossy.set_blackhole(true);
    for tag in 1..=12_u8 {
        writer.write(payload(tag)).await.expect("write");
    }
    lossy.set_blackhole(false);

    // Only the four the KEEP_LAST 4 history still holds can arrive; the
    // reader must not stall waiting for the eight that are gone.
    let samples = tokio::time::timeout(PATIENCE, reader.take_at_least(4))
        .await
        .expect("the surviving history must be delivered");
    let numbers: Vec<i64> = samples
        .iter()
        .map(|sample| sample.sequence_number.value())
        .collect();
    assert_eq!(
        numbers,
        vec![9, 10, 11, 12],
        "a KEEP_LAST 4 writer holds the last four and GAPs the rest"
    );

    // And the link keeps working afterwards.
    writer.write(payload(0xee)).await.expect("write");
    let sample = reader
        .take_within(PATIENCE)
        .await
        .expect("delivery continues past the gap");
    assert_eq!(sample.sequence_number, SequenceNumber::new(13));

    pair.shutdown().await;
}
