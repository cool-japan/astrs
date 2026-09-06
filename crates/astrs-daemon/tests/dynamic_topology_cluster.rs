//! Dynamic topology end to end, through a *real* coordinator (blueprint
//! §8, §17): `astrs node add/remove/replace/connect/disconnect` exactly as
//! a real `astrs` CLI process would drive them — one TCP `ControlRequest`
//! at a time — landing on a real [`Daemon`] over its real coordinator
//! uplink, never `Daemon::apply_add_node`/`apply_replace_node`/... called
//! directly.
//!
//! [`crates/astrs-daemon/tests/dynamic_topology.rs`] already covers the
//! daemon-local half of every one of these (add/remove/replace/edge ops,
//! called straight on a bare [`Daemon`], no coordinator in the loop at
//! all) in much finer-grained detail — dual-run message continuity,
//! live rewiring, `InputClosed` on removal. What *that* file cannot
//! prove is the other half of the task brief's own loop: "`astrs node
//! …` → coordinator validates via graph diff → dispatches to owning
//! daemons → daemon applies". This file is that other half, using the
//! same one-coordinator-plus-real-daemons harness
//! `tests/cluster_m2.rs` established for the M2 milestone (trimmed to one
//! daemon — every op here is scoped to nodes on a single machine).
//!
//! ```text
//!   CliActor ──ControlRequest::AddNode/RemoveNode/ReplaceNode/──►┐
//!             ◄───────────────────────────ControlReply::Ok───────┤
//!                                                       CoordinatorServer
//!                                                                 │
//!                                                     CoordinatorEvent
//!                                                                 ▼
//!                                                              Daemon
//!                                                          (real uplink,
//!                                                           real TCP)
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use astrs_coordinator::{Coordinator, CoordinatorConfig, CoordinatorServer};
use astrs_daemon::coordinator::UplinkConfig;
use astrs_daemon::server::NodeListeners;
use astrs_daemon::{Daemon, DaemonConfig, ListenConfig, PeerConfig, RuntimePaths};
use astrs_transport::{ConnectionCounters, FramedStream, HandshakeParams, LocalIdentity, initiate};
use astrs_wire::io::{AsyncFrameReader, AsyncFrameWriter};
use astrs_wire::{
    AuthToken, ControlReply, ControlRequest, DataId, DataflowId, DataflowSource, FeatureFlags,
    FrameKind, FrameLimits, InputSpec, Metadata, NodeEvent, NodeHandshake, NodeId, NodeRequest,
    NodeSource, NodeSpawnSpec, OutputPayload, PortRef, Role,
};
use tokio::io::DuplexStream;
use tokio::net::TcpStream;

/// How long any single wait may take before the test calls the cluster
/// stuck.
const DEADLINE: Duration = Duration::from_secs(20);

/// One slice of the daemon's event loop.
const SLICE: Duration = Duration::from_millis(20);

fn token() -> AuthToken {
    AuthToken::from_bytes([0x7A; 32])
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).expect("a legal node id")
}

