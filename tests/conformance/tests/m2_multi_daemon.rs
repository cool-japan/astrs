//! **Milestone M2**, half one (blueprint §21): the two-daemon cluster.
//!
//! One coordinator, two daemons with different machine names, and the
//! committed `examples/multi-daemon-cluster` graph split across both — with
//! **real node processes**, spawned by the daemon that was given their half of
//! the graph.
//!
//! ```text
//!              ┌──────── CoordinatorServer (port 0, own task) ────────┐
//!   CliActor ─►│  Start{manifest} ─► placement ─► CoordinatorEvent ──►│
//!              └───────▲──────────────────────────────────┬───────────┘
//!                      │ DaemonEvent             CoordinatorEvent
//!               ┌──────┴──────┐                     ┌──────▼──────┐
//!               │  daemon A   │   readings (peer)   │  daemon B   │
//!               │ robot-a     │════════════════════►│ robot-b     │
//!               │ sensor      │◄════════════════════│ planner     │
//!               │ checker     │   commands (peer)   │             │
//!               └──────┬──────┘                     └──────┬──────┘
//!                UDS listener                        UDS listener
//!                      ▼                                   ▼
//!            cluster-sensor, cluster-checker           cluster-planner
//!                  (real OS processes)                (real OS process)
//! ```
//!
//! # What is real, and the one thing that is not
//!
//! Real: both control legs and the peer leg over loopback TCP, the
//! `Hello`/`Welcome` handshake on each, the placement resolver, the spawn
//! dispatch, three node processes on two Unix sockets, and the committed
//! manifest — staged only to rewrite `path:` onto the binaries cargo built.
//! Not real: the machine boundary itself. Neither daemon, neither
//! coordinator, nor any node has a way to tell.
//!
//! # Why the daemons are pumped by hand
//!
//! [`astrs_daemon::Daemon::run`] would hold `&mut Daemon` for its whole life,
//! and this test has to *look* at each daemon between steps — which node did
//! it spawn, is a peer route established, is it still open after the dataflow
//! ended. [`astrs_daemon::Daemon::pump`] runs the same merged event loop for a
//! budget and hands the daemon back, so a request runs on its own task while
//! the daemons are pumped underneath it.
//!
//! | Test | M2 evidence |
//! |---|---|
//! | `a_split_dataflow_crosses_the_machine_boundary_twice` | the flagship: placement, two peer routes, the in-graph verdict, a clean finish |
//! | `each_daemon_is_told_only_about_its_own_half` | a daemon receives its own nodes, not the whole graph |
//! | `a_graph_naming_an_absent_machine_spawns_nothing_anywhere` | an unplaceable graph waits; it never runs on the wrong machine |

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use astrs_conformance::{Fixture, StageOptions, make_scratch_dir, result_file, stage_example};
use astrs_coordinator::{Coordinator, CoordinatorConfig, CoordinatorServer};
use astrs_daemon::coordinator::UplinkConfig;
use astrs_daemon::{Daemon, DaemonConfig, ListenConfig, PeerConfig, RuntimePaths};
use astrs_transport::{ConnectionCounters, FramedStream, HandshakeParams, LocalIdentity, initiate};
use astrs_wire::{
    AuthToken, ControlReply, ControlRequest, DataflowId, DataflowSource, DataflowStatus,
    FeatureFlags, FrameKind, FrameLimits, MachineName, NodeId, Role,
};
use multi_daemon_cluster::{CrossingReport, DEFAULT_READINGS, ENV_REPORT_PATH};
use tokio::net::TcpStream;

/// The example whose committed files this file drives.
const EXAMPLE: &str = "multi-daemon-cluster";

/// How long any single wait may take before the test calls the cluster stuck.
///
/// Generous enough that a loaded machine never fails spuriously, short enough
/// that a genuinely wedged exchange fails the test rather than hanging the
/// suite. Every wait here is deadline-polled; nothing sleeps for a fixed time
/// and then assumes.
const DEADLINE: Duration = Duration::from_secs(60);

