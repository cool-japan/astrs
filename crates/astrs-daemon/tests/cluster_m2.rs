//! The M2 backbone, end to end: one coordinator and two daemons, in one
//! process, over real loopback sockets (blueprint §4.2, §6.4, §7.3, §12).
//!
//! ```text
//!            ┌──────── CoordinatorServer (port 0, own task) ────────┐
//!   CLI ────►│  ControlRequest ─► handlers ─► CoordinatorEvent ────►│
//!            └───────▲──────────────────────────────────┬───────────┘
//!                    │ DaemonEvent            CoordinatorEvent
//!             ┌──────┴──────┐                    ┌──────▼──────┐
//!             │  daemon A   │                    │  daemon B   │
//!             │ machine=    │  PeerEvent::Output │ machine=    │
//!             │  robot-a    │═══════════════════►│  robot-b    │
//!             │  node camera│   (peer TCP, §6.4) │  node detect│
//!             └─────────────┘                    └─────────────┘
//! ```
//!
//! # Why in-process, and why pumped by hand
//!
//! Every part that matters is real: real TCP for the two control legs and
//! the peer leg, the real `Hello`/`Welcome` handshake on each, real
//! `oxicode` frames, the real placement resolver, the real spawn dispatch.
//! The only thing that is not real is the machine boundary — and neither a
//! daemon nor the coordinator has any way to tell.
//!
//! The two daemons are driven with [`astrs_daemon::Daemon::pump`] rather
//! than [`astrs_daemon::Daemon::run`] because a test needs to *look* at them
//! between steps (is this one degraded? did that route open?), which needs
//! the `&mut Daemon` that `run` would hold for its whole life. The
//! coordinator has no such need and runs on its own task, exactly as it does
//! in production.
//!
//! # The relay
//!
//! [`TcpRelay`] sits between daemon B and the coordinator so a test can cut
//! that one leg — and only that leg — at a chosen instant. Restarting the
//! coordinator instead would prove something weaker (a fresh coordinator has
//! no mutation log to catch up *from*), and killing a `TcpStream` a test does
//! not own is not possible at all.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use astrs_coordinator::{Coordinator, CoordinatorConfig, CoordinatorServer};
use astrs_daemon::coordinator::UplinkConfig;
use astrs_daemon::server::NodeListeners;
use astrs_daemon::{Daemon, DaemonConfig, ListenConfig, PeerConfig, RuntimePaths};
use astrs_transport::{ConnectionCounters, FramedStream, HandshakeParams, LocalIdentity, initiate};
use astrs_wire::io::{AsyncFrameReader, AsyncFrameWriter};
use astrs_wire::{
    AuthToken, ControlReply, ControlRequest, DaemonId, DataId, DataflowId, DataflowSource,
    DataflowStatus, FeatureFlags, FrameKind, FrameLimits, LogLevel, LogQuery, LogRecord,
    MachineName, Metadata, NodeEvent, NodeExitCause, NodeHandshake, NodeId, NodeRequest,
    OutputPayload, Role,
};
use tokio::io::DuplexStream;
use tokio::net::{TcpListener, TcpStream};

/// How long any single wait may take before the test calls the cluster stuck.
///
/// Generous enough that a loaded CI machine never fails spuriously, short
/// enough that a genuinely wedged exchange fails the test instead of hanging
/// the suite.
const DEADLINE: Duration = Duration::from_secs(20);

/// One slice of both daemons' event loops.
///
/// Short: every `pump_until` below runs many of them, and a shorter slice is
/// a finer interleaving between the two daemons and the coordinator's own
/// task.
const SLICE: Duration = Duration::from_millis(20);

/// The cluster token every leg authenticates with (§16).
fn token() -> AuthToken {
    AuthToken::from_bytes([0x5A; 32])
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).expect("a legal node id")
}

fn data(name: &str) -> DataId {
    DataId::new(name).expect("a legal port id")
}

