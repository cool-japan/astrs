//! The node leg of §24.1, verb by verb.
//!
//! Blueprint §7.3 and §24.1 freeze eleven `NodeRequest` verbs and fifteen
//! `NodeEvent` variants on the daemon↔node leg. A node API that quietly never
//! emits one of the verbs, or quietly ignores one of the events, is a
//! conformance hole that no round-trip test would catch — both ends would
//! agree, and both would be wrong about the protocol.
//!
//! So this file walks the list. For every request a node can originate, it
//! drives the public API that should produce it and asserts the daemon saw it;
//! for every event a node can receive, it injects one and asserts the
//! observable reaction.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use astrs_node_api::prelude::*;
use astrs_node_api::testing::{MockDaemon, TestHarness};
use astrs_wire::{
    ExtensionKey, NodeExitCause, NodeRequest, NodeSource, NodeSpawnSpec, OutputSpec,
    RouteCloseReason, RouteDowngradeReason, ShmSegmentSpec, WireMessage,
};

/// The deadline every wait in this file uses.
const WAIT: Duration = Duration::from_secs(5);

/// Waits until the daemon has seen a request with `name`.
fn saw(daemon: &MockDaemon, name: &'static str) -> bool {
    daemon
        .wait_for(WAIT, |requests| {
            requests
                .iter()
                .any(|entry| entry.request.variant_name() == name)
        })
        .is_ok()
}

#[test]
fn every_request_a_node_originates_is_reachable_from_the_public_api() {
    let daemon = MockDaemon::start().unwrap();
    let spec = NodeSpawnSpec::new(
        daemon.dataflow(),
        NodeId::new("conformer").unwrap(),
        0,
        NodeSource::Dynamic,
    )
    .with_output(OutputSpec::new(DataId::new("out").unwrap()));
    let (mut node, events) = daemon.connect_node(spec).unwrap();

    // 0 Register, 1 Subscribe — the handshake.
    assert!(saw(&daemon, "Register"));
    assert!(saw(&daemon, "Subscribe"));

    // 2 SendMessage, 3 OutputDone.
    {
        let mut out = node.raw_output("out").unwrap();
        out.send_bytes(vec![1, 2, 3], node.metadata()).unwrap();
        assert!(saw(&daemon, "SendMessage"));
        out.close().unwrap();
        assert!(saw(&daemon, "OutputDone"));
    }

    // 5 NextEvent — the pull style.
    node.request_events(8, Some(Duration::from_millis(50)))
        .unwrap();
    assert!(saw(&daemon, "NextEvent"));
    node.request_next_event().unwrap();

    // 7 ExtStore, 8 ExtLoad, 9 ExtDrop.
    let key = ExtensionKey::user("conformance").unwrap();
    node.ext_store(&key, vec![7]).unwrap();
    assert!(saw(&daemon, "ExtStore"));
    assert_eq!(node.ext_load(&key).unwrap(), Some(vec![7]));
    assert!(saw(&daemon, "ExtLoad"));
    node.ext_drop(&key).unwrap();
    assert!(saw(&daemon, "ExtDrop"));

    // 6 EventStreamDropped — a node that stops reading tells the daemon so.
    drop(events);
    assert!(
        saw(&daemon, "EventStreamDropped"),
        "§7.3: the daemon must stop queueing for a node that stopped reading"
    );

    // 4 CloseOutputs — what a node does as it exits.
    node.shutdown().unwrap();
    assert!(saw(&daemon, "CloseOutputs"));
}

#[test]
fn the_route_upgrade_acknowledgement_is_sent_for_both_answers() {
    // 10 RouteUpgradeAck. The refusal path needs no shared memory at all: a
    // segment name that does not match the node's own key is refused before
    // anything is mapped.
    let daemon = MockDaemon::start().unwrap();
    let spec = NodeSpawnSpec::new(
        daemon.dataflow(),
        NodeId::new("conformer").unwrap(),
        0,
        NodeSource::Dynamic,
    )
    .with_output(OutputSpec::new(DataId::new("out").unwrap()));
    let (node, _events) = daemon.connect_node(spec).unwrap();

    daemon
        .upgrade_route(
            node.id(),
            &DataId::new("out").unwrap(),
            ShmSegmentSpec::new("astrs-not-this-nodes-segment", 0, 4, 4096),
            Vec::new(),
        )
        .unwrap();

    daemon
        .wait_for(WAIT, |requests| {
            requests.iter().any(|entry| {
                matches!(
                    entry.request,
                    NodeRequest::RouteUpgradeAck {
                        accepted: false,
                        reason: Some(_),
                        ..
                    }
                )
            })
        })
        .expect("a refusal, with a reason");
}

