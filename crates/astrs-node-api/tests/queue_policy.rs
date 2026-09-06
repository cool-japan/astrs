//! Client-side input queues (blueprint §11.2), end to end.
//!
//! The queue policy is applied **in the node**, not in the daemon: a
//! `queue_size: 1` input means "this node always sees the newest frame"
//! whatever the daemon batched, and eviction immunity protects a correlated
//! message from ever being the one dropped.
//!
//! These tests drive real frames through a real session, so what they assert
//! is what a deployed node would actually observe.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use astrs_node_api::prelude::*;
use astrs_node_api::testing::MockDaemon;
use astrs_wire::{InputSpec, NodeSource, NodeSpawnSpec, PriorityLane, QueuePolicy};

/// The deadline every wait in this file uses.
const WAIT: Duration = Duration::from_secs(5);

/// Polls `condition` until it holds, or fails at the deadline.
///
/// Every wait in this file is a *condition* rather than a sleep: a fixed sleep
/// is either flaky on a loaded machine or slow on an idle one, and it proves
/// nothing about what the node actually did.
fn poll_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + WAIT;
    while !condition() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn spec_with_input(
    daemon: &MockDaemon,
    size: u32,
    policy: QueuePolicy,
    lane: PriorityLane,
) -> NodeSpawnSpec {
    let mut input = InputSpec::new(
        DataId::new("frames").unwrap(),
        PortRef::from_parts("camera", "image").unwrap(),
    )
    .with_queue(size, policy);
    input.priority_lane = lane;
    NodeSpawnSpec::new(
        daemon.dataflow(),
        NodeId::new("detect").unwrap(),
        0,
        NodeSource::Dynamic,
    )
    .with_input(input)
}

/// Feeds `count` frames and drains whatever the node ended up with.
fn feed_and_drain(
    daemon: &MockDaemon,
    node: &Node,
    events: &mut EventStream,
    count: u8,
) -> Vec<Vec<u8>> {
    let input = DataId::new("frames").unwrap();
    for value in 0..count {
        daemon
            .send_input(node.id(), &input, node.metadata(), vec![value])
            .unwrap();
    }
    // A marker on the control lane tells us the data lane has been fully
    // delivered to the queues: the daemon writes it after every frame, and the
    // node's reader processes frames in order.
    daemon.stop(node.id(), StopCause::Requested).unwrap();

    let mut received = Vec::new();
    while let Ok(Some(event)) = events.recv_timeout(WAIT) {
        match event {
            Event::Input { data, .. } => received.push(data.to_vec()),
            Event::Stop(_) => break,
            _ => {}
        }
    }
    // Drain whatever was queued behind the stop; the stream has fused, so read
    // the queues directly.
    while let Some(event) = events.source().try_next() {
        if let Event::Input { data, .. } = event {
            received.push(data.to_vec());
        }
    }
    received
}

#[test]
fn drop_oldest_keeps_the_newest_frames() {
    let daemon = MockDaemon::start().unwrap();
    let (node, mut events) = daemon
        .connect_node(spec_with_input(
            &daemon,
            2,
            QueuePolicy::DropOldest,
            PriorityLane::Data,
        ))
        .unwrap();

    let received = feed_and_drain(&daemon, &node, &mut events, 20);
    assert!(
        received.len() <= 20,
        "a bounded queue cannot deliver more than was sent"
    );
    if let Some(last) = received.last() {
        assert!(
            last[0] >= 17,
            "drop_oldest keeps the newest: last was {last:?}"
        );
    }
}

#[test]
fn the_queue_depth_never_exceeds_the_effective_capacity() {
    let daemon = MockDaemon::start().unwrap();
    let (node, events) = daemon
        .connect_node(spec_with_input(
            &daemon,
            2,
            QueuePolicy::DropOldest,
            PriorityLane::Data,
        ))
        .unwrap();
    let input = DataId::new("frames").unwrap();

    for value in 0..50u8 {
        daemon
            .send_input(node.id(), &input, node.metadata(), vec![value])
            .unwrap();
    }
    // Wait for the flood to have been enqueued *and* trimmed, checking the
    // ceiling on every observation rather than only at the end.
    poll_until("the queue to start dropping", || {
        let snapshot = events.source().queue_snapshot(&input).expect("the queue");
        assert!(
            snapshot.depth <= u64::from(snapshot.effective_capacity),
            "depth {} over capacity {}",
            snapshot.depth,
            snapshot.effective_capacity
        );
        snapshot.dropped > 0
    });

    let snapshot = events.source().queue_snapshot(&input).expect("the queue");
    assert_eq!(snapshot.capacity, 2);
    assert_eq!(
        snapshot.effective_capacity, 2,
        "drop_oldest does not buffer"
    );
    assert!(
        snapshot.depth <= u64::from(snapshot.effective_capacity),
        "depth {} over capacity {}",
        snapshot.depth,
        snapshot.effective_capacity
    );
    assert!(snapshot.dropped > 0, "frames were dropped, as they must be");
}

