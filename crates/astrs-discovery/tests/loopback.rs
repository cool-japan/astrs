//! Real-socket integration test: a [`BeaconSender`] and a [`BeaconWatcher`]
//! talking over actual loopback UDP sockets, with **no** dependency on
//! multicast actually working — the sender's unicast fallback list points
//! straight at the watcher's bound address. This is the test the task
//! calls out explicitly as distinct from (and unconditional on) real
//! multicast support: it must pass in every sandboxed/CI environment this
//! crate targets, per its own graceful-degradation design (see
//! `astrs_discovery`'s crate docs).
//!
//! Real multicast group behavior is covered separately by
//! `tests/multicast_real.rs`, gated behind `ASTRS_TEST_MULTICAST=1`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use astrs_discovery::sender::{BeaconSender, SenderConfig};
use astrs_discovery::watcher::{BeaconWatcher, WatcherConfig};
use astrs_discovery::{BeaconEvent, BeaconRole, DiscoverySocket, socket};
use astrs_time::HlcClock;
use astrs_wire::{AuthToken, DaemonId, MachineName};

const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 74, 7);

#[tokio::test]
async fn a_sender_and_watcher_discover_each_other_over_loopback_unicast() {
    let token = AuthToken::from_bytes([0x5A; 32]);

    let (watcher_socket, _watcher_multicast_status) =
        socket::bind("127.0.0.1:0".parse().unwrap(), GROUP)
            .await
            .expect("binding an ephemeral loopback port must succeed everywhere");
    let watcher_addr = watcher_socket.local_addr().unwrap();

    let (sender_socket, _sender_multicast_status) =
        socket::bind("127.0.0.1:0".parse().unwrap(), GROUP)
            .await
            .expect("binding an ephemeral loopback port must succeed everywhere");
    let sender_addr = sender_socket.local_addr().unwrap();

    let sender_id = DaemonId::generate(Some(MachineName::new("loopback-daemon").unwrap()));
    let sender_listen_addrs = vec!["10.0.0.5:7408".parse().unwrap()];
    let sender_config = SenderConfig::new(
        BeaconRole::Daemon,
        sender_id.clone(),
        sender_listen_addrs.clone(),
        token.clone(),
    )
    // Point the "unicast fallback list" straight at the watcher: no real
    // multicast group membership or delivery is required for this test.
    .with_unicast_fallback(vec![watcher_addr])
    .with_interval(Duration::from_millis(20));

    let watcher_config =
        WatcherConfig::new(token).with_expected_interval(Duration::from_millis(20));
    let (watcher, mut events) = BeaconWatcher::new(watcher_socket, watcher_config);
    let sender = BeaconSender::new(sender_socket, sender_config, Arc::new(HlcClock::system()));

    let watcher_handle = watcher.spawn();
    let sender_handle = sender.spawn();

    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("a Discovered event should arrive well within 5s of real loopback traffic")
        .expect("the event channel should not close while the watcher task is alive");

    match event {
        BeaconEvent::Discovered(info) => {
            assert_eq!(info.id, sender_id);
            assert_eq!(info.role, BeaconRole::Daemon);
            assert_eq!(info.listen_addrs, sender_listen_addrs);
            // The address the datagram physically arrived from: the
            // sender's own bound loopback socket, not one of its claimed
            // `listen_addrs` (which are a deliberately unrelated fixture
            // address above, exactly to keep this distinction visible).
            assert_eq!(info.source, sender_addr);
        }
        other => panic!("expected Discovered, got {other:?}"),
    }

    sender_handle.abort();
    watcher_handle.abort();
}
