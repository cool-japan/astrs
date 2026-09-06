//! §12 conformance zoo, scenario 16/16 (transport-failover-under-load) —
//! blueprint §12, §20.3, §6.4.
//!
//! The 2-daemon cluster harness from `cluster_m2.rs`, minus the coordinator
//! relay it uses for the *control* leg: this file hard-drops the *peer* leg
//! instead, while a producer keeps publishing across the cut, and proves
//! three things `cluster_m2.rs`'s own
//! `a_peer_partition_closes_the_input_and_its_repair_recovers_it` does not
//! attempt — continuous traffic across the outage, a loss bound tied to the
//! declared `queue_size`, and that neither daemon deadlocks or panics while
//! it happens:
//!
//! ```text
//!   camera (robot-a) ──frames──► detect (robot-b), queue_size: 8
//!            │  send seq 0,1,2,… every pump slice, uninterrupted
//!            │
//!   alpha.disconnect_peer(&beta_id) ─── the socket beta's mux reads from
//!            │                          simply ends — no goodbye frame the
//!            │                          far side treats specially, exactly
//!            │                          what a severed cable looks like
//!            │                          from the *surviving* end (see
//!            │                          `PeerManager::disconnect`'s own
//!            │                          doc: "closes the socket first, so
//!            │                          the peer at the other end observes
//!            │                          the partition instead of a
//!            │                          connection that quietly stops
//!            │                          carrying frames")
//!            │
//!   detect sees InputClosed ── camera keeps sending, all of it lost ──►
//!            │
//!   PEER_RECONCILE_INTERVAL ticks ── alpha re-dials on its own (§12) ──►
//!            │
//!   detect sees InputRecovered ── every seq sent from here on arrives ──►
//! ```
//!
//! # Not scenario 15
//!
//! Nothing here kills a process. Both daemons stay the same `Daemon` objects
//! for the whole test; only the peer *socket* between them dies and comes
//! back. Scenario 15 (daemon-killed-mid-route,
//! `daemon_killed_mid_route.rs`) is the one that SIGKILLs a real daemon
//! process — a different failure with a different blast radius (the whole
//! machine's worth of nodes, not one link), and the two must not be
//! conflated.
//!
//! # Why `disconnect_peer` counts as "hard-drop the socket"
//!
//! `astrs_daemon::coordinator::UplinkConfig::with_peer_address` cannot be
//! used to route the peer leg through a `TcpRelay` the way `cluster_m2.rs`
//! does for the *coordinator* leg: `Daemon::connect_coordinator` always
//! overwrites any caller-supplied peer address with the address its own
//! listener actually bound to (deliberately — a daemon must never announce
//! an address it does not truthfully listen on), so there is no seam to
//! interpose a relay on without fighting that invariant. `disconnect_peer`
//! is the honest alternative already built for exactly this: called on the
//! *producer's* daemon, it closes the raw socket without the graceful
//! per-message negotiation an application-level "please leave" would need,
//! so the *consumer's* daemon — the one under test here — discovers the
//! loss the same way it would discover a cable pulled out from under it: an
//! I/O error on its own read of a connection it never asked to end.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
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
    FrameKind, FrameLimits, MachineName, Metadata, NodeEvent, NodeHandshake, NodeId, NodeRequest,
    OutputPayload, Role,
};
use tokio::io::DuplexStream;
use tokio::net::TcpStream;

/// How long any single wait may take before the test calls the cluster
/// stuck. Generous: this test also spends real wall-clock time waiting out
/// `PEER_RECONCILE_INTERVAL` (500 ms), on top of everything `cluster_m2.rs`
/// already waits for.
const DEADLINE: Duration = Duration::from_secs(30);

/// One slice of both daemons' event loops — also the pace one payload is
/// sent at, so "continuous traffic" means one message per pump.
const SLICE: Duration = Duration::from_millis(20);

/// The queue depth `detect/frames` declares. Draining every slice keeps
/// ordinary queue-policy drops out of this test's loss accounting, so what
/// is left over is attributable to the outage alone.
const QUEUE_SIZE: usize = 8;

/// How many payloads to send before the cut — enough to prove traffic was
/// really flowing, not just set up.
const PRE_CUT_SENDS: u64 = 15;

/// How many payloads to send while the link is down — comfortably more than
/// `PEER_RECONCILE_INTERVAL / SLICE` (25) so the automatic backstop has time
/// to fire while traffic keeps coming.
const DURING_OUTAGE_SENDS: u64 = 40;

/// How many payloads to send after `InputRecovered` — every one of these
/// must arrive, or recovery was not real.
const POST_RECOVERY_SENDS: u64 = 15;

fn token() -> AuthToken {
    AuthToken::from_bytes([0x5A; 32])
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).expect("a legal node id")
}

