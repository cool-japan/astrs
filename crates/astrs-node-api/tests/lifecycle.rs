//! The full node lifecycle, over the real wire (blueprint §7.3, §9.1).
//!
//! Every test here drives an actual conversation — greeting, `Register`,
//! `Registered`, `Subscribe`, inputs, outputs, `Stop` — between a real
//! [`Node`] and the in-process [`MockDaemon`]. Nothing is stubbed except the
//! socket, so a change that breaks the protocol breaks these.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use astrs_node_api::prelude::*;
use astrs_node_api::testing::{MockDaemon, TestHarness};
use astrs_wire::{
    InputSpec, NodeRequest, NodeSource, NodeSpawnSpec, OutputSpec, RouteCloseReason, WireMessage,
};

/// The deadline every wait in this file uses.
const WAIT: Duration = Duration::from_secs(5);

fn spec(daemon: &MockDaemon, node: &str) -> NodeSpawnSpec {
    NodeSpawnSpec::new(
        daemon.dataflow(),
        NodeId::new(node).unwrap(),
        0,
        NodeSource::Dynamic,
    )
    .with_input(InputSpec::new(
        DataId::new("frames").unwrap(),
        PortRef::from_parts("camera", "image").unwrap(),
    ))
    .with_output(OutputSpec::new(DataId::new("detections").unwrap()))
}

#[test]
fn the_conversation_runs_register_subscribe_inputs_stop() {
    let daemon = MockDaemon::start().unwrap();
    let (mut node, mut events) = daemon.connect_node(spec(&daemon, "detect")).unwrap();
    let id = node.id().clone();

    // Register + Subscribe, in that order.
    daemon
        .wait_for(WAIT, |requests| {
            requests
                .iter()
                .any(|entry| matches!(entry.request, NodeRequest::Subscribe { .. }))
        })
        .unwrap();
    let verbs: Vec<&str> = daemon
        .requests()
        .iter()
        .map(|entry| entry.request.variant_name())
        .collect();
    assert_eq!(&verbs[..2], &["Register", "Subscribe"]);
    assert!(daemon.is_subscribed(&id));

    // Inputs.
    for value in 0..3u8 {
        daemon
            .send_input(
                &id,
                &DataId::new("frames").unwrap(),
                node.metadata(),
                vec![value],
            )
            .unwrap();
    }
    let mut received = Vec::new();
    while received.len() < 3 {
        let event = events.recv_timeout(WAIT).unwrap().expect("an input");
        let (input, _, data) = event.into_input().expect("an input event");
        assert_eq!(input.as_str(), "frames");
        received.push(data.to_vec());
    }
    assert_eq!(received, vec![vec![0], vec![1], vec![2]]);

    // Outputs.
    let mut detections = node.raw_output("detections").unwrap();
    detections.send_bytes(vec![9, 9], node.metadata()).unwrap();
    let sends = daemon
        .wait_for_sends(&id, &DataId::new("detections").unwrap(), 1, WAIT)
        .unwrap();
    assert_eq!(sends[0].bytes(), Some(&[9, 9][..]));

    // Stop, then the fuse.
    daemon.stop(&id, StopCause::Requested).unwrap();
    let event = events.recv_timeout(WAIT).unwrap().expect("the stop");
    assert!(event.is_stop());
    assert!(events.is_fused());
    assert!(events.recv_timeout(WAIT).unwrap().is_none());
}

#[test]
fn an_input_closing_reaches_the_node_after_its_queued_frames() {
    let daemon = MockDaemon::start().unwrap();
    let (node, mut events) = daemon.connect_node(spec(&daemon, "detect")).unwrap();
    let id = node.id().clone();
    let input = DataId::new("frames").unwrap();

    daemon
        .send_input(&id, &input, node.metadata(), vec![1])
        .unwrap();
    daemon
        .close_input(&id, &input, RouteCloseReason::ProducerFinished)
        .unwrap();

    let first = events.recv_timeout(WAIT).unwrap().expect("the frame");
    assert!(first.is_input(), "the frame comes before the close");
    let second = events.recv_timeout(WAIT).unwrap().expect("the close");
    assert!(matches!(second, Event::InputClosed { .. }));
}

#[test]
fn a_stop_pre_empts_a_backlog_of_frames() {
    let daemon = MockDaemon::start().unwrap();
    let (node, mut events) = daemon.connect_node(spec(&daemon, "detect")).unwrap();
    let id = node.id().clone();
    let input = DataId::new("frames").unwrap();

    for value in 0..5u8 {
        daemon
            .send_input(&id, &input, node.metadata(), vec![value])
            .unwrap();
    }
    daemon.stop(&id, StopCause::DataflowFinished).unwrap();

    // Read only once the session has ingested the stop.
    //
    // All six messages travel one ordered socket, so a stop sitting on the
    // control lane proves the five frames written before it are already in the
    // input's queue — which is precisely the backlog §11.3 says the control
    // lane pre-empts. Reading earlier races the pump: this thread can drain
    // frames as they trickle in and never build a backlog at all, which is a
    // measurement of scheduling luck rather than of the promise. That race is
    // why this test failed intermittently before the wait was added.
    let deadline = Instant::now() + WAIT;
    while events.stats().control_depth == 0 {
        assert!(
            Instant::now() < deadline,
            "the stop never reached the event stream"
        );
        std::thread::yield_now();
    }

    // Guard against a vacuous pass: with nothing queued there is nothing to
    // pre-empt. `queue_size` defaults to ten and the policy only evicts when
    // full, so all five are still waiting.
    let backlog = events
        .source()
        .queue_snapshot(&input)
        .expect("the input has a queue");
    assert_eq!(backlog.depth, 5, "the backlog must still be waiting");
    assert_eq!(backlog.dropped, 0, "nothing should have been evicted");

    // The control lane wins however deep the data lane is (§11.3): the stop is
    // the very next event, ahead of every one of the five queued frames.
    let first = events.recv_timeout(WAIT).unwrap().expect("an event");
    assert!(
        first.is_stop(),
        "the stop must arrive before any queued frame, got {first}"
    );
    assert_eq!(events.stop_cause(), Some(StopCause::DataflowFinished));

    // And the stop fuses the stream, so the backlog is never delivered.
    assert!(events.is_fused());
    assert!(events.recv_timeout(WAIT).unwrap().is_none());
}