/// One slice of both daemons' event loops.
///
/// Short: a shorter slice is a finer interleaving between the two daemons and
/// the coordinator's own task.
const SLICE: Duration = Duration::from_millis(20);

/// The machine the sensor and the checker are pinned to.
const MACHINE_A: &str = "robot-a";

/// The machine the planner is pinned to.
const MACHINE_B: &str = "robot-b";

/// The cluster token every leg authenticates with (§16).
fn token() -> AuthToken {
    AuthToken::from_bytes([0x5A; 32])
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).expect("a legal node id")
}

fn machine(name: &str) -> MachineName {
    MachineName::new(name).expect("a legal machine name")
}

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
        // The heartbeat budget is deliberately far longer than the test takes:
        // a coordinator removes a daemon after `interval × limit` of silence
        // (§12), and these daemons only beat while they are being pumped. The
        // watchdog has its own tests, next to the code that runs it.
        let config = CoordinatorConfig::new(token())
            .with_port(0)
            .with_heartbeat(Duration::from_millis(500), 600);
        let coordinator = Coordinator::open_in_memory(config).expect("an in-memory store");
        let server = CoordinatorServer::bind(coordinator.clone())
            .await
            .expect("a bound coordinator");
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
// The daemons
// ---------------------------------------------------------------------

