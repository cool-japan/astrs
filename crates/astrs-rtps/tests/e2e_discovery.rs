//! Two AstRS participants, real UDP sockets, mutual discovery.
//!
//! Blueprint §10.2's in-repo interoperability proof: not a mock, not a
//! captured trace, two independent participants in one process exchanging
//! genuine RTPS datagrams over loopback and completing SPDP and SEDP.
//!
//! Everything here runs over **unicast initial peers**. The multicast group
//! is the specification's default rendezvous and it is implemented, but a
//! sandboxed macOS host refuses the join, so the deterministic assertions use
//! the path that needs no kernel permission. The multicast capability is
//! probed and its outcome asserted loudly in
//! [`the_multicast_capability_is_answered_not_assumed`] — there is no silent
//! skip anywhere in this file.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use std::time::Duration;

use astrs_rtps::behavior::MulticastCapability;
use astrs_rtps::behavior::endpoint::TopicKey;
use astrs_rtps::discovery::{DiscoveryEvent, ReaderQos, RosCompat, WriterQos};
use harness::{
    PATIENCE, Pair, TICK, await_condition, await_matched, config, multicast_probe, payload, topic,
    wire,
};

#[tokio::test]
async fn two_participants_discover_each_other_over_unicast() {
    let pair = Pair::new().await;
    pair.await_discovery().await;

    assert_eq!(pair.left.known_participants().await, 1);
    assert_eq!(pair.right.known_participants().await, 1);
    assert!(pair.left.knows(pair.right.guid()).await);
    assert!(pair.right.knows(pair.left.guid()).await);

    // Each learned the other's *bound* metatraffic port, not a computed one.
    let known = pair.left.participants().await;
    assert_eq!(known, vec![pair.right.guid()]);

    pair.shutdown().await;
}

#[tokio::test]
async fn discovery_is_reported_as_an_event() {
    let left = astrs_rtps::behavior::Participant::new(config(11, None, RosCompat::Jazzy))
        .await
        .expect("bind");
    let mut events = left.events();

    let right = astrs_rtps::behavior::Participant::new(config(
        12,
        Some(left.metatraffic_locator()),
        RosCompat::Jazzy,
    ))
    .await
    .expect("bind");
    let pair = Pair::start(left, right);

    let discovered = tokio::time::timeout(PATIENCE, async {
        loop {
            match events.recv().await {
                Ok(DiscoveryEvent::ParticipantDiscovered(guid)) => return guid,
                Ok(_) => {}
                Err(_) => continue,
            }
        }
    })
    .await
    .expect("a discovery event must arrive");

    assert_eq!(discovered, pair.right.guid());
    pair.shutdown().await;
}

#[tokio::test]
async fn a_writer_and_a_reader_on_one_topic_match_through_sedp() {
    let pair = Pair::new().await;
    pair.await_discovery().await;

    let (writer, reader) = wire(
        &pair,
        WriterQos::services_default(),
        ReaderQos::reliable(10),
    )
    .await;

    assert_eq!(writer.matched_readers().await, 1);
    let right = pair.right.clone();
    let reader_id = reader.guid().entity_id;
    await_condition("the reader to match the writer", || {
        let right = right.clone();
        async move { right.matched_writers(reader_id).await > 0 }
    })
    .await;

    pair.shutdown().await;
}