fn data(name: &str) -> DataId {
    DataId::new(name).expect("a legal port id")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("as-dtc-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

// ---------------------------------------------------------------------
// The coordinator
// ---------------------------------------------------------------------

struct Cluster {
    addr: SocketAddr,
    coordinator: Coordinator,
    handle: astrs_coordinator::ServerHandle,
    task: tokio::task::JoinHandle<astrs_coordinator::Result<()>>,
}

impl Cluster {
    async fn start() -> Self {
        let config = CoordinatorConfig::new(token())
            .with_port(0)
            .with_heartbeat(Duration::from_millis(500), 240);
        let coordinator = Coordinator::open_in_memory(config).expect("an in-memory store");
        let server = CoordinatorServer::bind(coordinator.clone())
            .await
            .expect("bind");
        let addr = server.local_addr().expect("a bound address");
        let handle = server.handle();
        let task = tokio::spawn(server.serve());
        Self {
            addr,
            coordinator,
            handle,
            task,
        }
    }

    async fn shutdown(self) {
        self.handle.shutdown();
        let _ = tokio::time::timeout(DEADLINE, self.task).await;
    }
}

/// The CLI's end of one coordinator connection — real TCP, real framing,
/// exactly what `astrs node …` itself dials (`command::client::Client`,
/// one crate up).
struct CliActor {
    stream: FramedStream<TcpStream>,
}

impl CliActor {
    async fn connect(addr: SocketAddr) -> Self {
        let raw = TcpStream::connect(addr).await.expect("tcp connect");
        let mut stream =
            FramedStream::new(raw, FrameLimits::network(), ConnectionCounters::shared());
        let params = HandshakeParams::new(LocalIdentity::new(Role::Cli), token())
            .with_features(FeatureFlags::EMPTY);
        initiate(&mut stream, &params, DEADLINE)
            .await
            .expect("the cli handshake");
        Self { stream }
    }

    async fn request(&mut self, request: ControlRequest) -> ControlReply {
        self.stream
            .send_message(&request)
            .await
            .expect("a writable request");
        self.stream
            .expect_message(FrameKind::ControlReply)
            .await
            .expect("a readable reply")
    }

    /// As [`Self::request`], failing the test immediately if the
    /// coordinator refused — every mutating call in this file is expected
    /// to succeed, so a refusal is a test bug, not a case to branch on.
    async fn request_ok(&mut self, request: ControlRequest) -> ControlReply {
        let reply = self.request(request).await;
        assert!(
            !matches!(reply, ControlReply::Error { .. }),
            "unexpected refusal: {reply:?}"
        );
        reply
    }
}

// ---------------------------------------------------------------------
// The daemon
// ---------------------------------------------------------------------

async fn cluster_daemon(name: &str, coordinator: SocketAddr) -> Daemon {
    let config = DaemonConfig::new(RuntimePaths::under(scratch(name)))
        .with_listen(ListenConfig::none())
        .with_shm(false)
        .with_auth(token())
        .with_heartbeat_interval(Duration::from_millis(100))
        .with_metrics_interval(Duration::from_millis(100))
        .with_peer(PeerConfig::new(token()).with_loopback(0));
    let mut daemon = Daemon::new(config).expect("a daemon");
    let uplink = UplinkConfig::new(coordinator, token())
        .with_backoff(Duration::from_millis(20), Duration::from_millis(100))
        .with_dial_timeout(Duration::from_millis(500));
    daemon
        .connect_coordinator(uplink)
        .await
        .expect("the uplink starts");
    daemon
}

/// One scripted node's end of an in-process connection to the daemon —
/// standing in for a real `path: dynamic` process's `Node::init_from_node_id`
/// (§8.3); the wire protocol on this duplex is identical to a real UDS
/// socket's.
struct FakeNode {
    writer: AsyncFrameWriter<tokio::io::WriteHalf<DuplexStream>>,
    reader: AsyncFrameReader<tokio::io::ReadHalf<DuplexStream>>,
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
        }
    }

    async fn send(&mut self, request: NodeRequest) {
        self.writer.send(&request).await.expect("a writable frame");
    }

    async fn register(&mut self, dataflow: DataflowId, name: &str) {
        self.send(NodeRequest::Register(NodeHandshake::dynamic(
            dataflow,
            node(name),
        )))
        .await;
    }

    async fn try_recv(&mut self) -> Option<NodeEvent> {
        tokio::time::timeout(SLICE, self.reader.read_message::<NodeEvent>())
            .await
            .ok()?
            .ok()?
    }

    /// Reads events, pumping `daemon` between attempts, until one matching
    /// `pred` arrives or the deadline passes.
    async fn recv_matching(
        &mut self,
        daemon: &mut Daemon,
        mut pred: impl FnMut(&NodeEvent) -> bool,
    ) -> Option<NodeEvent> {
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline {
            if let Some(event) = self.try_recv().await {
                if pred(&event) {
                    return Some(event);
                }
                continue;
            }
            daemon.pump(SLICE).await;
        }
        None
    }
}

/// Pumps `daemon` until `ready` holds, or the deadline passes; returns
/// whether it did.
async fn pump_until(daemon: &mut Daemon, mut ready: impl FnMut(&Daemon) -> bool) -> bool {
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        if ready(daemon) {
            return true;
        }
        daemon.pump(SLICE).await;
    }
    ready(daemon)
}