#[test]
fn backpressure_buffers_ten_times_the_queue_size() {
    let daemon = MockDaemon::start().unwrap();
    let (node, events) = daemon
        .connect_node(spec_with_input(
            &daemon,
            2,
            QueuePolicy::Backpressure,
            PriorityLane::Data,
        ))
        .unwrap();
    let input = DataId::new("frames").unwrap();

    for value in 0..40u8 {
        daemon
            .send_input(node.id(), &input, node.metadata(), vec![value])
            .unwrap();
    }
    poll_until("the buffer to fill", || {
        let snapshot = events.source().queue_snapshot(&input).expect("the queue");
        assert!(snapshot.depth <= 20, "backpressure buffers 10x, no more");
        snapshot.depth == 20
    });

    let snapshot = events.source().queue_snapshot(&input).expect("the queue");
    assert_eq!(snapshot.capacity, 2);
    assert_eq!(snapshot.effective_capacity, 20, "ten times queue_size");
}

#[test]
fn a_correlated_message_survives_a_flood_of_ordinary_ones() {
    let daemon = MockDaemon::start().unwrap();
    let (node, events) = daemon
        .connect_node(spec_with_input(
            &daemon,
            2,
            QueuePolicy::DropOldest,
            PriorityLane::Data,
        ))
        .unwrap();
    let input = DataId::new("frames").unwrap();

    let mut correlated = node.metadata();
    correlated.set_request_id("req-immune");
    daemon
        .send_input(node.id(), &input, correlated, vec![255])
        .unwrap();
    for value in 0..40u8 {
        daemon
            .send_input(node.id(), &input, node.metadata(), vec![value])
            .unwrap();
    }
    poll_until("the flood to be enqueued and trimmed", || {
        events
            .source()
            .queue_snapshot(&input)
            .is_some_and(|snapshot| snapshot.dropped >= 30)
    });

    let mut found = false;
    while let Some(event) = events.source().try_next() {
        if let Event::Input { meta, .. } = event
            && meta.request_id() == Some("req-immune")
        {
            found = true;
        }
    }
    assert!(found, "the correlated message was never evicted (§11.2)");
}

#[test]
fn a_control_lane_input_pre_empts_a_data_lane_backlog() {
    let daemon = MockDaemon::start().unwrap();
    let spec = NodeSpawnSpec::new(
        daemon.dataflow(),
        NodeId::new("detect").unwrap(),
        0,
        NodeSource::Dynamic,
    )
    .with_input(
        InputSpec::new(
            DataId::new("frames").unwrap(),
            PortRef::from_parts("camera", "image").unwrap(),
        )
        .with_queue(32, QueuePolicy::DropOldest),
    )
    .with_input({
        let mut urgent = InputSpec::new(
            DataId::new("estop").unwrap(),
            PortRef::from_parts("safety", "stop").unwrap(),
        )
        .with_queue(4, QueuePolicy::DropOldest);
        urgent.priority_lane = PriorityLane::Control;
        urgent
    });
    let (node, events) = daemon.connect_node(spec).unwrap();

    for value in 0..10u8 {
        daemon
            .send_input(
                node.id(),
                &DataId::new("frames").unwrap(),
                node.metadata(),
                vec![value],
            )
            .unwrap();
    }
    daemon
        .send_input(
            node.id(),
            &DataId::new("estop").unwrap(),
            node.metadata(),
            vec![1],
        )
        .unwrap();
    poll_until("the urgent input to be queued", || {
        events
            .source()
            .queue_snapshot(&DataId::new("estop").unwrap())
            .is_some_and(|snapshot| snapshot.depth >= 1)
    });

    let first = events.source().try_next().expect("an event");
    assert_eq!(
        first.input().map(DataId::as_str),
        Some("estop"),
        "the control lane is served first (§11.3)"
    );
}

#[test]
fn an_exhausted_backpressure_buffer_is_reported_as_an_error_event() {
    let daemon = MockDaemon::start().unwrap();
    let (node, events) = daemon
        .connect_node(spec_with_input(
            &daemon,
            1,
            QueuePolicy::Backpressure,
            PriorityLane::Data,
        ))
        .unwrap();
    let input = DataId::new("frames").unwrap();

    for value in 0..40u8 {
        daemon
            .send_input(node.id(), &input, node.metadata(), vec![value])
            .unwrap();
    }
    poll_until("the buffer to be exhausted", || {
        events.stats().queue_signals > 0
    });

    let mut errors = 0;
    while let Some(event) = events.source().try_next() {
        if matches!(event, Event::Error(_)) {
            errors += 1;
        }
    }
    assert!(errors > 0, "§11.2 requires an ERROR, not a silent drop");
    assert!(events.stats().queue_signals > 0);
}