/// Builds one cluster daemon: its own runtime directory and Unix socket (so
/// real node processes can attach), a peer listener on an ephemeral loopback
/// port, and an uplink to `coordinator`.
async fn cluster_daemon(machine_name: &str, coordinator: SocketAddr, working_dir: &Path) -> Daemon {
    // `make_scratch_dir` is terse on purpose: the node socket lives inside it
    // and a Unix socket path has a hard length limit (104 bytes on Darwin).
    let paths = RuntimePaths::under(make_scratch_dir().expect("a runtime directory"));
    // The Unix socket only. `ListenConfig::defaults` would also open the §4.2
    // loopback TCP fallback on the fixed port 7408, which two daemons in one
    // process — or a developer's own daemon — cannot share.
    let listen = ListenConfig::uds(paths.socket_path());
    let config = DaemonConfig::new(paths)
        .with_listen(listen)
        .with_auth(token())
        .with_working_dir(working_dir)
        // §24.2's 5 s default is a robot's cadence, not a test's: these
        // daemons only beat while they are being pumped.
        .with_heartbeat_interval(Duration::from_millis(100))
        .with_metrics_interval(Duration::from_millis(100))
        .with_machine(machine(machine_name))
        .with_peer(PeerConfig::new(token()).with_loopback(0));
    let mut daemon = Daemon::new(config).expect("a daemon");
    daemon.bind().await.expect("the node listener binds");

    let uplink = UplinkConfig::new(coordinator, token())
        .with_machine(machine(machine_name))
        .with_label("zone", machine_name)
        .with_backoff(Duration::from_millis(20), Duration::from_millis(100))
        .with_dial_timeout(Duration::from_millis(500));
    daemon
        .connect_coordinator(uplink)
        .await
        .expect("the uplink starts");
    daemon
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
    tokio::time::timeout(SLICE * 8, task)
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

/// Stages the committed manifest onto built binaries and a per-run verdict
/// file.
fn stage_cluster(report: &PathBuf) -> Fixture {
    let _ = std::fs::remove_file(report);
    let options = StageOptions::new(EXAMPLE).with_env_path(ENV_REPORT_PATH, report);
    stage_example(EXAMPLE, &options).expect("the cluster graph stages")
}

/// Starts a staged manifest on the cluster, returning its dataflow id.
async fn start_dataflow(cli: &mut CliActor, fixture: &Fixture, name: &str) -> DataflowId {
    let yaml = fixture.text().expect("the staged manifest is readable");
    let reply = cli
        .request(ControlRequest::Start {
            source: DataflowSource::Manifest {
                yaml,
                working_dir: Some(fixture.dir.display().to_string()),
            },
            name: Some(name.to_owned()),
            detach: true,
        })
        .await;
    match reply {
        ControlReply::Started { dataflow, .. } => dataflow,
        other => panic!("start refused: {other:?}"),
    }
}

/// Polls `Check` on its own connection until the dataflow's verdict is
/// terminal, then returns it.
fn poll_until_terminal(
    addr: SocketAddr,
    dataflow: DataflowId,
) -> tokio::task::JoinHandle<ControlReply> {
    tokio::spawn(async move {
        let mut cli = CliActor::connect(addr).await;
        let deadline = Instant::now() + DEADLINE;
        loop {
            let reply = cli
                .request(ControlRequest::Check {
                    dataflow: Some(dataflow),
                })
                .await;
            if matches!(reply, ControlReply::DataflowResult { .. }) || Instant::now() >= deadline {
                return reply;
            }
            tokio::time::sleep(SLICE).await;
        }
    })
}

/// The checker's verdict, read back from the file it wrote.
fn read_verdict(path: &PathBuf) -> CrossingReport {
    let json = std::fs::read_to_string(path).unwrap_or_else(|error| {
        panic!(
            "the checker wrote no verdict at {}: {error}",
            path.display()
        )
    });
    serde_json::from_str(&json).expect("a readable verdict")
}

// ---------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------

/// The flagship M2 proof: the committed graph, split by `deploy.machine`
/// across two daemons, delivering data over a peer route in **both**
/// directions, with the crossing asserted from inside the graph and a clean
/// finish afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_split_dataflow_crosses_the_machine_boundary_twice() {
    let verdict_path = result_file("cluster-verdict.json");
    let fixture = stage_cluster(&verdict_path);

    let cluster = Cluster::start().await;
    let mut alpha = cluster_daemon(MACHINE_A, cluster.addr, &fixture.dir).await;
    let mut beta = cluster_daemon(MACHINE_B, cluster.addr, &fixture.dir).await;
    let mut cli = CliActor::connect(cluster.addr).await;

    await_registration(&cluster, &mut alpha, &mut beta).await;
    let dataflow = start_dataflow(&mut cli, &fixture, "m2-cluster").await;

    // ---- placement -----------------------------------------------------
    let dispatched = pump_until(&mut alpha, &mut beta, |a, b| {
        a.dataflow(dataflow)
            .is_some_and(|state| state.node(&node("sensor")).is_some())
            && a.dataflow(dataflow)
                .is_some_and(|state| state.node(&node("checker")).is_some())
            && b.dataflow(dataflow)
                .is_some_and(|state| state.node(&node("planner")).is_some())
    })
    .await;
    assert!(
        dispatched,
        "the spawn dispatch must fan out by `deploy.machine`"
    );

    // ---- the peer route ------------------------------------------------
    let routed = pump_until(&mut alpha, &mut beta, |a, b| {
        a.peers().routes().established_count() > 0
            || b.peers().routes().established_count() > 0
            || a.peers().routes().inbound_count() > 0
            || b.peers().routes().inbound_count() > 0
    })
    .await;
    assert!(
        routed,
        "a producer and consumer on different daemons must get a peer route (§6.4)"
    );
    assert_eq!(
        alpha.peer_directory().len(),
        1,
        "one peer, from one directive"
    );
    assert_eq!(beta.peer_directory().len(), 1);

    // ---- the run ------------------------------------------------------
    let reply = pump_while(
        &mut alpha,
        &mut beta,
        poll_until_terminal(cluster.addr, dataflow),
    )
    .await;
    // Read before asserting: when the run *did* fail, the checker's own
    // verdict is the only artefact that says why, and a failure message
    // without it costs a whole re-run to reproduce.
    let verdict_text = std::fs::read_to_string(&verdict_path)
        .unwrap_or_else(|error| format!("(no verdict written: {error})"));
    match reply {
        ControlReply::DataflowResult { result } => {
            assert_eq!(
                result.status,
                DataflowStatus::Finished,
                "the split dataflow did not finish: {result:?}\nverdict: {verdict_text}"
            );
            assert!(!result.has_failures(), "a node failed: {result:?}");
            assert_eq!(
                result.node_results.len(),
                3,
                "every node on both machines must be accounted for: {result:?}"
            );
        }
        other => panic!("expected a DataflowResult, got {other:?}"),
    }

    // ---- the in-graph verdict ------------------------------------------
    let verdict = read_verdict(&verdict_path);
    assert!(
        verdict.is_clean(),
        "the checker's verdict is not clean: {verdict:?}"
    );
    assert_eq!(verdict.received, DEFAULT_READINGS, "{verdict:?}");
    assert!(
        verdict.crossed_twice(),
        "the route must cross a machine boundary twice: {verdict:?}"
    );
    assert_eq!(verdict.sensor_machine, MACHINE_A, "{verdict:?}");
    assert_eq!(verdict.planner_machine, MACHINE_B, "{verdict:?}");
    assert_eq!(verdict.checker_machine, MACHINE_A, "{verdict:?}");
    // §8.3's `deploy.labels` rode the whole way: manifest → coordinator →
    // spawn spec → the node's own descriptor.
    assert_eq!(
        verdict.checker_labels.get("role").map(String::as_str),
        Some("sensing"),
        "{verdict:?}"
    );

    // ---- clean shutdown -------------------------------------------------
    alpha.begin_shutdown();
    beta.begin_shutdown();
    let torn_down = pump_until(&mut alpha, &mut beta, |a, b| {
        a.peers().routes().established_count() == 0
            && b.peers().routes().established_count() == 0
            && a.peers().routes().inbound_count() == 0
            && b.peers().routes().inbound_count() == 0
    })
    .await;
    assert!(
        torn_down,
        "shutting down must tear the peer routes down rather than leaking them"
    );
    assert!(alpha.state().is_shutting_down() && beta.state().is_shutting_down());

    cluster.shutdown().await;
    let _ = std::fs::remove_file(&verdict_path);
    fixture.clean();
}

