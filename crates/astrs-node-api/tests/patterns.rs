//! Services, actions and streams between two nodes (blueprint §9.4).
//!
//! Two real nodes, one real daemon, one real wire. The point of these tests is
//! that the three patterns need *no* machinery beyond edges and metadata: what
//! is exercised here is the same code path an ordinary message takes, plus a
//! handful of well-known keys.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use astrs_node_api::message::{AstrsMessage, Scalar, Text};
use astrs_node_api::patterns::{ChunkRef, StreamSegment};
use astrs_node_api::prelude::*;
use astrs_node_api::testing::MockDaemon;
use astrs_wire::{GoalStatus, InputSpec, NodeSource, NodeSpawnSpec, OutputSpec};

/// The deadline every wait in this file uses.
const WAIT: Duration = Duration::from_secs(5);

/// A client/server pair wired both ways.
struct Pair {
    client: Node,
    client_events: EventStream,
    server: Node,
    server_events: EventStream,
}

fn pair(daemon: &MockDaemon, request: &str, response: &str) -> Pair {
    let client_spec = NodeSpawnSpec::new(
        daemon.dataflow(),
        NodeId::new("client").unwrap(),
        0,
        NodeSource::Dynamic,
    )
    .with_output(OutputSpec::new(DataId::new(request).unwrap()))
    .with_input(InputSpec::new(
        DataId::new(response).unwrap(),
        PortRef::from_parts("server", response).unwrap(),
    ));
    let server_spec = NodeSpawnSpec::new(
        daemon.dataflow(),
        NodeId::new("server").unwrap(),
        0,
        NodeSource::Dynamic,
    )
    .with_input(InputSpec::new(
        DataId::new(request).unwrap(),
        PortRef::from_parts("client", request).unwrap(),
    ))
    .with_output(OutputSpec::new(DataId::new(response).unwrap()));

    let (client, client_events) = daemon.connect_node(client_spec).unwrap();
    let (server, server_events) = daemon.connect_node(server_spec).unwrap();
    Pair {
        client,
        client_events,
        server,
        server_events,
    }
}

#[test]
fn a_service_round_trip_correlates_by_request_id() {
    let daemon = MockDaemon::start().unwrap();
    let Pair {
        client,
        mut client_events,
        server,
        mut server_events,
    } = pair(&daemon, "request", "response");

    let mut client = client;
    let mut request_out = client.output::<Text>("request").unwrap();
    let first = client.service_request(&mut request_out, "ping").unwrap();
    let second = client.service_request(&mut request_out, "pong").unwrap();
    assert_ne!(first, second, "each exchange gets its own id");

    // The server answers both, deliberately in reverse order.
    let mut server = server;
    let mut response_out = server.output::<Text>("response").unwrap();
    let mut requests = Vec::new();
    while requests.len() < 2 {
        let event = server_events
            .recv_timeout(WAIT)
            .unwrap()
            .expect("a request");
        let request = ServiceRequest::from_event(event).expect("a correlated request");
        requests.push(request);
    }
    for request in requests.iter().rev() {
        let body = request.view::<Text>().unwrap().into_string();
        server
            .service_response(&mut response_out, &request.metadata, format!("{body}!"))
            .unwrap();
    }

    // The client matches answers to questions by id, not by arrival order.
    let mut answers = Vec::new();
    while answers.len() < 2 {
        let event = client_events
            .recv_timeout(WAIT)
            .unwrap()
            .expect("a response");
        let response = ServiceResponse::from_event(event).expect("a correlated response");
        let body = response.view::<Text>().unwrap().into_string();
        answers.push((response.id, body));
    }
    let for_first = answers
        .iter()
        .find(|(id, _)| id == &first)
        .expect("an answer to the first request");
    assert_eq!(for_first.1, "ping!");
    let for_second = answers
        .iter()
        .find(|(id, _)| id == &second)
        .expect("an answer to the second request");
    assert_eq!(for_second.1, "pong!");
}

