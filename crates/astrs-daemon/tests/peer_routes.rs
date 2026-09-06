//! Two daemons, one loopback TCP connection, one cross-host route (§6.4).
//!
//! Everything below runs two complete [`astrs_daemon::Daemon`] instances in one
//! process, each with its own identity, its own graph state and its own peer
//! table, talking over a real socket with the real framing. The only thing that
//! is not real is the machine boundary — and since the daemon has no idea which
//! side of one it is on, that changes nothing it does.
//!
//! ```text
//!   daemon B (producer host)                daemon A (consumer host)
//!   ────────────────────────                ────────────────────────
//!   fake node "camera" ──SendMessage──►  B
//!                                        B ──RouteSetup──────────► A
//!                                        B ◄─RouteAccept────────── A
//!                                        B ──Output(seq)────────► A
//!                                                                 A ──Input──► fake node "detect"
//! ```
//!
//! # Why both loops are pumped by hand
//!
//! [`astrs_daemon::Daemon::run`] returns when its dataflows finish; a daemon
//! acting as a peer has no reason to finish. [`astrs_daemon::Daemon::pump`]
//! runs the same loop for a bounded slice, so the two daemons and the scripted
//! nodes can be interleaved deterministically from one task instead of racing
//! in three.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::time::Duration;

use astrs_daemon::server::NodeListeners;
use astrs_daemon::{Daemon, DaemonConfig, ListenConfig, PeerConfig, RuntimePaths, plan_dataflow};
use astrs_manifest::Manifest;
use astrs_transport::TransportAddr;
use astrs_wire::io::{AsyncFrameReader, AsyncFrameWriter};
use astrs_wire::{
    AuthToken, DaemonId, DataId, DataflowId, FrameLimits, Metadata, NodeEvent, NodeHandshake,
    NodeId, NodeRequest, OutputPayload, PortRef, SessionId,
};
use tokio::io::DuplexStream;

/// The dataflow both daemons host part of.
fn dataflow() -> DataflowId {
    DataflowId::from_u128(0xC0FFEE)
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}

fn data(name: &str) -> DataId {
    DataId::new(name).unwrap()
}

fn port(text: &str) -> PortRef {
    text.parse().unwrap()
}

/// Both nodes are declared on both daemons; which one actually registers is
/// what makes a daemon the producer's host or the consumer's.
const PIPELINE: &str = "\
nodes:
  - id: camera
    path: dynamic
    outputs: [image]
  - id: detect
    path: dynamic
    inputs:
      frames:
        source: camera/image
        queue_size: 8
";

/// The cluster token both daemons authenticate with (§16).
fn token() -> AuthToken {
    AuthToken::from_bytes([0x5A; 32])
}

/// A daemon with a private runtime directory, no node listeners, no
/// shared-memory plane, and the given peer configuration.
fn daemon(name: &str, peer: PeerConfig) -> Daemon {
    let root = std::env::temp_dir().join(format!("astrs-peer-{}-{name}", std::process::id()));
    let _ = std::fs::create_dir_all(&root);
    let config = DaemonConfig::new(RuntimePaths::under(root))
        .with_listen(ListenConfig::none())
        .with_shm(false)
        .with_peer(peer);
    let mut daemon = Daemon::new(config).expect("a daemon");
    let manifest = Manifest::from_yaml_str(PIPELINE).expect("a manifest");
    let plan = plan_dataflow(dataflow(), &manifest, &BTreeMap::new()).expect("a plan");
    daemon.admit(&plan).expect("admitted");
    daemon
}

/// One scripted node's end of an in-process connection.
struct FakeNode {
    writer: AsyncFrameWriter<tokio::io::WriteHalf<DuplexStream>>,
    reader: AsyncFrameReader<tokio::io::ReadHalf<DuplexStream>>,
    #[allow(dead_code)]
    session: SessionId,
}

impl FakeNode {
    fn attach(daemon: &mut Daemon) -> Self {
        let session = daemon.sessions().mint();
        let (accepted, node_side) = NodeListeners::in_process(session);
        daemon.attach(accepted);
        let (reader, writer) = tokio::io::split(node_side);
        Self {
            writer: AsyncFrameWriter::new(writer, FrameLimits::uds()),
            reader: AsyncFrameReader::new(reader, FrameLimits::uds()),
            session,
        }
    }

    async fn send(&mut self, request: NodeRequest) {
        self.writer.send(&request).await.expect("a writable frame");
    }

