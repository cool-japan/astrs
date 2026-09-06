//! WLP over real sockets: a participant proving it is still there.
//!
//! Two mechanisms, and the tests keep them apart because they are answered by
//! different traffic:
//!
//! - **`AUTOMATIC`** — the participant's SPDP announcement is the assertion
//!   (§8.4.13.1). Nothing extra goes on the wire; the lease is renewed by the
//!   same datagram that keeps the participant in the discovery database.
//! - **`MANUAL_BY_PARTICIPANT`** — an explicit sample on the builtin topic
//!   `DCPSParticipantMessage`, which is the one builtin topic whose payload is
//!   plain `CDR_LE` rather than a `PL_CDR` parameter list.
//!
//! The second is what [`Participant::assert_liveliness`] sends, and what these
//! tests check arrives, decodes, and renews the right lease.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod harness;

use astrs_rtps::behavior::liveliness::{
    ParticipantMessageData, ParticipantMessageKind, WLP_TOPIC_NAME, WLP_TYPE_NAME,
};
use astrs_rtps::behavior::{LivelinessTracker, Participant};
use astrs_rtps::discovery::RosCompat;
use astrs_rtps::structure::{Duration, GuidPrefix, VendorId};
use harness::{PATIENCE, Pair, TICK, await_condition, config, prefix};
use std::time::Duration as StdDuration;
use std::time::Instant;

#[tokio::test]
async fn a_manual_assertion_reaches_the_peer_and_renews_its_lease() {
    let pair = Pair::new().await;
    pair.await_discovery().await;

    // The right participant is the one asserting; the left is the one whose
    // liveliness tracker should learn about it.
    pair.right
        .assert_liveliness(ParticipantMessageKind::Manual)
        .await
        .expect("the assertion must go out");

    let left = pair.left.clone();
    let right_guid = pair.right.guid();
    await_condition("the manual liveliness assertion to land", || {
        let left = left.clone();
        async move {
            left.liveliness_state(right_guid, ParticipantMessageKind::Manual)
                .await
                .is_some()
        }
    })
    .await;

    let state = pair
        .left
        .liveliness_state(pair.right.guid(), ParticipantMessageKind::Manual)
        .await
        .expect("the lease must exist");
    assert!(state.count >= 1);

    pair.shutdown().await;
}

#[tokio::test]
async fn an_spdp_announcement_renews_the_automatic_lease() {
    let pair = Pair::new().await;
    pair.await_discovery().await;

    // No explicit assertion: SPDP alone is the AUTOMATIC assertion.
    let left = pair.left.clone();
    let right_guid = pair.right.guid();
    await_condition("the automatic lease to appear", || {
        let left = left.clone();
        async move {
            left.liveliness_state(right_guid, ParticipantMessageKind::Automatic)
                .await
                .is_some()
        }
    })
    .await;

    let first = pair
        .left
        .liveliness_state(pair.right.guid(), ParticipantMessageKind::Automatic)
        .await
        .expect("a lease");

    // …and it keeps being renewed while the peer announces.
    let left = pair.left.clone();
    let baseline = first.count;
    await_condition("the automatic lease to be renewed", || {
        let left = left.clone();
        async move {
            left.liveliness_state(right_guid, ParticipantMessageKind::Automatic)
                .await
                .is_some_and(|state| state.count > baseline)
        }
    })
    .await;

    pair.shutdown().await;
}

#[tokio::test]
async fn a_manual_assertion_bumps_the_announced_count() {
    let participant = Participant::new(config(91, None, RosCompat::Jazzy))
        .await
        .expect("bind");
    assert_eq!(participant.manual_liveliness_count().await, 0);
    participant
        .assert_liveliness(ParticipantMessageKind::Manual)
        .await
        .expect("assert");
    assert_eq!(participant.manual_liveliness_count().await, 1);
    participant
        .assert_liveliness(ParticipantMessageKind::Automatic)
        .await
        .expect("assert");
    assert_eq!(
        participant.manual_liveliness_count().await,
        1,
        "an AUTOMATIC assertion is not a manual one"
    );
    participant.shutdown().await;
}

#[tokio::test]
async fn the_wlp_sample_is_plain_cdr_on_the_builtin_topic() {
    // The payload format is the trap this topic sets, so it is asserted
    // directly as well as end to end.
    let message =
        ParticipantMessageData::manual(GuidPrefix::vendor_scoped(VendorId::ASTRS, [7; 10]));
    let payload = message.to_payload().expect("encode");
    assert!(
        !payload.is_parameter_list(),
        "DCPSParticipantMessage is plain CDR, unlike every other builtin topic"
    );
    let decoded = ParticipantMessageData::from_payload(&payload).expect("decode");
    assert_eq!(decoded, message);
    assert_eq!(decoded.kind, ParticipantMessageKind::Manual);
    assert_eq!(WLP_TOPIC_NAME, "DCPSParticipantMessage");
    assert_eq!(WLP_TYPE_NAME, "ParticipantMessageData");
}

#[tokio::test]
async fn a_lease_that_is_never_renewed_runs_out() {
    // The timer half, exercised directly: no sockets, no waiting.
    let mut tracker = LivelinessTracker::new();
    let now = Instant::now();
    let participant = prefix(99).with_entity(astrs_rtps::structure::ENTITYID_PARTICIPANT);
    tracker.assert_automatic(participant, StdDuration::from_millis(50), now);

    assert!(tracker.is_alive(participant, now + StdDuration::from_millis(40)));
    let lost = tracker.reap(now + StdDuration::from_millis(60));
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].participant, participant);
    assert_eq!(lost[0].kind, ParticipantMessageKind::Automatic);
    assert!(tracker.is_empty());
}

#[tokio::test]
async fn liveliness_is_forgotten_when_the_participant_is() {
    let left = Participant::new(config(95, None, RosCompat::Jazzy))
        .await
        .expect("bind");
    let mut spdp = astrs_rtps::discovery::SpdpConfig::new(0, 0, prefix(96))
        .expect("domain 0")
        .with_multicast(false)
        .with_initial_peer(left.metatraffic_locator())
        .with_announce_period(TICK)
        .with_lease(Duration::from_millis(200));
    spdp.announce_period = TICK;
    let right = Participant::new(
        astrs_rtps::behavior::ParticipantConfig::from_spdp(spdp)
            .with_heartbeat_period(TICK)
            .with_tick_period(TICK),
    )
    .await
    .expect("bind");

    let left_task = left.spawn(TICK);
    let right_task = right.spawn(TICK);
    let right_guid = right.guid();

    let watcher = left.clone();
    await_condition("the peer's liveliness to be tracked", || {
        let watcher = watcher.clone();
        async move {
            watcher
                .liveliness_state(right_guid, ParticipantMessageKind::Automatic)
                .await
                .is_some()
        }
    })
    .await;

    right.shutdown().await;
    right_task.abort();

    let watcher = left.clone();
    await_condition("the peer's liveliness to be forgotten", || {
        let watcher = watcher.clone();
        async move {
            !watcher.knows(right_guid).await
                && watcher
                    .liveliness_state(right_guid, ParticipantMessageKind::Automatic)
                    .await
                    .is_none()
        }
    })
    .await;

    left.shutdown().await;
    let _ = tokio::time::timeout(PATIENCE, left_task).await;
}
