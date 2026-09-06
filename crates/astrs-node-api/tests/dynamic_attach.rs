//! A `path: dynamic` node attaching itself over a real socket (blueprint
//! §4.2, §8.3).
//!
//! Every other test in this crate hands the node one end of an in-process
//! duplex. These use an actual Unix-domain socket, so the dial, the
//! `SO_PEERCRED` pre-filter, the greeting and the framing all run the code a
//! deployed node runs — the one path a duplex cannot exercise.
//!
//! > **Daemon** — […] Local node listener on **TCP 7408** (loopback) and a
//! > Unix domain socket (`$XDG_RUNTIME_DIR/astrs/daemon.sock`) — UDS
//! > preferred, TCP fallback.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(unix)]

use std::time::Duration;

use astrs_node_api::prelude::*;
use astrs_node_api::testing::MockDaemon;
use astrs_wire::{NodeRequest, WireMessage};

/// The deadline every wait in this file uses.
const WAIT: Duration = Duration::from_secs(5);

/// A socket path unique to this test and this process.
fn socket_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("astrs-node-api-{tag}-{}.sock", std::process::id()))
}

#[test]
fn a_dynamic_node_attaches_itself_over_a_unix_socket() {
    let daemon = MockDaemon::start().unwrap();
    let endpoint = daemon.listen_unix(socket_path("attach")).unwrap();
    assert!(endpoint.path().exists(), "the socket was bound");
    assert!(endpoint.endpoint().starts_with("uds://"));

    let (mut node, mut events) = Node::builder()
        .node_id("probe")
        .unwrap()
        .dataflow(daemon.dataflow())
        .dynamic(true)
        .input("frames")
        .unwrap()
        .output("samples")
        .unwrap()
        .daemon(endpoint.endpoint())
        .orphan_guard(false)
        .connect()
        .expect("a dynamic attach over a real socket");

    assert_eq!(node.id().as_str(), "probe");
    assert_eq!(node.dataflow_id(), daemon.dataflow());
    assert!(!node.is_restart());

    // The daemon admitted it from the handshake alone.
    daemon
        .wait_for(WAIT, |requests| {
            requests.iter().any(|entry| {
                entry.node.as_str() == "probe"
                    && matches!(entry.request, NodeRequest::Subscribe { .. })
            })
        })
        .unwrap();
    let register = daemon
        .requests()
        .into_iter()
        .find(|entry| entry.request.variant_name() == "Register")
        .expect("a registration");
    let handshake = register.request.handshake().expect("a handshake").clone();
    assert!(handshake.dynamic, "§8.3: the node attached itself");
    assert_eq!(handshake.inputs.len(), 1);
    assert_eq!(handshake.outputs.len(), 1);
    assert_eq!(handshake.pid, Some(std::process::id()));

    // Its declared wiring became a real specification.
    assert_eq!(node.descriptor().inputs.len(), 1);
    assert_eq!(node.descriptor().outputs.len(), 1);

    // And it works: publish, then receive.
    let mut samples = node.raw_output("samples").unwrap();
    samples.send_bytes(vec![1, 2, 3], node.metadata()).unwrap();
    let sends = daemon
        .wait_for_sends(node.id(), &DataId::new("samples").unwrap(), 1, WAIT)
        .unwrap();
    assert_eq!(sends[0].bytes(), Some(&[1, 2, 3][..]));

    daemon
        .send_input(
            node.id(),
            &DataId::new("frames").unwrap(),
            node.metadata(),
            vec![7],
        )
        .unwrap();
    let event = events.recv_timeout(WAIT).unwrap().expect("the input");
    assert_eq!(event.payload().map(Payload::to_vec), Some(vec![7]));

    daemon.stop(node.id(), StopCause::Requested).unwrap();
    let event = events.recv_timeout(WAIT).unwrap().expect("the stop");
    assert!(event.is_stop());
}

#[test]
fn two_dynamic_nodes_route_between_themselves() {
    let daemon = MockDaemon::start().unwrap();
    let endpoint = daemon.listen_unix(socket_path("route")).unwrap();

    let (mut producer, _producer_events) = Node::builder()
        .node_id("emitter")
        .unwrap()
        .dataflow(daemon.dataflow())
        .dynamic(true)
        .output("ticks")
        .unwrap()
        .daemon(endpoint.endpoint())
        .orphan_guard(false)
        .connect()
        .unwrap();

    // The consumer's input names the producer's port, which is what the
    // daemon routes on.
    let consumer_spec = astrs_wire::NodeSpawnSpec::new(
        daemon.dataflow(),
        NodeId::new("counter").unwrap(),
        0,
        astrs_wire::NodeSource::Dynamic,
    )
    .with_input(astrs_wire::InputSpec::new(
        DataId::new("ticks").unwrap(),
        PortRef::from_parts("emitter", "ticks").unwrap(),
    ));
    let (_consumer, mut consumer_events) = daemon.connect_node(consumer_spec).unwrap();

    let mut ticks = producer.raw_output("ticks").unwrap();
    ticks.send_bytes(vec![42], producer.metadata()).unwrap();

    let event = consumer_events
        .recv_timeout(WAIT)
        .unwrap()
        .expect("the routed tick");
    let (id, _, data) = event.into_input().unwrap();
    assert_eq!(id.as_str(), "ticks");
    assert_eq!(data.to_vec(), vec![42]);
}