    async fn recv(&mut self) -> Option<NodeEvent> {
        tokio::time::timeout(
            Duration::from_millis(500),
            self.reader.read_message::<NodeEvent>(),
        )
        .await
        .ok()?
        .ok()?
    }

    async fn register(&mut self, name: &str) {
        self.send(NodeRequest::Register(NodeHandshake::dynamic(
            dataflow(),
            node(name),
        )))
        .await;
    }
}

/// Runs both loops for a slice, so a message can cross the socket.
async fn pump_both(first: &mut Daemon, second: &mut Daemon) {
    let slice = Duration::from_millis(120);
    tokio::join!(first.pump(slice), second.pump(slice));
}

/// Connects two daemons over loopback TCP and returns them, producer first.
///
/// `producer` listens nowhere and dials; `consumer` binds port 0 and accepts.
/// Which side dials is deliberately the *producer* here, mirroring §6.4's
/// "the daemon that owns the producer" minting the route handle.
async fn connected_pair() -> (Daemon, Daemon, DaemonId) {
    let mut consumer = daemon("consumer", PeerConfig::new(token()).with_loopback(0));
    let addr = consumer
        .peers_mut()
        .bind()
        .await
        .expect("a free loopback port");
    assert!(consumer.peers().spawn_accept_loop(consumer.handle()));
    let consumer_id = consumer.config().id().clone();

    let mut producer = daemon("producer", PeerConfig::new(token()));
    let handle = producer.handle();
    producer
        .peers_mut()
        .connect_peer(consumer_id.clone(), &TransportAddr::Tcp(addr), handle)
        .await
        .expect("the dial succeeds");

    // The accept side learns about the connection through its event loop.
    pump_both(&mut producer, &mut consumer).await;
    (producer, consumer, consumer_id)
}

#[tokio::test]
async fn two_daemons_connect_in_both_directions() {
    let (producer, consumer, consumer_id) = connected_pair().await;

    assert!(producer.peers().is_connected(&consumer_id));
    assert_eq!(producer.peers().len(), 1);
    assert_eq!(
        consumer.peers().len(),
        1,
        "the acceptor learned the dialler's identity from its greeting"
    );
    assert!(
        consumer.peers().is_connected(producer.config().id()),
        "keyed by daemon id, not by address"
    );
}

#[tokio::test]
async fn a_route_setup_is_answered_and_established() {
    let (mut producer, mut consumer, consumer_id) = connected_pair().await;

    let route = producer
        .open_remote_route(
            &consumer_id,
            dataflow(),
            port("camera/image"),
            port("detect/frames"),
        )
        .expect("a route handle");
    pump_both(&mut producer, &mut consumer).await;

    let state = producer
        .peers()
        .routes()
        .state(&consumer_id, route)
        .expect("the route is known");
    assert!(state.is_established(), "{state:?}");
    assert_eq!(consumer.peers().routes().inbound_count(), 1);
    assert_eq!(consumer.peers().routes().established_count(), 1);
}

#[tokio::test]
async fn a_route_to_a_port_the_peer_does_not_host_is_refused() {
    let (mut producer, mut consumer, consumer_id) = connected_pair().await;

    let route = producer
        .open_remote_route(
            &consumer_id,
            dataflow(),
            port("camera/image"),
            port("detect/nothing"),
        )
        .expect("a route handle");
    // Before the answer comes back it is an ordinary pending route, which is
    // what makes the assertion after the pump about the *refusal* and not
    // about the route never having been opened.
    assert!(
        producer
            .peers()
            .routes()
            .state(&consumer_id, route)
            .is_some_and(astrs_daemon::peer::RemoteRouteState::is_pending)
    );
    pump_both(&mut producer, &mut consumer).await;

    // `UnknownPort` is a *transient* rejection (`RouteRejection::is_transient`):
    // the consumer's spawn may simply not have reached its daemon yet. The
    // producer therefore forgets the route rather than leaving a `rejected`
    // tombstone that `has_outbound` would report as "already open" for ever,
    // so the reconciliation backstop re-opens it when the port appears. This
    // test asserted the tombstone before that was fixed; a cluster whose two
    // `Spawn`s raced its `PeerRoutes` lost the edge for the whole run.
    assert!(
        producer
            .peers()
            .routes()
            .state(&consumer_id, route)
            .is_none(),
        "a transient refusal must leave the edge retryable"
    );
    assert_eq!(producer.peers().routes().outbound_count(), 0);
    assert_eq!(consumer.peers().routes().inbound_count(), 0);
}