#[test]
fn every_event_a_node_receives_produces_an_observable_reaction() {
    let mut harness = TestHarness::start().unwrap();
    let id = harness.node_id();
    let input = DataId::new(TestHarness::DEFAULT_INPUT).unwrap();
    let output = DataId::new(TestHarness::DEFAULT_OUTPUT).unwrap();

    // 0 Input.
    harness.feed(vec![1]).unwrap();
    assert!(harness.next_event().unwrap().is_input());

    // 1 InputClosed.
    harness
        .daemon
        .close_input(&id, &input, RouteCloseReason::ProducerFinished)
        .unwrap();
    assert!(matches!(
        harness.next_event().unwrap(),
        Event::InputClosed { .. }
    ));

    // 2 InputRecovered.
    harness
        .daemon
        .send_event(
            &id,
            astrs_wire::NodeEvent::InputRecovered {
                id: input.clone(),
                source: PortRef::from_parts("peer", "out").unwrap(),
                generation: 3,
            },
        )
        .unwrap();
    assert!(matches!(
        harness.next_event().unwrap(),
        Event::InputRecovered { generation: 3, .. }
    ));

    // 4 Reload.
    harness
        .daemon
        .send_event(
            &id,
            astrs_wire::NodeEvent::Reload {
                operator: None,
                path: Some("./new".to_owned()),
            },
        )
        .unwrap();
    assert!(matches!(
        harness.next_event().unwrap(),
        Event::Reload { .. }
    ));

    // 5 AllInputsClosed.
    harness
        .daemon
        .send_event(&id, astrs_wire::NodeEvent::AllInputsClosed)
        .unwrap();
    assert!(matches!(
        harness.next_event().unwrap(),
        Event::AllInputsClosed
    ));

    // 6 NodeFailed.
    harness
        .daemon
        .send_event(
            &id,
            astrs_wire::NodeEvent::NodeFailed {
                peer: NodeId::new("peer").unwrap(),
                cause: NodeExitCause::Success,
            },
        )
        .unwrap();
    let failed = harness.next_event().unwrap();
    assert!(matches!(failed, Event::NodeFailed { .. }));
    assert!(failed.is_fault());

    // 7 Restarted.
    harness
        .daemon
        .send_event(
            &id,
            astrs_wire::NodeEvent::Restarted {
                peer: NodeId::new("peer").unwrap(),
                generation: 2,
            },
        )
        .unwrap();
    assert!(matches!(
        harness.next_event().unwrap(),
        Event::Restarted { generation: 2, .. }
    ));

    // 8 ParamUpdate, 9 ParamDeleted.
    harness
        .daemon
        .set_param(
            &id,
            astrs_wire::ParamKey::new("gain").unwrap(),
            astrs_wire::Parameter::Integer(3),
        )
        .unwrap();
    assert!(matches!(
        harness.next_event().unwrap(),
        Event::ParamUpdate { .. }
    ));
    harness
        .daemon
        .send_event(
            &id,
            astrs_wire::NodeEvent::ParamDeleted {
                scope: astrs_wire::messages::control::types::ParamScope::Global,
                key: astrs_wire::ParamKey::new("gain").unwrap(),
            },
        )
        .unwrap();
    assert!(matches!(
        harness.next_event().unwrap(),
        Event::ParamDeleted { .. }
    ));

    // 10 ExtDropped.
    harness
        .daemon
        .send_event(
            &id,
            astrs_wire::NodeEvent::ExtDropped {
                key: ExtensionKey::user("gone").unwrap(),
                reason: "ttl".to_owned(),
            },
        )
        .unwrap();
    assert!(matches!(
        harness.next_event().unwrap(),
        Event::ExtDropped { .. }
    ));

    // 12 RouteDowngrade — handled inside the session, not delivered.
    harness
        .daemon
        .downgrade_route(
            &id,
            &output,
            RouteDowngradeReason::ConsumerDetached {
                consumer: PortRef::from_parts("peer", "in").unwrap(),
            },
        )
        .unwrap();

    // 16 InputRouteDowngrade — the consumer-side twin, also handled inside the
    // session. Sending it for an input that was never on a ring is the case a
    // node meets after a fallback it took itself (§6.3), and must be a no-op
    // rather than an error on the stream.
    harness
        .daemon
        .send_event(
            &id,
            astrs_wire::NodeEvent::InputRouteDowngrade {
                input: input.clone(),
                reason: RouteDowngradeReason::SegmentClosed { generation: 1 },
            },
        )
        .unwrap();

    // 15 InputRouteUpgrade, naming a segment no key describes: refused, and
    // the refusal *is* the observable reaction (§6.2: never sleep-retry).
    harness
        .daemon
        .send_event(
            &id,
            astrs_wire::NodeEvent::InputRouteUpgrade {
                input: input.clone(),
                source: PortRef::from_parts("peer", "out").unwrap(),
                segment: astrs_wire::ShmSegmentSpec::new("astrs-no-such-segment", 1, 4, 1024),
                consumer: PortRef::new(id.clone(), input.clone()),
            },
        )
        .unwrap();
    let refused = harness.next_event().unwrap();
    assert!(matches!(refused, Event::Error(_)), "{refused}");
    assert_eq!(
        harness
            .node
            .input_plane(TestHarness::DEFAULT_INPUT)
            .as_str(),
        "daemon"
    );

    // 3 Stop — last, because it fuses the stream.
    harness.daemon.stop(&id, StopCause::Destroyed).unwrap();
    let stop = harness.next_event().unwrap();
    assert!(stop.is_stop());
    assert!(harness.events.is_fused());
    assert_eq!(harness.events.stop_cause(), Some(StopCause::Destroyed));
}