/// The exact scenario `Node::flush_outputs` exists for: a node built with
/// [`NodeBuilder::connect_async`] (so it lives on a *caller-owned*
/// current-thread runtime, exactly what a synchronous CLI verb's own
/// `runtime.block_on(async { ... })` gives every node it attaches — see
/// `astrs-cli`'s `command::topic::publish`) sends one message, calls
/// [`Node::shutdown`], then awaits [`Node::flush_outputs`] rather than
/// simply returning. Two things must both hold: the message must have
/// actually reached the daemon (not merely been queued locally), and this
/// must not idle out to `flush_outputs`'s own bound getting there —
/// `shutdown`'s `request_drain` is what lets the writer task *end*
/// promptly once everything queued ahead of it is flushed, rather than
/// leaving `flush_outputs` nothing to observe but its own timeout.
#[test]
fn flush_outputs_delivers_before_returning_on_a_current_thread_runtime() {
    let daemon = MockDaemon::start().unwrap();
    let endpoint = daemon.listen_unix(socket_path("flush")).unwrap();

    // A bare current-thread runtime, exactly `astrs_cli::command::client::runtime`'s
    // own shape — the one flavor `NodeBuilder::connect` (the sync form)
    // refuses to run on at all (blueprint: `NodeError::BlockingInAsync`),
    // which is why this test reaches the node through `connect_async`.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime");

    let started = std::time::Instant::now();
    runtime.block_on(async {
        let (mut node, _events) = Node::builder()
            .node_id("emitter")
            .unwrap()
            .dataflow(daemon.dataflow())
            .dynamic(true)
            .output("ticks")
            .unwrap()
            .daemon(endpoint.endpoint())
            .orphan_guard(false)
            .connect_async()
            .await
            .expect("a dynamic attach over a real socket, from async");

        let mut ticks = node.raw_output("ticks").unwrap();
        ticks.send_bytes(vec![9], node.metadata()).unwrap();
        let _ = node.shutdown();
        node.flush_outputs().await;
    });
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "shutdown's own drain sentinel must let the writer end long before \
         flush_outputs's 5s bound: took {:?}",
        started.elapsed()
    );

    let sends = daemon
        .wait_for_sends(
            &NodeId::new("emitter").unwrap(),
            &DataId::new("ticks").unwrap(),
            1,
            WAIT,
        )
        .unwrap();
    assert_eq!(sends[0].bytes(), Some(&[9][..]));
}

#[test]
fn a_wrong_token_is_refused_at_the_greeting() {
    let daemon = MockDaemon::start_with(
        astrs_node_api::runtime::NodeRuntime::acquire().unwrap(),
        astrs_wire::DataflowId::from_u128(77),
        astrs_wire::AuthToken::from_bytes([1; 32]),
    )
    .unwrap();
    let endpoint = daemon.listen_unix(socket_path("auth")).unwrap();

    let error = Node::builder()
        .node_id("impostor")
        .unwrap()
        .dynamic(true)
        .daemon(endpoint.endpoint())
        .auth(astrs_wire::AuthToken::from_bytes([2; 32]))
        .orphan_guard(false)
        .connect_timeout(Duration::from_secs(2))
        .connect()
        .unwrap_err();
    assert!(
        matches!(error, NodeError::Handshake(_)),
        "§16: the token is checked before anything else, got {error}"
    );
}

#[test]
fn a_socket_that_is_not_there_is_reported_with_every_attempt() {
    let missing = socket_path("absent");
    let _ = std::fs::remove_file(&missing);
    let error = Node::builder()
        .node_id("probe")
        .unwrap()
        .dynamic(true)
        .daemon(format!("uds://{}", missing.display()))
        .daemon("tcp://127.0.0.1:1")
        .orphan_guard(false)
        .connect_timeout(Duration::from_millis(200))
        .connect()
        .unwrap_err();
    let NodeError::Connect { attempts } = error else {
        panic!("expected a connect error");
    };
    assert_eq!(attempts.len(), 2, "both endpoints were tried: {attempts:?}");
    assert!(attempts[0].0.starts_with("uds://"));
    assert!(attempts[1].0.starts_with("tcp://"));
}

#[test]
fn the_endpoint_unlinks_its_socket_when_dropped() {
    let daemon = MockDaemon::start().unwrap();
    let path = socket_path("unlink");
    let endpoint = daemon.listen_unix(&path).unwrap();
    assert!(path.exists());
    drop(endpoint);
    assert!(!path.exists(), "a dropped endpoint leaves no stale socket");
}