#[tokio::test]
async fn a_payload_crosses_the_socket_and_reaches_the_consumer() {
    let (mut producer, mut consumer, consumer_id) = connected_pair().await;

    // The consumer's daemon hosts `detect`; the producer's hosts `camera`.
    let mut detect = FakeNode::attach(&mut consumer);
    let mut camera = FakeNode::attach(&mut producer);
    detect.register("detect").await;
    camera.register("camera").await;
    pump_both(&mut producer, &mut consumer).await;
    let _ = detect.recv().await;
    let _ = camera.recv().await;

    detect
        .send(NodeRequest::Subscribe {
            inputs: vec![data("frames")],
        })
        .await;
    producer
        .open_remote_route(
            &consumer_id,
            dataflow(),
            port("camera/image"),
            port("detect/frames"),
        )
        .expect("a route handle");
    pump_both(&mut producer, &mut consumer).await;

    camera
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(b"a remote frame".to_vec()),
        })
        .await;
    detect
        .send(NodeRequest::NextEvent {
            timeout: None,
            max_batch: 8,
        })
        .await;
    pump_both(&mut producer, &mut consumer).await;
    pump_both(&mut producer, &mut consumer).await;

    let mut received = None;
    while let Some(event) = detect.recv().await {
        if let NodeEvent::Input { id, payload, .. } = event {
            received = Some((id, payload));
            break;
        }
    }
    let (id, payload) = received.expect("the remote message was delivered");
    assert_eq!(id, data("frames"));
    assert_eq!(payload, b"a remote frame");

    assert!(producer.peers().bytes_sent() >= 14);
    assert!(consumer.peers().bytes_received() >= 14);
}

#[tokio::test]
async fn sequence_numbers_advance_across_the_route() {
    let (mut producer, mut consumer, consumer_id) = connected_pair().await;
    let mut camera = FakeNode::attach(&mut producer);
    camera.register("camera").await;
    pump_both(&mut producer, &mut consumer).await;

    let route = producer
        .open_remote_route(
            &consumer_id,
            dataflow(),
            port("camera/image"),
            port("detect/frames"),
        )
        .expect("a route handle");
    pump_both(&mut producer, &mut consumer).await;

    for index in 0..3u8 {
        camera
            .send(NodeRequest::SendMessage {
                output: data("image"),
                metadata: Metadata::default(),
                payload: OutputPayload::inline(vec![index; 4]),
            })
            .await;
    }
    pump_both(&mut producer, &mut consumer).await;

    let sent = producer
        .peers()
        .routes()
        .outbound(&consumer_id, route)
        .expect("the route is known");
    assert_eq!(sent.seq, 3, "one sequence number per message");
    assert_eq!(sent.messages, 3);
}

#[tokio::test]
async fn closing_the_producer_output_closes_the_remote_input() {
    let (mut producer, mut consumer, consumer_id) = connected_pair().await;
    let mut detect = FakeNode::attach(&mut consumer);
    let mut camera = FakeNode::attach(&mut producer);
    detect.register("detect").await;
    camera.register("camera").await;
    pump_both(&mut producer, &mut consumer).await;
    let _ = detect.recv().await;
    let _ = camera.recv().await;

    detect
        .send(NodeRequest::Subscribe {
            inputs: vec![data("frames")],
        })
        .await;
    producer
        .open_remote_route(
            &consumer_id,
            dataflow(),
            port("camera/image"),
            port("detect/frames"),
        )
        .expect("a route handle");
    pump_both(&mut producer, &mut consumer).await;

    camera
        .send(NodeRequest::CloseOutputs {
            outputs: vec![data("image")],
        })
        .await;
    detect
        .send(NodeRequest::NextEvent {
            timeout: None,
            max_batch: 8,
        })
        .await;
    pump_both(&mut producer, &mut consumer).await;
    pump_both(&mut producer, &mut consumer).await;

    let mut closed = false;
    while let Some(event) = detect.recv().await {
        if let NodeEvent::InputClosed { id, .. } = event
            && id == data("frames")
        {
            closed = true;
            break;
        }
    }
    assert!(closed, "the remote consumer was told its input ended");
}