#[test]
fn an_action_walks_its_finite_state_machine() {
    let daemon = MockDaemon::start().unwrap();
    let Pair {
        mut client,
        mut client_events,
        mut server,
        mut server_events,
    } = pair(&daemon, "goal", "status");

    let mut goal_out = client.output::<Scalar<f64>>("goal").unwrap();
    let goal = client.goal(&mut goal_out, 10.0_f64).unwrap();
    let mut tracker = GoalTracker::new();
    tracker.track(goal.clone());
    assert_eq!(tracker.status(&goal), Some(GoalStatus::Accepted));

    let event = server_events.recv_timeout(WAIT).unwrap().expect("the goal");
    assert_eq!(ActionOutcome::goal_of(&event), Some(goal.clone()));
    let (_, meta, _) = event.into_input().unwrap();
    assert_eq!(meta.goal_status(), Some(GoalStatus::Accepted));

    let mut status_out = server.output::<Scalar<f64>>("status").unwrap();
    for (status, progress) in [
        (GoalStatus::Executing, 0.25_f64),
        (GoalStatus::Executing, 0.75),
        (GoalStatus::Succeeded, 1.0),
    ] {
        server
            .goal_status(&mut status_out, &goal, status, progress)
            .unwrap();
    }

    let mut terminal = None;
    let mut feedback = 0;
    while terminal.is_none() {
        let event = client_events
            .recv_timeout(WAIT)
            .unwrap()
            .expect("an update");
        let Ok(update) = ActionOutcome::from_event(event) else {
            continue;
        };
        let status = tracker.observe(&update).unwrap();
        if update.is_feedback() {
            feedback += 1;
        }
        if update.is_terminal() {
            terminal = Some(status);
        }
    }
    assert_eq!(terminal, Some(GoalStatus::Succeeded));
    assert_eq!(feedback, 2, "two Executing updates carried progress");
    assert_eq!(tracker.in_flight(), 0);
}

#[test]
fn an_illegal_status_sequence_is_caught_by_the_client() {
    let daemon = MockDaemon::start().unwrap();
    let Pair {
        mut client,
        mut client_events,
        mut server,
        mut server_events,
    } = pair(&daemon, "goal", "status");

    let mut goal_out = client.output::<Scalar<f64>>("goal").unwrap();
    let goal = client.goal(&mut goal_out, 1.0_f64).unwrap();
    let mut tracker = GoalTracker::new();
    tracker.track(goal.clone());
    let _ = server_events.recv_timeout(WAIT).unwrap().expect("the goal");

    let mut status_out = server.output::<Scalar<f64>>("status").unwrap();
    // A misbehaving server: terminal, then alive again.
    server
        .goal_status(&mut status_out, &goal, GoalStatus::Succeeded, 1.0_f64)
        .unwrap();
    server
        .goal_status(&mut status_out, &goal, GoalStatus::Executing, 0.5_f64)
        .unwrap();

    let mut refused = false;
    for _ in 0..2 {
        let event = client_events
            .recv_timeout(WAIT)
            .unwrap()
            .expect("an update");
        let Ok(update) = ActionOutcome::from_event(event) else {
            continue;
        };
        if tracker.observe(&update).is_err() {
            refused = true;
        }
    }
    assert!(refused, "the FSM refused the resurrection");
}

