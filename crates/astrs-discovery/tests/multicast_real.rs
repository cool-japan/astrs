//! Real-multicast integration test, gated behind
//! `ASTRS_TEST_MULTICAST=1` (blueprint task requirement: "guard
//! actual-multicast tests with an env var ... and skip otherwise").
//!
//! Multicast is routinely unavailable in sandboxes, containers and CI
//! runners (no IGMP support, network namespaces without a multicast route,
//! virtualization layers that drop it outright) — exactly the situation
//! `astrs-discovery` is designed to degrade gracefully around (see
//! [`astrs_discovery::socket::MulticastStatus`]). This test is therefore
//! opt-in, not part of the default suite, and reports why it did nothing
//! rather than silently vanishing when skipped.
//!
//! A single test process binds one socket, joins the real multicast group,
//! and sends its own beacon to that group — verifying real
//! `IP_ADD_MEMBERSHIP` + `IP_MULTICAST_LOOP` + kernel multicast delivery
//! end to end via self-addressed loopback. (Plain, multicast-independent
//! sender/watcher wiring is already covered unconditionally by
//! `tests/loopback.rs`.)

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use astrs_discovery::defaults::{
    DEFAULT_MULTICAST_GROUP, DEFAULT_MULTICAST_PORT, ENV_TEST_MULTICAST,
};
use astrs_discovery::sender::{BeaconSender, SenderConfig};
use astrs_discovery::socket::MulticastStatus;
use astrs_discovery::watcher::{BeaconWatcher, WatcherConfig};
use astrs_discovery::{BeaconEvent, BeaconRole, socket};
use astrs_time::HlcClock;
use astrs_wire::{AuthToken, DaemonId, MachineName};

#[tokio::test]
async fn a_beacon_sent_to_the_real_multicast_group_loops_back_on_this_host() {
    if std::env::var(ENV_TEST_MULTICAST).as_deref() != Ok("1") {
        eprintln!(
            "SKIPPED a_beacon_sent_to_the_real_multicast_group_loops_back_on_this_host: \
             set {ENV_TEST_MULTICAST}=1 to run it (requires real, working IPv4 multicast \
             on this host)."
        );
        return;
    }

    let bind_addr = format!("0.0.0.0:{DEFAULT_MULTICAST_PORT}").parse().unwrap();
    let (socket, status) = match socket::bind(bind_addr, DEFAULT_MULTICAST_GROUP).await {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!(
                "SKIPPED: could not even bind the discovery port ({err}); \
                 {ENV_TEST_MULTICAST}=1 was set but this host cannot run the test."
            );
            return;
        }
    };
    if !matches!(status, MulticastStatus::Joined) {
        eprintln!(
            "SKIPPED: multicast join reported {status:?} on this host/network, even though \
             {ENV_TEST_MULTICAST}=1 was set — this is exactly the degraded mode this crate is \
             designed to tolerate, so it is not a test failure, just an unmet precondition."
        );
        return;
    }

    // One socket, shared by both roles — the realistic topology (blueprint
    // §6.4): a single daemon-owned socket both announces itself and
    // listens for everyone else.
    let socket = Arc::new(socket);
    let token = AuthToken::from_bytes([0x77; 32]);
    let id = DaemonId::generate(Some(MachineName::new("multicast-self-test").unwrap()));
    let listen_addrs = vec!["10.0.0.5:7408".parse().unwrap()];

    let sender_config = SenderConfig::new(
        BeaconRole::Daemon,
        id.clone(),
        listen_addrs.clone(),
        token.clone(),
    )
    .with_interval(Duration::from_millis(50));
    let watcher_config =
        WatcherConfig::new(token).with_expected_interval(Duration::from_millis(50));

    let (watcher, mut events) = BeaconWatcher::new(Arc::clone(&socket), watcher_config);
    let sender = BeaconSender::new(
        Arc::clone(&socket),
        sender_config,
        Arc::new(HlcClock::system()),
    );

    let watcher_handle = watcher.spawn();
    let sender_handle = sender.spawn();

    let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("a self-addressed multicast beacon should loop back within 10s on a host with working multicast")
        .expect("event channel should stay open while the watcher task is alive");

    match event {
        BeaconEvent::Discovered(info) => {
            assert_eq!(info.id, id);
            assert_eq!(info.role, BeaconRole::Daemon);
            assert_eq!(info.listen_addrs, listen_addrs);
        }
        other => panic!("expected Discovered, got {other:?}"),
    }

    sender_handle.abort();
    watcher_handle.abort();
}