/// Waits until `daemon` has registered with the coordinator.
async fn await_registration(cluster: &Cluster, daemon: &mut Daemon) {
    let registered = pump_until(daemon, |_| !cluster.coordinator.daemons().is_empty()).await;
    assert!(registered, "the daemon must register with the coordinator");
}

/// A manifest with two dynamic nodes: `camera` (produces `image`) and
/// `detect` (consumes `camera/image` as `frames`) — every scenario below
/// starts from this, then edits it live.
const TWO_NODE_PIPELINE: &str = "\
nodes:
  - id: camera
    path: dynamic
    outputs: [image]
  - id: detect
    path: dynamic
    inputs:
      frames: camera/image
";

/// As [`TWO_NODE_PIPELINE`], with a third node `sink` downstream of
/// `detect` — for the removal scenario, which needs a node that is
/// simultaneously a consumer (of `camera`) and a producer (for `sink`).
const THREE_NODE_PIPELINE: &str = "\
nodes:
  - id: camera
    path: dynamic
    outputs: [image]
  - id: detect
    path: dynamic
    outputs: [boxes]
    inputs:
      frames: camera/image
  - id: sink
    path: dynamic
    inputs:
      boxes: detect/boxes
";

async fn start_pipeline(cli: &mut CliActor, yaml: &str) -> DataflowId {
    match cli
        .request(ControlRequest::Start {
            source: DataflowSource::Manifest {
                yaml: yaml.to_owned(),
                working_dir: None,
            },
            name: None,
            detach: true,
        })
        .await
    {
        ControlReply::Started { dataflow, .. } => dataflow,
        other => panic!("start refused: {other:?}"),
    }
}