#[tokio::test]
async fn a_lost_peer_closes_every_input_it_carried() {
    let (mut producer, mut consumer, consumer_id) = connected_pair().await;
    let mut detect = FakeNode::attach(&mut consumer);
    detect.register("detect").await;
    pump_both(&mut producer, &mut consumer).await;
    let _ = detect.recv().await;

    detect
        .send(NodeRequest::Subscribe {
            inputs: vec![data("frames")],
        })
        .await;
    producer
        .open_remote_route(
            &consumer_id,
            dataflow(),
            port("camera/image"),
            port("detect/frames"),
        )
        .expect("a route handle");
    pump_both(&mut producer, &mut consumer).await;
    assert_eq!(consumer.peers().routes().established_count(), 1);

    // The producer's daemon goes away: the socket closes, and the consumer's
    // loop learns about it through its own pump (§12 peer partition).
    let producer_id = producer.config().id().clone();
    drop(producer);
    for _ in 0..8 {
        consumer.pump(Duration::from_millis(60)).await;
        if !consumer.peers().is_connected(&producer_id) {
            break;
        }
    }

    detect
        .send(NodeRequest::NextEvent {
            timeout: None,
            max_batch: 8,
        })
        .await;
    consumer.pump(Duration::from_millis(120)).await;

    assert!(
        consumer.peers().routes().is_empty(),
        "the routes went with it"
    );
    let mut closed = false;
    while let Some(event) = detect.recv().await {
        if let NodeEvent::InputClosed { id, reason, .. } = event
            && id == data("frames")
        {
            assert!(!reason.is_expected(), "a partition is not a clean close");
            closed = true;
            break;
        }
    }
    assert!(closed, "the consumer saw its remote input close");
}

#[tokio::test]
async fn a_route_for_a_dataflow_the_peer_does_not_host_is_refused() {
    let (mut producer, mut consumer, consumer_id) = connected_pair().await;

    // A dataflow only the producer knows about.
    let manifest = Manifest::from_yaml_str(PIPELINE).expect("a manifest");
    let other = DataflowId::from_u128(0xBEEF);
    let plan = plan_dataflow(other, &manifest, &BTreeMap::new()).expect("a plan");
    producer.admit(&plan).expect("admitted");

    let route = producer
        .open_remote_route(
            &consumer_id,
            other,
            port("camera/image"),
            port("detect/frames"),
        )
        .expect("a route handle");
    pump_both(&mut producer, &mut consumer).await;

    // As `a_route_to_a_port_the_peer_does_not_host_is_refused`:
    // `UnknownDataflow` is transient (the peer may be told about the dataflow
    // a moment later), so the refusal is recorded by *forgetting* the route,
    // leaving the edge retryable rather than permanently closed.
    assert!(
        producer
            .peers()
            .routes()
            .state(&consumer_id, route)
            .is_none(),
        "a transient refusal must leave the edge retryable"
    );
    assert_eq!(consumer.peers().routes().inbound_count(), 0);
}

#[tokio::test]
async fn peer_metrics_count_what_crossed() {
    let (mut producer, mut consumer, consumer_id) = connected_pair().await;
    let mut camera = FakeNode::attach(&mut producer);
    camera.register("camera").await;
    pump_both(&mut producer, &mut consumer).await;

    producer
        .open_remote_route(
            &consumer_id,
            dataflow(),
            port("camera/image"),
            port("detect/frames"),
        )
        .expect("a route handle");
    pump_both(&mut producer, &mut consumer).await;

    camera
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![7; 32]),
        })
        .await;
    pump_both(&mut producer, &mut consumer).await;

    let batch = producer
        .metrics()
        .snapshot(astrs_time::HlcTimestamp::new(1, 0));
    let sent = batch
        .points
        .iter()
        .find(|point| point.name == astrs_daemon::metrics::names::PEER_BYTES_SENT_TOTAL)
        .expect("the peer byte counter is registered");
    assert!(sent.value.as_f64() >= 32.0, "{}", sent.value.as_f64());

    let remote = batch
        .points
        .iter()
        .find(|point| {
            point.name == astrs_daemon::metrics::names::ROUTES
                && point.label("plane") == Some("remote")
        })
        .expect("the remote routes gauge is registered");
    assert_eq!(remote.value.as_f64(), 1.0);
}