/// A short scratch directory — the daemon's Unix socket lives inside it and
/// a socket path has a hard length limit (104 bytes on macOS).
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("as-m2-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// The canonical split graph: a producer on one machine, its consumer on the
/// other. Both nodes are `path: dynamic`, so no process is ever spawned and
/// the test scripts both ends by hand.
const SPLIT_PIPELINE: &str = "\
nodes:
  - id: camera
    path: dynamic
    deploy:
      machine: robot-a
    outputs: [image]
  - id: detect
    path: dynamic
    deploy:
      machine: robot-b
    inputs:
      frames:
        source: camera/image
        queue_size: 8
";

/// The same split, with a consumer that is a real process and exits
/// non-zero — a crash on daemon B that the CLI must see in the dataflow's
/// result (§12: typed causes, not strings).
///
/// `/bin/sh` rather than `/usr/bin/false`: the former is at that exact path
/// on every platform this workspace builds for, and the exit code is then
/// this test's own choice rather than a coreutils convention.
const CRASHING_PIPELINE: &str = "\
nodes:
  - id: camera
    path: dynamic
    deploy:
      machine: robot-a
    outputs: [image]
  - id: crasher
    path: /bin/sh
    args: ['-c', 'exit 3']
    deploy:
      machine: robot-b
    inputs:
      frames: camera/image
";

// ---------------------------------------------------------------------
// The coordinator
// ---------------------------------------------------------------------

/// A running coordinator, and the pieces a test needs to inspect and stop it.
struct Cluster {
    addr: SocketAddr,
    coordinator: Coordinator,
    handle: astrs_coordinator::ServerHandle,
    task: tokio::task::JoinHandle<astrs_coordinator::Result<()>>,
}

impl Cluster {
    async fn start() -> Self {
        // The heartbeat budget is deliberately far longer than any test
        // takes. A coordinator declares a daemon lost after
        // `interval × limit` of silence (§12) and *removes* it, which makes
        // its next frame arrive on a connection the coordinator no longer
        // recognises — a reconnect flap that would make every assertion here
        // race the watchdog instead of testing what it means to test. The
        // watchdog itself has its own tests, next to the code that runs it.
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

/// The CLI's end of one coordinator connection.
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
}

// ---------------------------------------------------------------------
// The relay
// ---------------------------------------------------------------------

/// A cuttable TCP relay: everything written to [`TcpRelay::addr`] is
/// forwarded to the upstream address, until [`TcpRelay::cut`] refuses new
/// connections and drops every open one.
///
/// The one honest way to partition a link a test does not own either end of.
struct TcpRelay {
    addr: SocketAddr,
    open: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

impl TcpRelay {
    async fn start(upstream: SocketAddr) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
        let addr = listener.local_addr().expect("a bound address");
        let open = Arc::new(AtomicBool::new(true));
        let generation = Arc::new(AtomicU64::new(0));
        let task = tokio::spawn(relay_loop(
            listener,
            upstream,
            Arc::clone(&open),
            Arc::clone(&generation),
        ));
        Self {
            addr,
            open,
            generation,
            task,
        }
    }

    /// Refuses new connections and drops every open one.
    fn cut(&self) {
        self.open.store(false, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Accepts connections again.
    fn restore(&self) {
        self.open.store(true, Ordering::SeqCst);
    }

    fn stop(self) {
        self.task.abort();
    }
}

/// One relay's accept loop.
async fn relay_loop(
    listener: TcpListener,
    upstream: SocketAddr,
    open: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
) {
    loop {
        let Ok((inbound, _)) = listener.accept().await else {
            return;
        };
        if !open.load(Ordering::SeqCst) {
            drop(inbound);
            continue;
        }
        let Ok(outbound) = TcpStream::connect(upstream).await else {
            continue;
        };
        let epoch = generation.load(Ordering::SeqCst);
        let generation = Arc::clone(&generation);
        tokio::spawn(async move {
            let mut inbound = inbound;
            let mut outbound = outbound;
            let copy = tokio::io::copy_bidirectional(&mut inbound, &mut outbound);
            tokio::pin!(copy);
            loop {
                tokio::select! {
                    _ = &mut copy => return,
                    () = tokio::time::sleep(Duration::from_millis(5)) => {
                        if generation.load(Ordering::SeqCst) != epoch {
                            // A cut: drop both halves so each end sees the
                            // partition rather than a silent stall.
                            return;
                        }
                    }
                }
            }
        });
    }
}

// ---------------------------------------------------------------------
// The daemons
// ---------------------------------------------------------------------

/// Builds a cluster daemon: its own runtime directory, no node listener (the
/// test attaches nodes in-process), no shared-memory plane, a peer listener
/// on an ephemeral loopback port, and an uplink to `coordinator`.
async fn cluster_daemon(name: &str, machine: &str, coordinator: SocketAddr) -> Daemon {
    let config = DaemonConfig::new(RuntimePaths::under(scratch(name)))
        .with_listen(ListenConfig::none())
        .with_shm(false)
        .with_auth(token())
        // §24.2's 5 s default is a robot's cadence, not a test's: these
        // daemons only beat while they are being pumped, and the coordinator
        // must hear from them often enough to keep them in its registry.
        .with_heartbeat_interval(Duration::from_millis(100))
        .with_metrics_interval(Duration::from_millis(100))
        .with_machine(MachineName::new(machine).expect("a legal machine name"))
        .with_peer(PeerConfig::new(token()).with_loopback(0));
    let mut daemon = Daemon::new(config).expect("a daemon");
    let uplink = UplinkConfig::new(coordinator, token())
        .with_machine(MachineName::new(machine).expect("a legal machine name"))
        .with_label("zone", machine)
        .with_backoff(Duration::from_millis(20), Duration::from_millis(100))
        .with_dial_timeout(Duration::from_millis(500));
    daemon
        .connect_coordinator(uplink)
        .await
        .expect("the uplink starts");
    daemon
}

/// One scripted node's end of an in-process connection to a daemon.
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

    /// The next event, or `None` if none arrives within one slice.
    async fn try_recv(&mut self) -> Option<NodeEvent> {
        tokio::time::timeout(SLICE, self.reader.read_message::<NodeEvent>())
            .await
            .ok()?
            .ok()?
    }
}

/// Runs both daemons' loops for one slice.
async fn pump_pair(first: &mut Daemon, second: &mut Daemon) {
    tokio::join!(first.pump(SLICE), second.pump(SLICE));
}

/// Pumps both daemons until `ready` holds, or the deadline passes.
async fn pump_until<F>(first: &mut Daemon, second: &mut Daemon, mut ready: F) -> bool
where
    F: FnMut(&Daemon, &Daemon) -> bool,
{
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        if ready(first, second) {
            return true;
        }
        pump_pair(first, second).await;
    }
    ready(first, second)
}

/// Pumps both daemons until `task` finishes, then yields its output.
///
/// The shape every CLI verb that needs the daemons to answer takes: the
/// request runs on its own task, and the daemons are pumped underneath it
/// until it resolves.
async fn pump_while<T>(
    first: &mut Daemon,
    second: &mut Daemon,
    task: tokio::task::JoinHandle<T>,
) -> T {
    let deadline = Instant::now() + DEADLINE;
    while !task.is_finished() && Instant::now() < deadline {
        pump_pair(first, second).await;
    }
    // Bounded even past the pump deadline: a task that never resolves must
    // fail this test, not hang the suite behind an unbounded `await`.
    tokio::time::timeout(SLICE * 4, task)
        .await
        .expect("the driving task finished")
        .expect("the driving task did not panic")
}

/// Waits until both daemons have registered with the coordinator.
async fn await_registration(cluster: &Cluster, first: &mut Daemon, second: &mut Daemon) {
    let registered = pump_until(first, second, |_, _| {
        cluster.coordinator.daemons().len() == 2
    })
    .await;
    assert!(
        registered,
        "both daemons must register with the coordinator"
    );
}

/// Starts `SPLIT_PIPELINE` and returns its dataflow id.
async fn start_split_pipeline(
    cluster: &Cluster,
    cli: &mut CliActor,
    first: &mut Daemon,
    second: &mut Daemon,
) -> DataflowId {
    await_registration(cluster, first, second).await;
    let reply = cli
        .request(ControlRequest::Start {
            source: DataflowSource::Manifest {
                yaml: SPLIT_PIPELINE.to_owned(),
                working_dir: None,
            },
            name: Some("m2".to_owned()),
            detach: true,
        })
        .await;
    match reply {
        ControlReply::Started { dataflow, .. } => dataflow,
        other => panic!("start refused: {other:?}"),
    }
}

// ---------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dataflow_split_across_two_daemons_exchanges_data_cross_daemon() {
    let cluster = Cluster::start().await;
    let mut alpha = cluster_daemon("x-a", "robot-a", cluster.addr).await;
    let mut beta = cluster_daemon("x-b", "robot-b", cluster.addr).await;
    let mut cli = CliActor::connect(cluster.addr).await;

    let dataflow = start_split_pipeline(&cluster, &mut cli, &mut alpha, &mut beta).await;

    // Each daemon is told to spawn exactly the node its machine was given.
    let dispatched = pump_until(&mut alpha, &mut beta, |a, b| {
        a.dataflow(dataflow)
            .is_some_and(|state| state.node(&node("camera")).is_some())
            && b.dataflow(dataflow)
                .is_some_and(|state| state.node(&node("detect")).is_some())
    })
    .await;
    assert!(dispatched, "the spawn dispatch must fan out by machine");
    assert!(
        alpha
            .dataflow(dataflow)
            .is_some_and(|state| state.node(&node("detect")).is_none()),
        "a daemon is told about its own nodes, not the whole graph"
    );

    let mut camera = FakeNode::attach(&mut alpha);
    let mut detect = FakeNode::attach(&mut beta);
    camera.register(dataflow, "camera").await;
    detect.register(dataflow, "detect").await;
    detect
        .send(NodeRequest::Subscribe {
            inputs: vec![data("frames")],
        })
        .await;

    // The cross-daemon route: whichever daemon sorts lower dials, and the
    // producing daemon opens the route over the resulting link (§6.4).
    let routed = pump_until(&mut alpha, &mut beta, |a, b| {
        a.peers().routes().established_count() > 0 || b.peers().routes().inbound_count() > 0
    })
    .await;
    assert!(
        routed,
        "a producer and consumer on different daemons must get a peer route"
    );
    assert_eq!(
        alpha.peer_directory().len(),
        1,
        "one peer, from one directive"
    );
    assert_eq!(beta.peer_directory().len(), 1);

    camera
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::Inline {
                bytes: b"frame-0".to_vec(),
            },
        })
        .await;

    let mut delivered = None;
    let deadline = Instant::now() + DEADLINE;
    while delivered.is_none() && Instant::now() < deadline {
        pump_pair(&mut alpha, &mut beta).await;
        while let Some(event) = detect.try_recv().await {
            if let NodeEvent::Input { id, payload, .. } = event
                && id == data("frames")
            {
                delivered = Some(payload);
                break;
            }
        }
    }
    assert_eq!(
        delivered.as_deref(),
        Some(b"frame-0".as_slice()),
        "the payload must cross the machine boundary over the peer leg"
    );

    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_coordinator_partition_degrades_then_reconnects_and_catches_up() {
    let cluster = Cluster::start().await;
    let relay = TcpRelay::start(cluster.addr).await;

    // Daemon alpha talks to the coordinator directly; daemon beta talks to it
    // through the relay, so only beta's control leg can be cut.
    let mut alpha = cluster_daemon("p-a", "robot-a", cluster.addr).await;
    let mut beta = cluster_daemon("p-b", "robot-b", relay.addr).await;
    let mut cli = CliActor::connect(cluster.addr).await;

    let dataflow = start_split_pipeline(&cluster, &mut cli, &mut alpha, &mut beta).await;
    let spawned = pump_until(&mut alpha, &mut beta, |_, b| {
        b.dataflow(dataflow)
            .is_some_and(|state| state.node(&node("detect")).is_some())
    })
    .await;
    assert!(spawned, "beta must receive its half of the graph");

    let cursor_before = beta
        .cluster()
        .uplink()
        .expect("an uplink")
        .state()
        .catch_up_seq();
    assert_eq!(
        beta.cluster().uplink().expect("an uplink").state().epoch(),
        1,
        "one registration so far"
    );

    // ---- the partition -------------------------------------------------
    relay.cut();
    let degraded = pump_until(&mut alpha, &mut beta, |_, b| b.cluster().is_degraded()).await;
    assert!(degraded, "a daemon that loses its coordinator says so");
    assert!(
        beta.dataflow(dataflow)
            .is_some_and(|state| state.node(&node("detect")).is_some()),
        "degraded-autonomous keeps the local dataflow exactly as it was (§12)"
    );

    // Work happens while the link is down: the coordinator records mutations
    // beta cannot see, and beta produces reports it cannot send.
    let set = cli
        .request(ControlRequest::SetParam {
            scope: astrs_wire::ParamScope::dataflow_scope(dataflow),
            key: astrs_wire::ParamKey::new("exposure").unwrap(),
            value: astrs_wire::Parameter::Integer(42),
            create_only: false,
        })
        .await;
    assert_eq!(set, ControlReply::Ok, "the cluster keeps working");

    let buffered = pump_until(&mut alpha, &mut beta, |_, b| {
        b.cluster().uplink().expect("an uplink").buffered() > 0
    })
    .await;
    assert!(
        buffered,
        "a daemon with no coordinator buffers its reports rather than losing them (§12)"
    );

    // ---- the repair ----------------------------------------------------
    relay.restore();
    let reconnected = pump_until(&mut alpha, &mut beta, |_, b| {
        b.cluster().uplink().expect("an uplink").state().epoch() >= 2
    })
    .await;
    assert!(reconnected, "the uplink reconnects with backoff (§12)");

    let caught_up = pump_until(&mut alpha, &mut beta, |_, b| {
        b.cluster()
            .uplink()
            .expect("an uplink")
            .state()
            .catch_up_seq()
            > cursor_before
    })
    .await;
    assert!(
        caught_up,
        "the replay must carry the mutations recorded while the daemon was gone \
         (cursor was {cursor_before})"
    );

    relay.stop();
    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_partition_closes_the_input_and_its_repair_recovers_it() {
    let cluster = Cluster::start().await;
    let mut alpha = cluster_daemon("r-a", "robot-a", cluster.addr).await;
    let mut beta = cluster_daemon("r-b", "robot-b", cluster.addr).await;
    let mut cli = CliActor::connect(cluster.addr).await;

    let dataflow = start_split_pipeline(&cluster, &mut cli, &mut alpha, &mut beta).await;
    let dispatched = pump_until(&mut alpha, &mut beta, |a, b| {
        a.dataflow(dataflow)
            .is_some_and(|state| state.node(&node("camera")).is_some())
            && b.dataflow(dataflow)
                .is_some_and(|state| state.node(&node("detect")).is_some())
    })
    .await;
    assert!(dispatched);

    let mut camera = FakeNode::attach(&mut alpha);
    let mut detect = FakeNode::attach(&mut beta);
    camera.register(dataflow, "camera").await;
    detect.register(dataflow, "detect").await;
    detect
        .send(NodeRequest::Subscribe {
            inputs: vec![data("frames")],
        })
        .await;

    let routed = pump_until(&mut alpha, &mut beta, |_, b| {
        b.peers().routes().inbound_count() > 0
    })
    .await;
    assert!(routed, "the consumer's daemon must admit the route first");

    // Cut the link from whichever side actually holds it.
    let alpha_id = alpha.config().id().clone();
    let beta_id = beta.config().id().clone();
    if alpha.peers().is_connected(&beta_id) {
        alpha
            .disconnect_peer(&beta_id, "the test cut the cable")
            .await;
    } else {
        beta.disconnect_peer(&alpha_id, "the test cut the cable")
            .await;
    }

    let closed = drain_for(
        &mut alpha,
        &mut beta,
        &mut detect,
        |event| matches!(event, NodeEvent::InputClosed { id, .. } if *id == data("frames")),
    )
    .await;
    assert!(
        closed,
        "a peer partition surfaces to the application as an ordinary InputClosed (§12)"
    );

    let recovered = drain_for(
        &mut alpha,
        &mut beta,
        &mut detect,
        |event| matches!(event, NodeEvent::InputRecovered { id, .. } if *id == data("frames")),
    )
    .await;
    assert!(
        recovered,
        "the reconciliation backstop re-dials and the consumer sees InputRecovered (§12)"
    );

    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_failure_on_one_daemon_reaches_the_cli_as_a_dataflow_result() {
    let cluster = Cluster::start().await;
    let mut alpha = cluster_daemon("f-a", "robot-a", cluster.addr).await;
    let mut beta = cluster_daemon("f-b", "robot-b", cluster.addr).await;
    let mut cli = CliActor::connect(cluster.addr).await;

    await_registration(&cluster, &mut alpha, &mut beta).await;
    let reply = cli
        .request(ControlRequest::Start {
            source: DataflowSource::Manifest {
                yaml: CRASHING_PIPELINE.to_owned(),
                working_dir: None,
            },
            name: Some("m2-crash".to_owned()),
            detach: true,
        })
        .await;
    let dataflow = match reply {
        ControlReply::Started { dataflow, .. } => dataflow,
        other => panic!("start refused: {other:?}"),
    };

    let dispatched = pump_until(&mut alpha, &mut beta, |a, b| {
        a.dataflow(dataflow)
            .is_some_and(|state| state.node(&node("camera")).is_some())
            && b.dataflow(dataflow)
                .is_some_and(|state| state.node(&node("crasher")).is_some())
    })
    .await;
    assert!(dispatched);

    // The producer is a scripted dynamic node that finishes cleanly; the
    // consumer on the *other* daemon is a real process that exits non-zero.
    let mut camera = FakeNode::attach(&mut alpha);
    camera.register(dataflow, "camera").await;
    pump_pair(&mut alpha, &mut beta).await;
    camera
        .send(NodeRequest::CloseOutputs { outputs: vec![] })
        .await;
    drop(camera);

    let crashed = pump_until(&mut alpha, &mut beta, |_, b| {
        b.dataflow(dataflow)
            .and_then(|state| state.node(&node("crasher")))
            .and_then(|state| state.exit_cause().cloned())
            .is_some_and(|cause| cause.is_failure())
    })
    .await;
    assert!(crashed, "the far daemon must observe its node's failure");

    let checked = tokio::spawn({
        let addr = cluster.addr;
        async move {
            let mut cli = CliActor::connect(addr).await;
            let deadline = Instant::now() + DEADLINE;
            loop {
                let reply = cli
                    .request(ControlRequest::Check {
                        dataflow: Some(dataflow),
                    })
                    .await;
                if matches!(reply, ControlReply::DataflowResult { .. })
                    || Instant::now() >= deadline
                {
                    return reply;
                }
                tokio::time::sleep(SLICE).await;
            }
        }
    });
    let reply = pump_while(&mut alpha, &mut beta, checked).await;

    match reply {
        ControlReply::DataflowResult { result } => {
            assert!(
                result.has_failures(),
                "a crash on the far daemon must be a failure on the CLI's result: {result:?}"
            );
            assert!(
                result
                    .failed_nodes()
                    .any(|(id, cause)| id.as_str() == "crasher" && cause.is_failure()),
                "the failing node must be named: {result:?}"
            );
        }
        other => panic!("expected a DataflowResult, got {other:?}"),
    }

    cluster.shutdown().await;
}

/// The `astrs start --attach` regression, on the leg that can be made
/// deterministic here: a node that never registers must *settle* the ready
/// barrier rather than hold it forever.
///
/// `--attach` sends `Start { detach: false }`, and the coordinator answers it
/// only once every hosting daemon has reported `AllNodesReady`. The barrier
/// used to be "every node is registered *right now*", which the `crasher`
/// below can never satisfy — it is a real process that exits before it ever
/// speaks the protocol. Daemon B therefore reported nothing, the coordinator's
/// `PendingSpawn` never resolved, and the CLI timed out after 30 s on a
/// dataflow that had long since finished.
///
/// The barrier now settles on "registered this incarnation, or terminal", and
/// `AllNodesReady { nodes }` names only the nodes that actually registered —
/// which is the only vocabulary the wire has for "this one never came up",
/// and what the coordinator's dispatched-vs-reported diff reads.
///
/// The failure mode this guards against is a *hang*, so the assertion that
/// matters is that a reply arrives at all: [`pump_while`] gives up after
/// [`DEADLINE`], well inside the coordinator's own 60 s wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_non_detached_start_is_answered_when_a_node_never_registers() {
    let cluster = Cluster::start().await;
    let mut alpha = cluster_daemon("nr-a", "robot-a", cluster.addr).await;
    let mut beta = cluster_daemon("nr-b", "robot-b", cluster.addr).await;

    await_registration(&cluster, &mut alpha, &mut beta).await;

    // The CLI's own connection has to run on its own task: a non-detached
    // `Start` does not answer until the daemons underneath it have reported,
    // and the daemons are pumped by this one.
    let started = tokio::spawn({
        let addr = cluster.addr;
        async move {
            let mut cli = CliActor::connect(addr).await;
            cli.request(ControlRequest::Start {
                source: DataflowSource::Manifest {
                    yaml: CRASHING_PIPELINE.to_owned(),
                    working_dir: None,
                },
                name: Some("m2-attach-never".to_owned()),
                detach: false,
            })
            .await
        }
    });

    // `camera` is the dynamic node of `CRASHING_PIPELINE`, and nothing
    // attaches to it in this test: a `path: dynamic` node is excluded from the
    // barrier precisely so a graph does not block on an operator who has not
    // arrived yet, so daemon A is ready without it.
    let reply = pump_while(&mut alpha, &mut beta, started).await;

    match reply {
        ControlReply::Error { code, context, .. } => {
            assert_eq!(
                code,
                astrs_wire::ErrorCode::FailedPrecondition,
                "a node that never came up is a failed precondition, not an internal error"
            );
            assert!(
                context.iter().any(|entry| entry.contains("crasher")),
                "the reply must name the node that never registered: {context:?}"
            );
        }
        other => panic!("expected a spawn failure naming `crasher`, got {other:?}"),
    }

    cluster.shutdown().await;
}

/// The same verb's happy path, in the shape `astrs start --attach` actually
/// sends it: `detach: false`, both nodes register, both then exit, and the
/// reply is `Started`.
///
/// The barrier is a *historical* question — "did every node come up?" — and
/// the daemon re-derives it from its node table on a periodic pass, so the
/// answer must not depend on when that pass happens to run relative to the
/// nodes' exits. `Daemon::pump` ticks after every event, so this test cannot
/// force the interleaving that broke production (`Daemon::run` returns events
/// as fast as they arrive and ticks only when the channel drains, so a short
/// dataflow's whole life is one burst with no pass in it); the deterministic
/// guard for that is `DataflowState`'s own unit tests. What this test does
/// witness end to end is that the exact request `--attach` sends is answered
/// on a two-daemon cluster whose nodes have already finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_non_detached_start_is_answered_when_the_nodes_have_already_finished() {
    let cluster = Cluster::start().await;
    let mut alpha = cluster_daemon("af-a", "robot-a", cluster.addr).await;
    let mut beta = cluster_daemon("af-b", "robot-b", cluster.addr).await;

    await_registration(&cluster, &mut alpha, &mut beta).await;

    let started = tokio::spawn({
        let addr = cluster.addr;
        async move {
            let mut cli = CliActor::connect(addr).await;
            cli.request(ControlRequest::Start {
                source: DataflowSource::Manifest {
                    yaml: SPLIT_PIPELINE.to_owned(),
                    working_dir: None,
                },
                name: Some("m2-attach-fast".to_owned()),
                detach: false,
            })
            .await
        }
    });

    // The id the reply would have carried has to come from somewhere else,
    // because the reply is what this test is waiting for: `List` on a second
    // connection, which the coordinator answers from its registry the moment
    // `Start` has registered the dataflow.
    let listed = tokio::spawn({
        let addr = cluster.addr;
        async move {
            let mut cli = CliActor::connect(addr).await;
            let deadline = Instant::now() + DEADLINE;
            loop {
                if let ControlReply::DataflowList { dataflows, .. } =
                    cli.request(ControlRequest::List { all: true }).await
                    && let Some(summary) = dataflows.first()
                {
                    return Some(summary.id);
                }
                if Instant::now() >= deadline {
                    return None;
                }
                tokio::time::sleep(SLICE).await;
            }
        }
    });
    let dataflow = pump_while(&mut alpha, &mut beta, listed)
        .await
        .expect("the coordinator registers the dataflow before it dispatches it");

    // Both ends attach, run and finish while the `Start` is still in flight —
    // a fast dataflow, which is every example graph in this workspace.
    let dispatched = pump_until(&mut alpha, &mut beta, |a, b| {
        a.dataflow(dataflow)
            .is_some_and(|state| state.node(&node("camera")).is_some())
            && b.dataflow(dataflow)
                .is_some_and(|state| state.node(&node("detect")).is_some())
    })
    .await;
    assert!(dispatched, "both daemons must be told what to spawn");

    let mut camera = FakeNode::attach(&mut alpha);
    let mut detect = FakeNode::attach(&mut beta);
    camera.register(dataflow, "camera").await;
    detect.register(dataflow, "detect").await;
    pump_pair(&mut alpha, &mut beta).await;
    camera
        .send(NodeRequest::CloseOutputs { outputs: vec![] })
        .await;
    drop(camera);
    drop(detect);

    let reply = pump_while(&mut alpha, &mut beta, started).await;
    assert!(
        matches!(reply, ControlReply::Started { .. }),
        "an attached start whose nodes have finished is still a successful start: {reply:?}"
    );

    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logs_from_both_daemons_merge_in_hlc_order() {
    let cluster = Cluster::start().await;
    let mut alpha = cluster_daemon("l-a", "robot-a", cluster.addr).await;
    let mut beta = cluster_daemon("l-b", "robot-b", cluster.addr).await;
    let mut cli = CliActor::connect(cluster.addr).await;

    let dataflow = start_split_pipeline(&cluster, &mut cli, &mut alpha, &mut beta).await;
    let dispatched = pump_until(&mut alpha, &mut beta, |a, b| {
        a.dataflow(dataflow)
            .is_some_and(|state| state.node(&node("camera")).is_some())
            && b.dataflow(dataflow)
                .is_some_and(|state| state.node(&node("detect")).is_some())
    })
    .await;
    assert!(dispatched);

    // Interleaved stamps: alpha writes the odd ones, beta the even ones, so a
    // merged answer is only in order if it was actually merged.
    for seq in 0..6u64 {
        let record = LogRecord::new(
            astrs_time::HlcTimestamp::new(1_000 + seq, 0),
            LogLevel::Info,
            format!("line-{seq}"),
        )
        .with_dataflow(dataflow)
        .with_node(if seq % 2 == 0 {
            node("camera")
        } else {
            node("detect")
        });
        if seq % 2 == 0 {
            alpha.record_log(record);
        } else {
            beta.record_log(record);
        }
    }

    let fetched = tokio::spawn({
        let addr = cluster.addr;
        async move {
            let mut cli = CliActor::connect(addr).await;
            cli.request(ControlRequest::Logs {
                dataflow,
                node: None,
                query: LogQuery::new(),
            })
            .await
        }
    });
    let reply = pump_while(&mut alpha, &mut beta, fetched).await;

    match reply {
        ControlReply::Logs { records, .. } => {
            let messages: Vec<&str> = records
                .iter()
                .map(|record| record.message.as_str())
                .collect();
            assert_eq!(
                messages,
                ["line-0", "line-1", "line-2", "line-3", "line-4", "line-5"],
                "records from both daemons must come back HLC-ordered (§13)"
            );
            let daemons: std::collections::BTreeSet<&str> = records
                .iter()
                .filter_map(|record| record.node.as_ref().map(NodeId::as_str))
                .collect();
            assert_eq!(daemons.len(), 2, "both daemons answered");
        }
        other => panic!("expected Logs, got {other:?}"),
    }

    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_registry_reports_both_daemons_with_their_machines_and_labels() {
    let cluster = Cluster::start().await;
    let mut alpha = cluster_daemon("i-a", "robot-a", cluster.addr).await;
    let mut beta = cluster_daemon("i-b", "robot-b", cluster.addr).await;
    let mut cli = CliActor::connect(cluster.addr).await;

    await_registration(&cluster, &mut alpha, &mut beta).await;

    // `ConnectedDaemons` is answered from the durable record, which
    // `register` writes through a `spawn_blocking` hop *after* the live
    // registry already has the daemon. Poll rather than race that hop.
    let mut listed = Vec::new();
    let deadline = Instant::now() + DEADLINE;
    while listed.len() < 2 && Instant::now() < deadline {
        let reply = cli
            .request(ControlRequest::ConnectedDaemons {
                include_unreachable: false,
            })
            .await;
        match reply {
            ControlReply::DaemonList { daemons } => listed = daemons,
            other => panic!("expected a DaemonList, got {other:?}"),
        }
        if listed.len() < 2 {
            pump_pair(&mut alpha, &mut beta).await;
        }
    }
    {
        let daemons = listed;
        {
            assert_eq!(daemons.len(), 2, "the live registry drives the answer");
            let machines: std::collections::BTreeSet<&str> = daemons
                .iter()
                .filter_map(|info| info.labels.get("zone").map(String::as_str))
                .collect();
            assert!(machines.contains("robot-a"), "{daemons:?}");
            assert!(machines.contains("robot-b"), "{daemons:?}");
            for info in &daemons {
                assert!(
                    info.address.starts_with("tcp:"),
                    "a daemon registers the address peers dial: {info:?}"
                );
                assert!(info.reachable);
            }
        }
    }

    // The same two ids, from the coordinator's own registry.
    let ids: Vec<DaemonId> = cluster
        .coordinator
        .daemons()
        .handles()
        .map(|handle| handle.id.clone())
        .collect();
    assert_eq!(ids.len(), 2);

    let list = cli.request(ControlRequest::List { all: true }).await;
    assert!(matches!(list, ControlReply::DataflowList { .. }));

    cluster.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_from_the_cli_reaches_both_daemons_without_stopping_either() {
    let cluster = Cluster::start().await;
    let mut alpha = cluster_daemon("s-a", "robot-a", cluster.addr).await;
    let mut beta = cluster_daemon("s-b", "robot-b", cluster.addr).await;
    let mut cli = CliActor::connect(cluster.addr).await;

    let dataflow = start_split_pipeline(&cluster, &mut cli, &mut alpha, &mut beta).await;
    let dispatched = pump_until(&mut alpha, &mut beta, |a, b| {
        a.dataflow(dataflow).is_some() && b.dataflow(dataflow).is_some()
    })
    .await;
    assert!(dispatched);

    let stop = cli
        .request(ControlRequest::Stop {
            dataflow,
            grace: None,
        })
        .await;
    assert_eq!(stop, ControlReply::Ok);

    let stopped = pump_until(&mut alpha, &mut beta, |a, b| {
        a.dataflow(dataflow)
            .is_some_and(|state| state.status() != DataflowStatus::Starting)
            && b.dataflow(dataflow)
                .is_some_and(|state| state.status() != DataflowStatus::Starting)
    })
    .await;
    assert!(stopped, "both daemons act on one StopDataflow");
    assert!(
        !alpha.state().is_shutting_down() && !beta.state().is_shutting_down(),
        "stopping a dataflow must not stop the daemons hosting it"
    );
    assert!(alpha.handle().is_open() && beta.handle().is_open());

    cluster.shutdown().await;
}

/// Pumps both daemons, draining `subject`'s event stream, until one event
/// satisfies `wanted` or the deadline passes.
async fn drain_for<F>(
    first: &mut Daemon,
    second: &mut Daemon,
    subject: &mut FakeNode,
    mut wanted: F,
) -> bool
where
    F: FnMut(&NodeEvent) -> bool,
{
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        pump_pair(first, second).await;
        while let Some(event) = subject.try_recv().await {
            if wanted(&event) {
                return true;
            }
        }
    }
    false
}

/// Keeps the unused-import lint honest about the one type only a helper
/// signature mentions.
#[allow(dead_code)]
fn _type_anchors(_: BTreeMap<NodeId, NodeExitCause>) {}