// ---------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_node_through_the_coordinator_reaches_the_daemon_and_the_new_consumer_receives() {
    let cluster = Cluster::start().await;
    let mut daemon = cluster_daemon("add", cluster.addr).await;
    let mut cli = CliActor::connect(cluster.addr).await;
    await_registration(&cluster, &mut daemon).await;

    let dataflow = start_pipeline(
        &mut cli,
        "nodes:\n  - id: camera\n    path: dynamic\n    outputs: [image]\n",
    )
    .await;
    assert!(pump_until(&mut daemon, |d| d.dataflow(dataflow).is_some()).await);
    let mut camera = FakeNode::attach(&mut daemon);
    camera.register(dataflow, "camera").await;
    assert!(
        camera
            .recv_matching(&mut daemon, |e| matches!(e, NodeEvent::Registered { .. }))
            .await
            .is_some()
    );

    // Nobody is listening yet: publishing must not error.
    camera
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![1]),
        })
        .await;
    daemon.pump(SLICE).await;

    // `astrs node add` — a brand-new node, never in the manifest, added to
    // the *running* dataflow entirely through the coordinator.
    let spec = NodeSpawnSpec::new(dataflow, node("detect"), 0, NodeSource::Dynamic).with_input(
        InputSpec::new(
            data("frames"),
            PortRef::from_parts("camera", "image").unwrap(),
        ),
    );
    cli.request_ok(ControlRequest::AddNode {
        dataflow,
        node: Box::new(spec),
        start: true,
    })
    .await;
    assert!(
        pump_until(&mut daemon, |d| d
            .dataflow(dataflow)
            .and_then(|s| s.node(&node("detect")))
            .is_some())
        .await,
        "the daemon admitted the dynamically added node"
    );
    let mut detect = FakeNode::attach(&mut daemon);
    detect.register(dataflow, "detect").await;
    assert!(
        detect
            .recv_matching(&mut daemon, |e| matches!(e, NodeEvent::Registered { .. }))
            .await
            .is_some()
    );

    camera
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![2]),
        })
        .await;
    let event = detect
        .recv_matching(&mut daemon, |e| matches!(e, NodeEvent::Input { .. }))
        .await
        .expect("the newly added consumer receives what was published after it joined");
    match event {
        NodeEvent::Input { id, payload, .. } => {
            assert_eq!(id, data("frames"));
            assert_eq!(payload, vec![2], "not the message sent before it existed");
        }
        other => panic!("unexpected {other:?}"),
    }

    // `astrs list`/`astrs info`, backed by the durable store, see the
    // addition too — not only the daemon's own live state.
    match cli
        .request(ControlRequest::Info {
            dataflow,
            include_nodes: true,
        })
        .await
    {
        ControlReply::DataflowList { nodes, .. } => {
            assert!(
                nodes.iter().any(|n| n.node.as_str() == "detect"),
                "Info reflects the dynamically added node: {nodes:?}"
            );
        }
        other => panic!("unexpected {other:?}"),
    }

    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_node_through_the_coordinator_stops_it_while_its_upstream_producer_keeps_running() {
    let cluster = Cluster::start().await;
    let mut daemon = cluster_daemon("remove", cluster.addr).await;
    let mut cli = CliActor::connect(cluster.addr).await;
    await_registration(&cluster, &mut daemon).await;

    let dataflow = start_pipeline(&mut cli, THREE_NODE_PIPELINE).await;
    assert!(
        pump_until(&mut daemon, |d| d
            .dataflow(dataflow)
            .is_some_and(|s| s.node(&node("sink")).is_some()))
        .await
    );
    let mut camera = FakeNode::attach(&mut daemon);
    camera.register(dataflow, "camera").await;
    let mut detect = FakeNode::attach(&mut daemon);
    detect.register(dataflow, "detect").await;
    let mut sink = FakeNode::attach(&mut daemon);
    sink.register(dataflow, "sink").await;
    for fake in [&mut camera, &mut detect, &mut sink] {
        assert!(
            fake.recv_matching(&mut daemon, |e| matches!(e, NodeEvent::Registered { .. }))
                .await
                .is_some()
        );
    }

    // `astrs node remove detect` — the middle node, which is both a
    // consumer (of `camera`) and a producer (for `sink`).
    cli.request_ok(ControlRequest::RemoveNode {
        dataflow,
        node: node("detect"),
        grace: None,
    })
    .await;

    let stop = detect
        .recv_matching(&mut daemon, |e| matches!(e, NodeEvent::Stop { .. }))
        .await;
    assert!(stop.is_some(), "detect is asked to stop");
    drop(detect);
    // A real process's exit is what actually triggers `close_outputs_of`;
    // dropping this fake connection is that same signal for a `path:
    // dynamic` node.
    assert!(
        pump_until(&mut daemon, |d| d
            .dataflow(dataflow)
            .and_then(|s| s.node(&node("detect")))
            .is_some_and(|n| !n.is_live()))
        .await,
        "detect's incarnation actually ended"
    );

    let closed = sink
        .recv_matching(&mut daemon, |e| matches!(e, NodeEvent::InputClosed { .. }))
        .await
        .expect("sink is told its input from detect closed");
    match closed {
        NodeEvent::InputClosed { id, .. } => assert_eq!(id, data("boxes")),
        other => panic!("unexpected {other:?}"),
    }

    // The upstream producer was never touched.
    assert!(
        daemon
            .dataflow(dataflow)
            .unwrap()
            .node(&node("camera"))
            .unwrap()
            .is_live(),
        "removing a downstream node must not touch its upstream producer"
    );

    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replace_node_through_the_coordinator_cuts_over_with_no_message_loss() {
    let cluster = Cluster::start().await;
    let mut daemon = cluster_daemon("replace", cluster.addr).await;
    let mut cli = CliActor::connect(cluster.addr).await;
    await_registration(&cluster, &mut daemon).await;

    let dataflow = start_pipeline(&mut cli, TWO_NODE_PIPELINE).await;
    assert!(
        pump_until(&mut daemon, |d| d
            .dataflow(dataflow)
            .is_some_and(|s| s.node(&node("detect")).is_some()))
        .await
    );
    let mut camera_v1 = FakeNode::attach(&mut daemon);
    camera_v1.register(dataflow, "camera").await;
    let mut detect = FakeNode::attach(&mut daemon);
    detect.register(dataflow, "detect").await;
    for fake in [&mut camera_v1, &mut detect] {
        assert!(
            fake.recv_matching(&mut daemon, |e| matches!(e, NodeEvent::Registered { .. }))
                .await
                .is_some()
        );
    }

    camera_v1
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![1]),
        })
        .await;
    daemon.pump(SLICE).await;
    let generation_before_replace = daemon
        .dataflow(dataflow)
        .and_then(|s| s.node(&node("camera")))
        .map(astrs_daemon::state::NodeState::generation)
        .expect("camera is tracked");

    // `astrs node replace camera` — a fresh generation is admitted and
    // spawned *before* the outgoing incarnation is asked to leave.
    let replacement = NodeSpawnSpec::new(dataflow, node("camera"), 0, NodeSource::Dynamic)
        .with_output(astrs_wire::OutputSpec::new(data("image")));
    cli.request_ok(ControlRequest::ReplaceNode {
        dataflow,
        node: Box::new(replacement),
        drain: false,
    })
    .await;
    assert!(
        pump_until(&mut daemon, |d| d
            .dataflow(dataflow)
            .and_then(|s| s.node(&node("camera")))
            .is_some_and(|n| n.generation() == generation_before_replace + 1))
        .await,
        "the coordinator's ReplaceNode reached the daemon and advanced the generation \
         past whatever it already was — never collided with it"
    );

    // The dual-run window: the *old* incarnation's connection is still
    // open and can still publish.
    camera_v1
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![2]),
        })
        .await;
    daemon.pump(SLICE).await;

    let mut camera_v2 = FakeNode::attach(&mut daemon);
    camera_v2.register(dataflow, "camera").await;
    assert!(
        camera_v2
            .recv_matching(&mut daemon, |e| matches!(e, NodeEvent::Registered { .. }))
            .await
            .is_some(),
        "the new incarnation registers on its own connection, unambiguously — \
         it does not share a generation number with the outgoing one"
    );
    camera_v2
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![3]),
        })
        .await;
    daemon.pump(SLICE).await;

    let mut seen = Vec::new();
    for _ in 0..3 {
        detect
            .send(NodeRequest::NextEvent {
                timeout: None,
                max_batch: 8,
            })
            .await;
        if let Some(NodeEvent::Input { payload, .. }) = detect
            .recv_matching(&mut daemon, |e| matches!(e, NodeEvent::Input { .. }))
            .await
        {
            seen.push(payload);
        }
    }
    assert_eq!(
        seen,
        vec![vec![1], vec![2], vec![3]],
        "every message from both incarnations arrived, in order, none lost — \
         the reliable path's continuity guarantee across a coordinator-driven replace"
    );

    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_and_disconnect_through_the_coordinator_rewire_delivery_live() {
    let cluster = Cluster::start().await;
    let mut daemon = cluster_daemon("edge", cluster.addr).await;
    let mut cli = CliActor::connect(cluster.addr).await;
    await_registration(&cluster, &mut daemon).await;

    let dataflow = start_pipeline(
        &mut cli,
        "nodes:\n  \
         - id: camera\n    path: dynamic\n    outputs: [image]\n  \
         - id: camera2\n    path: dynamic\n    outputs: [image]\n  \
         - id: detect\n    path: dynamic\n    inputs:\n      frames: camera/image\n",
    )
    .await;
    assert!(
        pump_until(&mut daemon, |d| d
            .dataflow(dataflow)
            .is_some_and(|s| s.node(&node("detect")).is_some()))
        .await
    );
    let mut camera = FakeNode::attach(&mut daemon);
    camera.register(dataflow, "camera").await;
    let mut camera2 = FakeNode::attach(&mut daemon);
    camera2.register(dataflow, "camera2").await;
    let mut detect = FakeNode::attach(&mut daemon);
    detect.register(dataflow, "detect").await;
    for fake in [&mut camera, &mut camera2, &mut detect] {
        assert!(
            fake.recv_matching(&mut daemon, |e| matches!(e, NodeEvent::Registered { .. }))
                .await
                .is_some()
        );
    }

    // The manifest already wired `detect.frames` to `camera/image` —
    // `AddEdge`/`astrs node connect` only ever rewires an *existing*
    // declared input (blueprint §17; `astrs_wire::ControlRequest::AddEdge`'s
    // own docs: "connect an existing output to an existing input"), never
    // conjures a brand-new port a node did not declare. Confirm the
    // manifest wiring works before touching it.
    camera
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![10]),
        })
        .await;
    let first = detect
        .recv_matching(&mut daemon, |e| matches!(e, NodeEvent::Input { .. }))
        .await
        .expect("connected to camera via the manifest");
    assert!(matches!(first, NodeEvent::Input { payload, .. } if payload == vec![10]));

    // `astrs node connect detect frames camera2/image` — the *same*
    // input, a different producer: a live rewire, not a duplicate.
    cli.request_ok(ControlRequest::AddEdge {
        dataflow,
        consumer: node("detect"),
        input: InputSpec::new(
            data("frames"),
            PortRef::from_parts("camera2", "image").unwrap(),
        ),
    })
    .await;

    camera
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![11]),
        })
        .await;
    camera2
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![12]),
        })
        .await;
    detect
        .send(NodeRequest::NextEvent {
            timeout: None,
            max_batch: 8,
        })
        .await;
    let second = detect
        .recv_matching(&mut daemon, |e| matches!(e, NodeEvent::Input { .. }))
        .await
        .expect("connected to camera2 now");
    assert!(
        matches!(second, NodeEvent::Input { payload, .. } if payload == vec![12]),
        "the old producer's message must not arrive: the edge moved, it did not duplicate"
    );

    // `astrs node disconnect detect frames`.
    cli.request_ok(ControlRequest::RemoveEdge {
        dataflow,
        consumer: node("detect"),
        input: data("frames"),
    })
    .await;
    let closed = detect
        .recv_matching(&mut daemon, |e| matches!(e, NodeEvent::InputClosed { .. }))
        .await
        .expect("disconnecting tells the consumer");
    match closed {
        NodeEvent::InputClosed { id, reason, .. } => {
            assert_eq!(id, data("frames"));
            assert_eq!(reason.kind_name(), "disconnected");
        }
        other => panic!("unexpected {other:?}"),
    }

    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_type_unsafe_connect_is_refused_before_it_ever_reaches_the_daemon() {
    let cluster = Cluster::start().await;
    let mut daemon = cluster_daemon("typesafe", cluster.addr).await;
    let mut cli = CliActor::connect(cluster.addr).await;
    await_registration(&cluster, &mut daemon).await;

    let dataflow = start_pipeline(
        &mut cli,
        "strict_types: true\nnodes:\n  \
         - id: camera\n    path: dynamic\n    outputs: [frames]\n    \
           output_types: { frames: std/core/v1/Float32 }\n  \
         - id: bad_camera\n    path: dynamic\n    outputs: [frames]\n    \
           output_types: { frames: std/core/v1/Float64 }\n  \
         - id: detector\n    path: dynamic\n    inputs: { frames: camera/frames }\n    \
           input_types: { frames: std/core/v1/Float32 }\n",
    )
    .await;
    assert!(
        pump_until(&mut daemon, |d| d
            .dataflow(dataflow)
            .is_some_and(|s| s.node(&node("detector")).is_some()))
        .await
    );
    let mut detector = FakeNode::attach(&mut daemon);
    detector.register(dataflow, "detector").await;
    assert!(
        detector
            .recv_matching(&mut daemon, |e| matches!(e, NodeEvent::Registered { .. }))
            .await
            .is_some()
    );

    let reply = cli
        .request(ControlRequest::AddEdge {
            dataflow,
            consumer: node("detector"),
            input: InputSpec::new(
                data("frames"),
                PortRef::from_parts("bad_camera", "frames").unwrap(),
            ),
        })
        .await;
    match reply {
        ControlReply::Error { code, context, .. } => {
            assert_eq!(code, astrs_wire::ErrorCode::FailedPrecondition);
            assert!(
                context
                    .iter()
                    .any(|line| line.contains("Float32") && line.contains("Float64")),
                "the diagnostic names both sides of the mismatch: {context:?}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    // Never reached the daemon at all: `detector`'s input is untouched.
    assert!(
        detector.try_recv().await.is_none(),
        "a type-unsafe edge must never dispatch"
    );

    cluster.shutdown().await;
}