#[test]
fn dropping_the_node_closes_its_outputs_as_a_teardown() {
    // Blueprint §12: both disappearing handles — the `RawOutput` here and the
    // `Node` itself — announce themselves with `CloseOutputs`, the request
    // §7.3 defines as "what a node does as it exits". That spelling is what
    // lets the daemon wait for the exit status before deciding whether this
    // producer finished or crashed; `OutputDone` would commit it to
    // "finished" before the process had even exited.
    let daemon = MockDaemon::start().unwrap();
    let (mut node, events) = daemon.connect_node(spec(&daemon, "detect")).unwrap();
    let id = node.id().clone();
    {
        let mut out = node.raw_output("detections").unwrap();
        out.send_bytes(vec![1], node.metadata()).unwrap();
    }
    drop(events);
    drop(node);

    daemon
        .wait_for(WAIT, |requests| {
            requests.iter().any(|entry| {
                entry.node == id && matches!(entry.request, NodeRequest::CloseOutputs { .. })
            })
        })
        .unwrap();
    assert!(
        daemon.requests().iter().all(|entry| {
            entry.node != id || !matches!(entry.request, NodeRequest::OutputDone { .. })
        }),
        "nothing on a teardown path may claim the producer simply finished"
    );
}

#[test]
fn a_restarted_node_reports_its_generation() {
    let daemon = MockDaemon::start().unwrap();
    let spec = NodeSpawnSpec::new(
        daemon.dataflow(),
        NodeId::new("detect").unwrap(),
        4,
        NodeSource::Dynamic,
    );
    let (node, _events) = daemon.connect_node(spec).unwrap();
    assert!(node.is_restart());
    assert_eq!(node.restart_count(), 4);
    assert_eq!(node.generation(), 4);
    assert_eq!(node.descriptor().generation, 4);
}

#[test]
fn parameter_updates_reach_the_node() {
    let daemon = MockDaemon::start().unwrap();
    let (node, mut events) = daemon.connect_node(spec(&daemon, "detect")).unwrap();
    daemon
        .set_param(
            node.id(),
            astrs_wire::ParamKey::new("gain").unwrap(),
            astrs_wire::Parameter::from(2.5),
        )
        .unwrap();
    let event = events.recv_timeout(WAIT).unwrap().expect("the update");
    let Event::ParamUpdate { key, value, .. } = event else {
        panic!("expected a parameter update, got {event}");
    };
    assert_eq!(key.as_str(), "gain");
    assert_eq!(value, astrs_wire::Parameter::Float(2.5));
}

#[test]
fn the_iterator_face_drives_a_whole_node_loop() {
    let mut harness = TestHarness::start().unwrap();
    let id = harness.node_id();
    for value in 0..3u8 {
        harness.feed(vec![value]).unwrap();
    }
    harness.daemon.stop(&id, StopCause::Requested).unwrap();

    let mut inputs = 0;
    let mut stopped = false;
    for event in &mut harness.events {
        match event {
            Event::Input { .. } => inputs += 1,
            Event::Stop(_) => {
                stopped = true;
                break;
            }
            _ => {}
        }
    }
    assert!(stopped, "the loop ended on the stop");
    assert!(inputs <= 3);
}

#[test]
fn session_counters_track_both_directions() {
    let daemon = MockDaemon::start().unwrap();
    let (mut node, mut events) = daemon.connect_node(spec(&daemon, "detect")).unwrap();
    let id = node.id().clone();

    let mut out = node.raw_output("detections").unwrap();
    out.send_bytes(vec![1, 2, 3], node.metadata()).unwrap();
    daemon
        .send_input(
            &id,
            &DataId::new("frames").unwrap(),
            node.metadata(),
            vec![4],
        )
        .unwrap();
    let _ = events.recv_timeout(WAIT).unwrap().expect("the input");

    let stats = node.stats();
    // `Register` and `Subscribe` are written straight to the link during the
    // handshake, before the writer task exists, so they are not counted here.
    assert!(stats.requests_sent >= 1, "the publish was counted");
    assert_eq!(stats.sends_inline, 1);
    assert_eq!(stats.sends_zero_copy, 0);
    assert!(stats.events_received >= 1);
}
