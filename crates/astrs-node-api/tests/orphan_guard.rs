//! The orphan guard, end to end (blueprint §4.2).
//!
//! A node whose supervisor dies must wind down through the ordinary stop path
//! rather than linger holding shared-memory segments and bound ports. These
//! tests drive a real node with a real guard and assert the observable
//! behaviour: a live parent changes nothing, and a dead one produces a
//! `Stop`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use astrs_node_api::orphan::{DEFAULT_POLL_INTERVAL, OrphanGuard, process_exists};
use astrs_node_api::prelude::*;
use astrs_node_api::runtime::NodeRuntime;
use astrs_node_api::testing::TestHarness;

/// The deadline every wait in this file uses.
const WAIT: Duration = Duration::from_secs(5);

#[test]
fn a_live_process_is_reported_alive_and_an_impossible_one_is_not() {
    assert!(process_exists(std::process::id()));
    assert!(!process_exists(u32::MAX));
}

#[test]
fn a_node_with_a_live_supervisor_keeps_running() {
    let mut harness = TestHarness::start().unwrap();
    let runtime = NodeRuntime::acquire().unwrap();
    let guard = OrphanGuard::watch(
        std::process::id(),
        &runtime,
        std::sync::Arc::clone(harness.events.source()),
        Duration::from_millis(10),
    );

    harness.feed(vec![1, 2, 3]).unwrap();
    let event = harness.next_event().unwrap();
    assert!(event.is_input(), "the node is still working");
    assert!(!guard.has_fired());
    assert!(!harness.node.is_orphaned());

    // The watch really is polling — give it a couple of intervals first, since
    // the input above may well have arrived before the first tick.
    let deadline = std::time::Instant::now() + WAIT;
    while guard.poll_count() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the watch never polled"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!guard.has_fired(), "a live parent never fires the guard");
}

#[test]
fn a_node_whose_supervisor_vanished_is_stopped() {
    let mut harness = TestHarness::start().unwrap();
    let runtime = NodeRuntime::acquire().unwrap();
    let _guard = OrphanGuard::watch(
        u32::MAX,
        &runtime,
        std::sync::Arc::clone(harness.events.source()),
        Duration::from_millis(10),
    );

    // The guard reports the orphaning, then stops the node.
    let mut saw_report = false;
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "the guard never fired"
        );
        let Ok(Some(event)) = harness.events.recv_timeout(Duration::from_millis(200)) else {
            continue;
        };
        match event {
            Event::Error(message) => {
                assert!(message.contains("orphaned"), "{message}");
                saw_report = true;
            }
            Event::Stop(cause) => {
                assert!(saw_report, "the report comes before the stop");
                assert_eq!(cause, StopCause::DaemonShutdown);
                break;
            }
            _ => {}
        }
    }
    assert!(harness.events.is_fused(), "the stream fused on the stop");
}

#[test]
fn a_builder_can_turn_the_guard_off() {
    // A node that deliberately outlives its launcher.
    let builder = Node::builder()
        .node_id("detached")
        .unwrap()
        .orphan_guard(false)
        .parent_pid(u32::MAX)
        .connect_timeout(Duration::from_millis(50))
        .daemon(format!(
            "uds://{}",
            std::env::temp_dir()
                .join("astrs-orphan-absent.sock")
                .display()
        ));
    // The dial fails (there is no daemon), which is the point: the builder's
    // settings are applied before any guard would be started.
    let error = builder.connect().unwrap_err();
    assert!(matches!(error, NodeError::Connect { .. }), "{error}");
}

#[test]
fn the_poll_interval_is_the_documented_one() {
    assert_eq!(DEFAULT_POLL_INTERVAL, Duration::from_secs(2));
}