#[test]
fn a_second_registration_is_reported_rather_than_accepted() {
    // 13 Registered, out of place: the init handshake consumed the first one,
    // so a second means the daemon is confused about this session.
    let mut harness = TestHarness::start().unwrap();
    let id = harness.node_id();
    let spec = harness.node.descriptor().clone();
    harness
        .daemon
        .send_event(
            &id,
            astrs_wire::NodeEvent::Registered {
                spec: Box::new(spec),
                session: astrs_wire::SessionId::from_u128(99),
            },
        )
        .unwrap();
    let event = harness.next_event().unwrap();
    let Event::Error(message) = event else {
        panic!("expected an error, got {event}");
    };
    assert!(message.contains("second"), "{message}");
}

#[test]
fn an_unsolicited_extension_value_is_absorbed_rather_than_delivered() {
    // 14 ExtValue with nobody waiting: not an error, but never an event.
    let mut harness = TestHarness::start().unwrap();
    let id = harness.node_id();
    harness
        .daemon
        .send_event(
            &id,
            astrs_wire::NodeEvent::ExtValue {
                key: ExtensionKey::user("nobody-asked").unwrap(),
                value: Some(vec![1]),
            },
        )
        .unwrap();
    // Something else, to prove the stream is still live and ordered.
    harness.feed(vec![9]).unwrap();
    let event = harness.next_event().unwrap();
    assert!(
        event.is_input(),
        "the ExtValue never reached the node, {event}"
    );
}

#[test]
fn the_frozen_variant_names_are_the_ones_this_crate_speaks() {
    // A guard against a rename upstream: the API above is written against
    // these names, and the §24.1 snapshot freezes them.
    assert_eq!(
        &NodeRequest::VARIANT_NAMES[..11],
        &[
            "Register",
            "Subscribe",
            "SendMessage",
            "OutputDone",
            "CloseOutputs",
            "NextEvent",
            "EventStreamDropped",
            "ExtStore",
            "ExtLoad",
            "ExtDrop",
            "RouteUpgradeAck",
        ]
    );
    // Twelve: §24.1's eleven, plus `ReportDeadlineViolation` (§11.3) — this
    // crate is the one that sends it (`EventSource`'s deadline-violation
    // relay hook).
    assert_eq!(NodeRequest::VARIANT_NAMES.len(), 12);
    assert_eq!(NodeRequest::VARIANT_NAMES[11], "ReportDeadlineViolation");
    // Eighteen: §24.1's thirteen, `Registered` and `ExtValue`, the
    // consumer-side route pair this crate acts on in `session::inputs`, and
    // `DeadlineViolated` (§11.3) — the daemon's relay of the request above,
    // delivered back on `astrs/status`.
    assert_eq!(astrs_wire::NodeEvent::VARIANT_NAMES.len(), 18);
    assert_eq!(
        &astrs_wire::NodeEvent::VARIANT_NAMES[13..],
        &[
            "Registered",
            "ExtValue",
            "InputRouteUpgrade",
            "InputRouteDowngrade",
            "DeadlineViolated",
        ]
    );
}