/// A daemon is told about its own nodes, not about the whole graph — the
/// property that makes a split dataflow a *distributed* one rather than two
/// copies of the same thing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_daemon_is_told_only_about_its_own_half() {
    let verdict_path = result_file("cluster-halves.json");
    let fixture = stage_cluster(&verdict_path);

    let cluster = Cluster::start().await;
    let mut alpha = cluster_daemon(MACHINE_A, cluster.addr, &fixture.dir).await;
    let mut beta = cluster_daemon(MACHINE_B, cluster.addr, &fixture.dir).await;
    let mut cli = CliActor::connect(cluster.addr).await;

    await_registration(&cluster, &mut alpha, &mut beta).await;
    let dataflow = start_dataflow(&mut cli, &fixture, "m2-halves").await;

    let dispatched = pump_until(&mut alpha, &mut beta, |a, b| {
        a.dataflow(dataflow)
            .is_some_and(|state| state.node(&node("sensor")).is_some())
            && b.dataflow(dataflow)
                .is_some_and(|state| state.node(&node("planner")).is_some())
    })
    .await;
    assert!(dispatched);

    assert!(
        alpha
            .dataflow(dataflow)
            .is_some_and(|state| state.node(&node("planner")).is_none()),
        "robot-a was given robot-b's node"
    );
    for absent in ["sensor", "checker"] {
        assert!(
            beta.dataflow(dataflow)
                .is_some_and(|state| state.node(&node(absent)).is_none()),
            "robot-b was given robot-a's `{absent}`"
        );
    }

    // Both halves belong to one dataflow, under one id, on both daemons.
    assert!(alpha.dataflow(dataflow).is_some());
    assert!(beta.dataflow(dataflow).is_some());

    // Stopping is one request that reaches both, without stopping either
    // daemon (§12).
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
    assert!(!alpha.state().is_shutting_down() && !beta.state().is_shutting_down());

    cluster.shutdown().await;
    let _ = std::fs::remove_file(&verdict_path);
    fixture.clean();
}