#[test]
fn a_stream_reassembles_across_segments() {
    let daemon = MockDaemon::start().unwrap();
    let Pair {
        mut client,
        client_events: _client_events,
        server: _server,
        mut server_events,
    } = pair(&daemon, "chunks", "unused");

    let mut output = client.raw_output("chunks").unwrap();
    let mut writer = StreamWriter::with_session("bulk-transfer");

    let first: Vec<u8> = (0..300u32).map(|value| (value % 256) as u8).collect();
    let second: Vec<u8> = (0..100u32).map(|value| (value % 17) as u8).collect();
    let first_chunks = client
        .stream_segment(&mut output, &mut writer, &first, 128)
        .unwrap();
    let second_chunks = client
        .stream_segment(&mut output, &mut writer, &second, 128)
        .unwrap();
    assert_eq!(first_chunks, 3);
    assert_eq!(second_chunks, 1);
    assert_eq!(writer.finished_segments(), 2);

    let mut assembler = StreamAssembler::new();
    let mut segments: Vec<StreamSegment> = Vec::new();
    while segments.len() < 2 {
        let event = server_events.recv_timeout(WAIT).unwrap().expect("a chunk");
        let (_, chunk): (DataId, ChunkRef) = Node::stream_chunk_of(&event).expect("a stream chunk");
        assert_eq!(chunk.session, "bulk-transfer");
        if let Ok(Some(segment)) = assembler.accept_event(event).unwrap() {
            segments.push(segment);
        }
    }
    assert_eq!(segments[0].bytes, first);
    assert_eq!(segments[0].segment, 0);
    assert_eq!(segments[0].chunks, 3);
    assert_eq!(segments[1].bytes, second);
    assert_eq!(segments[1].segment, 1);
    assert_eq!(assembler.completed(), 2);
    assert!(assembler.open_segments().is_empty());
}

#[test]
fn correlated_messages_are_immune_to_a_full_queue() {
    // A one-deep queue flooded with ordinary messages still delivers the
    // correlated one (§11.2) — which is what keeps a service client from
    // waiting forever.
    let daemon = MockDaemon::start().unwrap();
    let spec = NodeSpawnSpec::new(
        daemon.dataflow(),
        NodeId::new("server").unwrap(),
        0,
        NodeSource::Dynamic,
    )
    .with_input(
        InputSpec::new(
            DataId::new("request").unwrap(),
            PortRef::from_parts("client", "request").unwrap(),
        )
        .with_queue(1, astrs_wire::QueuePolicy::DropOldest),
    );
    let (server, events) = daemon.connect_node(spec).unwrap();
    let input = DataId::new("request").unwrap();

    let mut correlated = server.metadata();
    correlated.set_request_id("must-survive");
    daemon
        .send_input(server.id(), &input, correlated, vec![1])
        .unwrap();
    for value in 0..30u8 {
        daemon
            .send_input(server.id(), &input, server.metadata(), vec![value])
            .unwrap();
    }
    // Wait for the flood to have been enqueued and trimmed, rather than
    // sleeping a guessed interval.
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let dropped = events
            .source()
            .queue_snapshot(&input)
            .map_or(0, |snapshot| snapshot.dropped);
        if dropped >= 25 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the flood never landed"
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    let mut survived = false;
    while let Some(event) = events.source().try_next() {
        if let Event::Input { meta, .. } = event
            && meta.request_id() == Some("must-survive")
        {
            survived = true;
        }
    }
    assert!(survived);
}

#[test]
fn the_metadata_keys_are_the_ones_the_protocol_reserves() {
    // A guard against a rename: the patterns are metadata, and the metadata
    // keys are §6.1's.
    use astrs_wire::metadata::keys;
    assert_eq!(keys::REQUEST_ID, "request_id");
    assert_eq!(keys::GOAL_ID, "goal_id");
    assert_eq!(keys::GOAL_STATUS, "goal_status");
    assert_eq!(keys::SESSION_ID, "session_id");
    assert_eq!(keys::SEGMENT_ID, "segment_id");
    assert_eq!(keys::SEQ, "seq");
    assert_eq!(keys::FIN, "fin");
    assert_eq!(keys::FLUSH, "flush");
    assert_eq!(keys::CORRELATION, &["request_id", "goal_id", "goal_status"]);
    assert_eq!(<Text as AstrsMessage>::URN, "std/core/v1/String");
}