fn data(name: &str) -> DataId {
    DataId::new(name).expect("a legal port id")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("as-failover-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// One producer on `robot-a`, one consumer on `robot-b`, a small bounded
/// queue so ordinary overflow behaviour stays in scope.
const LOAD_PIPELINE: &str = "\
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

async fn cluster_daemon(name: &str, machine: &str, coordinator: SocketAddr) -> Daemon {
    let config = DaemonConfig::new(RuntimePaths::under(scratch(name)))
        .with_listen(ListenConfig::none())
        .with_shm(false)
        .with_auth(token())
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

async fn pump_pair(first: &mut Daemon, second: &mut Daemon) {
    tokio::join!(first.pump(SLICE), second.pump(SLICE));
}

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

/// A payload naming its own send sequence number, so `detect`'s receipts can
/// be checked for completeness and order without any side channel.
fn seq_payload(seq: u64) -> Vec<u8> {
    seq.to_le_bytes().to_vec()
}

fn decode_seq(bytes: &[u8]) -> u64 {
    let array: [u8; 8] = bytes.try_into().expect("an 8-byte sequence payload");
    u64::from_le_bytes(array)
}

/// What one call to [`traffic_round`] observed.
#[derive(Default)]
struct RoundOutcome {
    input_closed: bool,
    input_recovered: bool,
}

/// Sends one payload from `camera`, pumps both daemons one slice, and drains
/// every event `detect` has ready — recording delivered sequence numbers
/// into `received` and flagging `InputClosed`/`InputRecovered` if either
/// arrived. `detect` re-issues `NextEvent` every round so the queue is
/// drained continuously (§11.2 — this keeps ordinary overflow out of the
/// loss this test attributes to the outage).
async fn traffic_round(
    alpha: &mut Daemon,
    beta: &mut Daemon,
    camera: &mut FakeNode,
    detect: &mut FakeNode,
    seq: u64,
    received: &mut Vec<u64>,
) -> RoundOutcome {
    camera
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::Inline {
                bytes: seq_payload(seq),
            },
        })
        .await;
    detect
        .send(NodeRequest::NextEvent {
            timeout: None,
            max_batch: QUEUE_SIZE as u32,
        })
        .await;
    pump_pair(alpha, beta).await;

    let mut outcome = RoundOutcome::default();
    while let Some(event) = detect.try_recv().await {
        match event {
            NodeEvent::Input { id, payload, .. } if id == data("frames") => {
                received.push(decode_seq(&payload));
            }
            NodeEvent::InputClosed { id, .. } if id == data("frames") => {
                outcome.input_closed = true;
            }
            NodeEvent::InputRecovered { id, .. } if id == data("frames") => {
                outcome.input_recovered = true;
            }
            _ => {}
        }
    }
    outcome
}

/// §12 conformance zoo, scenario 16/16: continuous traffic survives a hard
/// peer-socket drop with bounded loss, an automatic reconnect, and no
/// deadlock or panic anywhere in the pump loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn under_continuous_traffic_a_hard_dropped_peer_socket_reconnects_with_bounded_loss() {
    let cluster = Cluster::start().await;
    let mut alpha = cluster_daemon("lf-a", "robot-a", cluster.addr).await;
    let mut beta = cluster_daemon("lf-b", "robot-b", cluster.addr).await;
    let mut cli = CliActor::connect(cluster.addr).await;

    let registered = pump_until(&mut alpha, &mut beta, |_, _| {
        cluster.coordinator.daemons().len() == 2
    })
    .await;
    assert!(
        registered,
        "both daemons must register with the coordinator"
    );

    let reply = cli
        .request(ControlRequest::Start {
            source: DataflowSource::Manifest {
                yaml: LOAD_PIPELINE.to_owned(),
                working_dir: None,
            },
            name: Some("failover-under-load".to_owned()),
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
                .is_some_and(|state| state.node(&node("detect")).is_some())
    })
    .await;
    assert!(dispatched, "the spawn dispatch must fan out by machine");

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

    let beta_id = beta.config().id().clone();
    assert!(
        alpha.peers().is_connected(&beta_id),
        "the producer's daemon must hold the link this test is about to drop"
    );

    let mut received: Vec<u64> = Vec::new();
    let mut seq = 0u64;

    // ---- phase A: traffic flows normally, before any cut ---------------
    for _ in 0..PRE_CUT_SENDS {
        let outcome = traffic_round(
            &mut alpha,
            &mut beta,
            &mut camera,
            &mut detect,
            seq,
            &mut received,
        )
        .await;
        assert!(
            !outcome.input_closed && !outcome.input_recovered,
            "no closure or recovery event before the link is ever touched"
        );
        seq += 1;
    }
    let pre_cut_sent = seq;
    assert!(
        received.len() >= (PRE_CUT_SENDS as usize) / 2,
        "traffic must have been genuinely flowing before the cut: {} of {} arrived",
        received.len(),
        PRE_CUT_SENDS
    );

    // ---- the hard drop ---------------------------------------------------
    // Closes the raw socket from the *producer's* side; the *consumer's*
    // daemon (beta, under test below) discovers this the way it would
    // discover a severed cable — an I/O failure on a read it never asked to
    // end, not a negotiated goodbye (see this file's module doc).
    alpha
        .disconnect_peer(&beta_id, "scenario 16: hard-drop under load")
        .await;

    // ---- phase B: traffic keeps coming while the link is down -----------
    let mut input_closed = false;
    let mut input_recovered = false;
    for _ in 0..DURING_OUTAGE_SENDS {
        let outcome = traffic_round(
            &mut alpha,
            &mut beta,
            &mut camera,
            &mut detect,
            seq,
            &mut received,
        )
        .await;
        input_closed |= outcome.input_closed;
        input_recovered |= outcome.input_recovered;
        seq += 1;
        if input_recovered {
            break;
        }
    }
    assert!(
        input_closed,
        "the consumer's daemon must observe InputClosed once the socket dies under it"
    );

    // ---- phase C: keep sending until the automatic backstop reconnects --
    // `reconcile_peer` re-dials on its own timer (§12) — nothing here calls
    // a "reconnect" verb, exactly as production recovers.
    let recovery_deadline = Instant::now() + DEADLINE;
    while !input_recovered && Instant::now() < recovery_deadline {
        let outcome = traffic_round(
            &mut alpha,
            &mut beta,
            &mut camera,
            &mut detect,
            seq,
            &mut received,
        )
        .await;
        input_recovered |= outcome.input_recovered;
        seq += 1;
    }
    assert!(
        input_recovered,
        "the peer link must reconnect and the consumer must see InputRecovered \
         within {DEADLINE:?} — a hang here is exactly the deadlock this scenario \
         guards against"
    );
    let recovery_baseline = seq;

    // ---- phase D: full, reliable delivery resumes ------------------------
    for _ in 0..POST_RECOVERY_SENDS {
        traffic_round(
            &mut alpha,
            &mut beta,
            &mut camera,
            &mut detect,
            seq,
            &mut received,
        )
        .await;
        seq += 1;
    }
    let total_sent = seq;

    // A few more rounds with nothing new to send, purely to let any
    // in-flight post-recovery payload catch up before the final count.
    for _ in 0..8 {
        detect
            .send(NodeRequest::NextEvent {
                timeout: None,
                max_batch: QUEUE_SIZE as u32,
            })
            .await;
        pump_pair(&mut alpha, &mut beta).await;
        while let Some(event) = detect.try_recv().await {
            if let NodeEvent::Input { id, payload, .. } = event
                && id == data("frames")
            {
                received.push(decode_seq(&payload));
            }
        }
    }

    // ---- the loss bound ----------------------------------------------
    let distinct: BTreeSet<u64> = received.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        received.len(),
        "every delivered sequence number must be distinct — a duplicate would mean \
         the reconnect replayed something rather than resuming cleanly: {received:?}"
    );
    assert!(
        received.len() < total_sent as usize,
        "the outage must have actually cost something, or this test proves nothing: \
         {} delivered of {total_sent} sent",
        received.len()
    );
    // Full recovery, not partial: every payload sent after `InputRecovered`
    // fired must have arrived — the bound the queue policy actually owes an
    // application once a route is healthy again.
    let missing_after_recovery: Vec<u64> = (recovery_baseline..total_sent)
        .filter(|s| !distinct.contains(s))
        .collect();
    assert!(
        missing_after_recovery.is_empty(),
        "every payload sent after InputRecovered must be delivered — missing {missing_after_recovery:?} \
         out of {recovery_baseline}..{total_sent}"
    );
    // And the pre-cut traffic that this test already confirmed arrived
    // (phase A's own assertion) is still present — recovery must not have
    // discarded history the outage did not touch.
    let pre_cut_received = distinct.iter().filter(|&&s| s < pre_cut_sent).count();
    assert!(
        pre_cut_received > 0,
        "pre-cut deliveries must survive the whole test, not just phase A's own check"
    );

    let route_reestablished =
        alpha.peers().routes().established_count() > 0 || beta.peers().routes().inbound_count() > 0;
    assert!(
        route_reestablished,
        "InputRecovered fired but the route table disagrees — alpha={:?} beta={:?}",
        alpha.peers().routes().established_count(),
        beta.peers().routes().inbound_count()
    );

    cluster.shutdown().await;
}