#[tokio::test]
async fn endpoints_on_different_topics_never_match() {
    let pair = Pair::new().await;
    pair.await_discovery().await;

    let _reader = pair
        .right
        .create_reader(
            TopicKey::new("rt/elsewhere", harness::TYPE_NAME).expect("valid"),
            ReaderQos::reliable(10),
        )
        .await
        .expect("reader");
    let writer = pair
        .left
        .create_writer(topic(), WriterQos::services_default())
        .await
        .expect("writer");

    // Give discovery every chance to do the wrong thing.
    tokio::time::sleep(TICK * 5).await;
    assert_eq!(
        writer.matched_readers().await,
        0,
        "a reader on another topic must never be matched"
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn an_incompatible_qos_pair_never_matches() {
    let pair = Pair::new().await;
    pair.await_discovery().await;

    // A RELIABLE reader cannot be served by a BEST_EFFORT writer.
    let _reader = pair
        .right
        .create_reader(topic(), ReaderQos::reliable(10))
        .await
        .expect("reader");
    let writer = pair
        .left
        .create_writer(topic(), WriterQos::sensor_data())
        .await
        .expect("writer");

    tokio::time::sleep(TICK * 5).await;
    assert_eq!(
        writer.matched_readers().await,
        0,
        "request-versus-offered must refuse this pairing"
    );

    pair.shutdown().await;
}

#[tokio::test]
async fn humble_and_jazzy_both_discover() {
    for (left_compat, right_compat) in [
        (RosCompat::Humble, RosCompat::Humble),
        (RosCompat::Jazzy, RosCompat::Jazzy),
        (RosCompat::Humble, RosCompat::Jazzy),
    ] {
        let pair = Pair::with_compat(left_compat, right_compat).await;
        pair.await_discovery().await;

        let (writer, reader) = wire(
            &pair,
            WriterQos::services_default(),
            ReaderQos::reliable(10),
        )
        .await;
        writer.write(payload(0x5a)).await.expect("write");

        let sample = reader
            .take_within(PATIENCE)
            .await
            .unwrap_or_else(|| panic!("{left_compat} to {right_compat} delivered nothing"));
        assert_eq!(sample.as_slice(), payload(0x5a).as_slice());

        pair.shutdown().await;
    }
}

#[tokio::test]
async fn the_multicast_capability_is_answered_not_assumed() {
    let capability = multicast_probe().await;
    match &capability {
        MulticastCapability::Joined { group } => {
            assert_eq!(group.octets(), [239, 255, 0, 1]);
        }
        MulticastCapability::Refused { group, reason } => {
            assert_eq!(group.octets(), [239, 255, 0, 1]);
            assert!(
                !reason.message.is_empty(),
                "a refusal must say what the kernel said"
            );
        }
        MulticastCapability::Disabled => {
            panic!("a probe never asks for a disabled capability, it asks the kernel")
        }
    }
    // Whichever it was, it is an answer — and every protocol assertion in
    // this file ran on the unicast path regardless.
    assert!(capability.is_joined() || capability.is_refused());
}

#[tokio::test]
async fn a_participant_that_enables_multicast_still_records_the_outcome() {
    let mut spdp = astrs_rtps::discovery::SpdpConfig::new(0, 0, harness::prefix(21))
        .expect("domain 0")
        .with_announce_period(TICK);
    spdp = spdp.with_multicast(true);
    let participant = astrs_rtps::behavior::Participant::new(
        astrs_rtps::behavior::ParticipantConfig::from_spdp(spdp),
    )
    .await
    .expect("bind");

    let capability = participant.multicast();
    assert!(
        capability.is_joined() || capability.is_refused(),
        "the participant must record what happened, got {capability}"
    );
    assert_ne!(
        capability,
        &MulticastCapability::Disabled,
        "multicast was requested, so 'disabled' would be a lie"
    );
    participant.shutdown().await;
}

#[tokio::test]
async fn the_announced_locators_are_the_bound_ones() {
    let participant = astrs_rtps::behavior::Participant::new(config(31, None, RosCompat::Jazzy))
        .await
        .expect("bind");

    let metatraffic = participant.metatraffic_locator();
    let user = participant.user_locator();
    assert_ne!(metatraffic.udp_port(), Some(0), "port 0 is never announced");
    assert_ne!(user.udp_port(), Some(0));
    assert_ne!(
        metatraffic.udp_port(),
        Some(7_410),
        "the computed §9.6.1.1 port is not what an ephemeral bind produces"
    );
    assert!(metatraffic.is_loopback());
    participant.shutdown().await;
}

#[tokio::test]
async fn a_participant_that_goes_away_is_forgotten_when_its_lease_runs_out() {
    // A one-second lease and a participant that never re-announces.
    let left = astrs_rtps::behavior::Participant::new(config(41, None, RosCompat::Jazzy))
        .await
        .expect("bind");
    let mut spdp = astrs_rtps::discovery::SpdpConfig::new(0, 0, harness::prefix(42))
        .expect("domain 0")
        .with_multicast(false)
        .with_initial_peer(left.metatraffic_locator())
        .with_announce_period(TICK)
        .with_lease(astrs_rtps::structure::Duration::from_millis(200));
    spdp.announce_period = Duration::from_millis(10);
    let right = astrs_rtps::behavior::Participant::new(
        astrs_rtps::behavior::ParticipantConfig::from_spdp(spdp),
    )
    .await
    .expect("bind");

    let left_task = left.spawn(TICK);
    let right_task = right.spawn(TICK);

    let watcher = left.clone();
    let right_guid = right.guid();
    await_condition("the peer to be discovered", || {
        let watcher = watcher.clone();
        async move { watcher.knows(right_guid).await }
    })
    .await;

    // Stop the peer announcing; its lease runs out and it is forgotten.
    right.shutdown().await;
    right_task.abort();

    let watcher = left.clone();
    await_condition("the peer's lease to run out", || {
        let watcher = watcher.clone();
        async move { !watcher.knows(right_guid).await }
    })
    .await;

    left.shutdown().await;
    let _ = tokio::time::timeout(PATIENCE, left_task).await;
}

#[tokio::test]
async fn discovery_survives_endpoints_created_before_the_peer_is_known() {
    // The endpoints exist before either participant has heard of the other,
    // so SEDP has to catch up after SPDP does.
    let left = astrs_rtps::behavior::Participant::new(config(51, None, RosCompat::Jazzy))
        .await
        .expect("bind");
    let writer = left
        .create_writer(topic(), WriterQos::services_default())
        .await
        .expect("writer");

    let right = astrs_rtps::behavior::Participant::new(config(
        52,
        Some(left.metatraffic_locator()),
        RosCompat::Jazzy,
    ))
    .await
    .expect("bind");
    let reader = right
        .create_reader(topic(), ReaderQos::reliable(10))
        .await
        .expect("reader");

    let pair = Pair::start(left, right);
    await_matched(&writer).await;
    writer.write(payload(0x11)).await.expect("write");
    let sample = reader
        .take_within(PATIENCE)
        .await
        .expect("the sample must arrive once SEDP catches up");
    assert_eq!(sample.as_slice(), payload(0x11).as_slice());

    pair.shutdown().await;
}