#[tokio::test]
async fn a_reconnected_peer_recovers_the_input_it_lost() {
    // The consumer daemon keeps running while the producer's link comes and
    // goes: its consumer must see `InputClosed` then `InputRecovered` (§12).
    let mut consumer = daemon(
        "recover-consumer",
        PeerConfig::new(token()).with_loopback(0),
    );
    let addr = consumer.peers_mut().bind().await.expect("a free port");
    assert!(consumer.peers().spawn_accept_loop(consumer.handle()));
    let consumer_id = consumer.config().id().clone();

    let mut detect = FakeNode::attach(&mut consumer);
    detect.register("detect").await;
    consumer.pump(Duration::from_millis(80)).await;
    let _ = detect.recv().await;
    detect
        .send(NodeRequest::Subscribe {
            inputs: vec![data("frames")],
        })
        .await;
    consumer.pump(Duration::from_millis(80)).await;

    // First incarnation of the producer daemon.
    {
        let mut producer = daemon("recover-producer-1", PeerConfig::new(token()));
        let handle = producer.handle();
        producer
            .peers_mut()
            .connect_peer(consumer_id.clone(), &TransportAddr::Tcp(addr), handle)
            .await
            .expect("the dial succeeds");
        pump_both(&mut producer, &mut consumer).await;
        producer
            .open_remote_route(
                &consumer_id,
                dataflow(),
                port("camera/image"),
                port("detect/frames"),
            )
            .expect("a route handle");
        pump_both(&mut producer, &mut consumer).await;
        assert_eq!(consumer.peers().routes().established_count(), 1);
    }

    // It goes away: the input closes.
    for _ in 0..8 {
        consumer.pump(Duration::from_millis(60)).await;
        if consumer.peers().is_empty() {
            break;
        }
    }
    assert!(consumer.peers().routes().is_empty());

    // A second incarnation dials in and re-opens the same edge.
    let mut producer = daemon("recover-producer-2", PeerConfig::new(token()));
    let handle = producer.handle();
    producer
        .peers_mut()
        .connect_peer(consumer_id.clone(), &TransportAddr::Tcp(addr), handle)
        .await
        .expect("the dial succeeds");
    pump_both(&mut producer, &mut consumer).await;
    producer
        .open_remote_route(
            &consumer_id,
            dataflow(),
            port("camera/image"),
            port("detect/frames"),
        )
        .expect("a route handle");
    pump_both(&mut producer, &mut consumer).await;

    detect
        .send(NodeRequest::NextEvent {
            timeout: None,
            max_batch: 32,
        })
        .await;
    consumer.pump(Duration::from_millis(120)).await;

    let mut saw_closed = false;
    let mut saw_recovered = false;
    while let Some(event) = detect.recv().await {
        match event {
            NodeEvent::InputClosed { id, .. } if id == data("frames") => saw_closed = true,
            NodeEvent::InputRecovered { id, .. } if id == data("frames") => {
                saw_recovered = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_closed, "the partition closed the input");
    assert!(saw_recovered, "the repair reopened it");
}

#[tokio::test]
async fn a_clean_shutdown_tears_routes_down_with_a_goodbye() {
    let (mut producer, mut consumer, consumer_id) = connected_pair().await;
    let mut detect = FakeNode::attach(&mut consumer);
    detect.register("detect").await;
    pump_both(&mut producer, &mut consumer).await;
    let _ = detect.recv().await;
    detect
        .send(NodeRequest::Subscribe {
            inputs: vec![data("frames")],
        })
        .await;
    producer
        .open_remote_route(
            &consumer_id,
            dataflow(),
            port("camera/image"),
            port("detect/frames"),
        )
        .expect("a route handle");
    pump_both(&mut producer, &mut consumer).await;
    assert_eq!(consumer.peers().routes().established_count(), 1);

    // A deliberate shutdown, not a partition: the peer must hear about it.
    producer.begin_shutdown();
    assert!(producer.peers().routes().is_empty());
    detect
        .send(NodeRequest::NextEvent {
            timeout: None,
            max_batch: 8,
        })
        .await;
    pump_both(&mut producer, &mut consumer).await;

    let mut reason = None;
    while let Some(event) = detect.recv().await {
        if let NodeEvent::InputClosed {
            id, reason: why, ..
        } = event
            && id == data("frames")
        {
            reason = Some(why);
            break;
        }
    }
    let reason = reason.expect("the consumer was told its input closed");
    assert!(
        reason.is_expected(),
        "a clean shutdown is not a fault: {reason:?}"
    );
    assert_eq!(reason.kind_name(), "daemon_shutdown");
}