/// A graph naming a machine no daemon claims never runs on the wrong one.
///
/// The invariant, not the mechanism: this build **refuses** the `Start`
/// outright (`no daemon registered for machine "robot-b"`), and a build that
/// instead left the dataflow `Pending` until that robot booted would be an
/// equally legitimate design — a fleet where one machine starts late should
/// not have to re-issue the command. What must hold either way, and what is
/// asserted here, is that no node of the graph is spawned on a daemon that
/// was not asked for. A typo in `deploy.machine` therefore costs a dataflow
/// that visibly never starts, never a silent single-host run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_graph_naming_an_absent_machine_spawns_nothing_anywhere() {
    let verdict_path = result_file("cluster-unplaceable.json");
    let fixture = stage_cluster(&verdict_path);

    let cluster = Cluster::start().await;
    // One daemon, claiming `robot-a`. The committed manifest also asks for
    // `robot-b`, which nothing here provides.
    let mut alpha = cluster_daemon(MACHINE_A, cluster.addr, &fixture.dir).await;
    let mut idle = cluster_daemon("robot-c", cluster.addr, &fixture.dir).await;
    let mut cli = CliActor::connect(cluster.addr).await;

    let registered = pump_until(&mut alpha, &mut idle, |_, _| {
        cluster.coordinator.daemons().len() == 2
    })
    .await;
    assert!(registered);

    let yaml = fixture.text().expect("the staged manifest is readable");
    let addr = cluster.addr;
    let dir = fixture.dir.display().to_string();
    let started = tokio::spawn(async move {
        let mut cli = CliActor::connect(addr).await;
        cli.request(ControlRequest::Start {
            source: DataflowSource::Manifest {
                yaml,
                working_dir: Some(dir),
            },
            name: Some("m2-unplaceable".to_owned()),
            detach: true,
        })
        .await
    });
    let reply = pump_while(&mut alpha, &mut idle, started).await;

    let started_as = match reply {
        // What this build does: name the machine it could not resolve.
        ControlReply::Error { ref message, .. } => {
            assert!(
                message.contains(MACHINE_B),
                "a refusal must name the machine it could not place: {message}"
            );
            None
        }
        ControlReply::Started { dataflow, .. } => Some(dataflow),
        ref other => panic!("expected a refusal or a deferral, got {other:?}"),
    };

    // Pump well past the point a placeable graph would have been dispatched:
    // the flagship test above sees its nodes on both daemons within a few
    // slices, so a generous run of them here is a strong "nothing happened".
    for _ in 0..64 {
        pump_pair(&mut alpha, &mut idle).await;
    }
    for daemon in [&alpha, &idle] {
        assert!(
            daemon.state().dataflows().next().is_none(),
            "a graph that could not be placed put nodes on a daemon anyway"
        );
    }
    if let Some(dataflow) = started_as {
        let list = cli.request(ControlRequest::List { all: true }).await;
        match list {
            ControlReply::DataflowList { dataflows, .. } => {
                let entry = dataflows
                    .iter()
                    .find(|summary| summary.id == dataflow)
                    .expect("a deferred dataflow is still listed");
                assert_eq!(entry.running_nodes, 0, "{entry:?}");
            }
            other => panic!("expected a DataflowList, got {other:?}"),
        }
    }

    // The coordinator is still answering: an unplaceable graph is not a
    // broken cluster.
    let list = cli.request(ControlRequest::List { all: true }).await;
    assert!(matches!(list, ControlReply::DataflowList { .. }));

    cluster.shutdown().await;
    let _ = std::fs::remove_file(&verdict_path);
    fixture.clean();
}
