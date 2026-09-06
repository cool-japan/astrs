//! §12 conformance zoo, scenario 15/16 (daemon-killed-mid-route) —
//! blueprint §4.2, §12, §20.3.
//!
//! `SIGKILL`s a **real daemon process** — not a node child, not the
//! in-process `Daemon` object `cluster_m2.rs`'s own tests pump by hand —
//! while continuous traffic is crossing the peer route it hosts one end of,
//! then restarts it and proves the cluster comes back.
//!
//! ```text
//!   real coordinator (in-process, real TCP)
//!            ▲                          ▲
//!   real `astrs daemon` A       real `astrs daemon` B
//!    (machine robot-a)           (machine robot-b)
//!            ▲                          ▲
//!    node-api client            node-api client
//!    "producer" (real UDS)      "consumer" (real UDS)
//!            │                          │
//!            └──── continuous traffic ──┘
//!
//!   SIGKILL daemon A ──► consumer sees InputClosed (peer socket dies)
//!                    ──► coordinator's control-leg socket to A dies too,
//!                        noticed the same way: the kernel closes every fd
//!                        of a killed process, so the coordinator's own
//!                        read of that connection fails almost immediately
//!                        — the heartbeat watchdog's sweep-timeout path
//!                        (§12; `DaemonRegistry::sweep_heartbeats`) is the
//!                        backstop for a daemon that goes silent *without*
//!                        its socket dying (a black-holed network), and is
//!                        not what actually fires for a killed process, but
//!                        it is what this test's own budget is sized
//!                        against (see [`Cluster::start`]) precisely
//!                        because a real daemon's default heartbeat cadence
//!                        must never let the watchdog *also* catch the
//!                        survivor
//!   restart daemon A ──► re-registers under `robot-a` (fresh identity —
//!                        see "What this build does not do" below)
//!                    ──► a FRESH dataflow through the restarted daemon
//!                        proves the cluster, not just the process, is
//!                        healthy again
//! ```
//!
//! # Not scenario 16
//!
//! `transport_failover_under_load.rs` hard-drops the *socket* between two
//! daemon **objects** that keep running the whole time; nothing there ever
//! kills a process. This file is the other half: the daemon **process**
//! itself dies, taking every route and every node it hosted with it — a
//! strictly larger blast radius, and the two must not be conflated (the
//! task brief that produced this file says so explicitly).
//!
//! Also not `cluster_m2.rs::a_peer_partition_closes_the_input_and_its_repair_recovers_it`,
//! which cuts the link between two in-process `Daemon` objects that are
//! never processes to begin with — there is no pid there for `SIGKILL` to
//! name.
//!
//! # What this build does not do — read before trusting "routes re-establish"
//!
//! A restarted `astrs daemon` gets a **fresh** [`astrs_wire::DaemonId`]
//! (`crates/astrs-daemon/src/config/mod.rs`'s `DaemonConfig::new` calls
//! `DaemonId::generate(None)` unconditionally; nothing in `astrs daemon`'s
//! own CLI wiring persists or recovers a prior one across a process
//! restart). And `CoordinatorEvent::StateCatchUp`'s entries
//! (`astrs_wire::messages::coordinator_daemon::types::StateEntryKind`) are
//! `DataflowStatus`/`NodeState`/`ParamSet`/`ParamDeleted` facts — never a
//! `Spawn`/placement directive — while
//! `handlers::lifecycle::handle_daemon_disconnected` only releases barriers
//! a `--attach` might be waiting on; neither it nor anything reachable from
//! a fresh daemon's own `register()` re-dispatches a dead daemon's nodes to
//! whoever next claims its machine name.
//!
//! So the *original* dataflow's `camera` does not come back — nothing in
//! this build promises it would, and asserting otherwise here would be
//! testing a design decision this codebase has not made. What "routes
//! re-establish" is proven as instead: the coordinator's registry shows the
//! restarted daemon back under `robot-a`, and a **second, fresh** dataflow
//! placed across `robot-a` (the restarted process) and `robot-b` (the
//! survivor, never touched) opens a real peer route and carries real data —
//! the cluster is usable again, through the daemon that came back, not
//! merely "a process exists".
//!
//! # StateCatchUp happens; this test does not watch it happen
//!
//! The restarted daemon's `register()` really does receive an initial
//! `StateCatchUp` batch — `astrs-coordinator`'s own session task pushes one
//! right after *every* `Register`, first-ever or reconnect alike (see
//! `crates/astrs-coordinator/src/session/daemon.rs`'s own test commentary:
//! "the initial `StateCatchUp` batch `run` pushes right after `Register`"),
//! and the daemon side of it
//! (`crates/astrs-daemon/src/coordinator/apply.rs::apply_state_catch_up`)
//! logs `tracing::debug!(seq, applied, final_batch, "applied a state
//! catch-up batch")` and answers `StateCatchUpAck`. This test asserts none
//! of that, and — within the files this task authorized — cannot:
//!
//! - the daemon's own log is not an option. `astrs daemon`'s CLI wiring
//!   (`bins/astrs-cli/src/command/serve.rs::daemon`) installs no `tracing`
//!   subscriber at all, unlike `coordinator`'s neighbouring
//!   `install_trace_sink`; the debug line above never reaches this file's
//!   own `daemon.std{out,err}.log` regardless of `RUST_LOG`, because there
//!   is nothing subscribed to emit it anywhere;
//! - the coordinator side is not an option either, even though the
//!   coordinator is in-process and directly reachable here: neither its
//!   public registry (`astrs_coordinator::registry::daemon::DaemonHandle`,
//!   the type behind [`Cluster::hosts`]) nor the wire-level
//!   `ControlRequest::ConnectedDaemons` / `astrs_wire::DaemonInfo` a real
//!   `astrs list --daemons` would use carries a catch-up sequence or any
//!   other sync marker a test could poll.
//!
//! Closing that gap is a `bins/astrs-cli` / `astrs-coordinator` /
//! `astrs-wire` change, not a test change — every file it would touch is
//! outside this task's `tests/**` scope. Named here rather than silently
//! omitted: "a restarted daemon … resyncs (StateCatchUp)" was one of this
//! scenario's four asked-for assertions, and this build genuinely has
//! nothing observable to assert it against.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(unix)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use astrs_coordinator::{Coordinator, CoordinatorConfig, CoordinatorServer};
use astrs_daemon::RuntimePaths;
use astrs_node_api::node::builder::NodeBuilder;
use astrs_node_api::{Event, EventStream, Node};
use astrs_transport::{ConnectionCounters, FramedStream, HandshakeParams, LocalIdentity, initiate};
use astrs_wire::{
    AuthToken, ControlReply, ControlRequest, DataflowId, DataflowSource, FeatureFlags, FrameKind,
    FrameLimits, MachineName, Role,
};
use tokio::net::TcpStream;

/// How long any single "wait for the cluster to react" loop may take before
/// the test calls it stuck.
const SETTLE: Duration = Duration::from_secs(20);

/// How long the wait for the coordinator's heartbeat watchdog to declare a
/// silent daemon lost may take. Bounded by [`HEARTBEAT_INTERVAL`] ×
/// [`MISSED_HEARTBEAT_LIMIT`] × 2 (see [`cluster`]'s doc for the arithmetic)
/// plus generous scheduling slack — a real daemon process's own heartbeat
/// cadence is the §24.2 production default (5 s), not a test's, and this
/// budget must stay comfortably above it or the *survivor* gets swept too.
const LOSS_DEADLINE: Duration = Duration::from_secs(45);

/// One polling step for a deadline-polled wait.
const STEP: Duration = Duration::from_millis(100);

fn token() -> AuthToken {
    AuthToken::from_bytes([0x5A; 32])
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("as-killroute-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// The `astrs` binary this workspace built, found the way
/// `tests/conformance`'s own `paths::binary` does: `CARGO_BIN_EXE_<name>`
/// only exists for a binary target of the *same* package
/// (`bins/astrs-cli`'s own tests get it because they *are* that package),
/// and `crates/astrs-daemon` is a different one — so this walks one
/// directory up from this test binary's own path instead, exactly as
/// `tests/conformance/src/paths.rs`'s module doc explains.
fn astrs_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("this test binary's own path");
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("<target>/<profile>/deps/<test> has two parents");
    for candidate in [profile_dir.join("astrs"), profile_dir.join("astrs.exe")] {
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "no `astrs` binary under {}; run `cargo build -p astrs-cli` first",
        profile_dir.display()
    );
}

/// Polls `condition` until it holds or `deadline` passes, sleeping [`STEP`]
/// between checks.
async fn eventually(what: &str, deadline: Duration, mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + deadline;
    loop {
        if condition() {
            return;
        }
        assert!(Instant::now() < until, "timed out waiting for {what}");
        tokio::time::sleep(STEP).await;
    }
}

// ---------------------------------------------------------------------
// The coordinator — real TCP, in-process, never killed
// ---------------------------------------------------------------------

struct Cluster {
    addr: SocketAddr,
    coordinator: Coordinator,
    handle: astrs_coordinator::ServerHandle,
    task: tokio::task::JoinHandle<astrs_coordinator::Result<()>>,
}

impl Cluster {
    /// A coordinator whose heartbeat budget is tuned against a **real**
    /// daemon process's production heartbeat cadence (§24.2 default: 5 s),
    /// not a test's own pumped one. `cluster_m2.rs`'s in-process daemons set
    /// `with_heartbeat_interval(100ms)` on themselves and can therefore use
    /// a coordinator budget far tighter than production; a real `astrs
    /// daemon` subprocess here has no such override (`DaemonArgs` exposes
    /// none), so this scenario's "lost" threshold must clear a real 5 s gap
    /// with real margin.
    ///
    /// `missed_heartbeat_limit` of 16 at a 500 ms sweep interval: a daemon
    /// is declared lost at `missed >= limit * 2` (§12,
    /// `DaemonRegistry::sweep_heartbeats`), so roughly `16 * 2 * 500ms` =
    /// 16 s of continuous silence — more than 3x the survivor's worst
    /// natural gap, so the watchdog can never sweep the daemon that is
    /// still alive and beating every ~5 s.
    async fn start() -> Self {
        let config = CoordinatorConfig::new(token())
            .with_port(0)
            .with_heartbeat(Duration::from_millis(500), 16);
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

    fn daemon_count(&self) -> usize {
        self.coordinator.daemons().len()
    }

    /// Whether the registry currently holds an entry registered under
    /// `machine` — a stronger check than [`Self::daemon_count`] alone,
    /// which would pass just as happily if the watchdog swept the
    /// *survivor* and left the killed daemon's stale entry behind.
    fn hosts(&self, machine: &str) -> bool {
        let name = MachineName::new(machine).expect("a legal machine name");
        self.coordinator.daemons().find_by_machine(&name).is_some()
    }

    async fn shutdown(self) {
        self.handle.shutdown();
        let _ = tokio::time::timeout(SETTLE, self.task).await;
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
        initiate(&mut stream, &params, SETTLE)
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
// The daemon subprocesses
// ---------------------------------------------------------------------

/// One real `astrs daemon` child process and the pieces a test needs to
/// reach it: its pid (for `SIGKILL`), its runtime directory (for its node
/// socket path), and its own stdout/stderr captured to files rather than a
/// pipe this test does not drain — `bins/astrs-cli/src/command/serve.rs`'s
/// own module doc explains why a long-lived child's stdout is never safe to
/// pipe without a dedicated reader.
struct DaemonProcess {
    child: Child,
    runtime_dir: PathBuf,
}

impl DaemonProcess {
    fn spawn(machine: &str, coordinator: SocketAddr, tag: &str) -> Self {
        let runtime_dir = scratch(&format!("rt-{tag}"));
        let working_dir = scratch(&format!("wd-{tag}"));
        let stdout = std::fs::File::create(runtime_dir.join("daemon.stdout.log"))
            .expect("a writable log file");
        let stderr = std::fs::File::create(runtime_dir.join("daemon.stderr.log"))
            .expect("a writable log file");
        let child = Command::new(astrs_binary())
            .arg("daemon")
            .arg("--coordinator")
            .arg(coordinator.to_string())
            .arg("--machine")
            .arg(machine)
            .arg("--peer-port")
            .arg("0")
            .arg("--runtime-dir")
            .arg(&runtime_dir)
            .arg("--working-dir")
            .arg(&working_dir)
            .arg("--token")
            .arg(token().reveal_hex())
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .unwrap_or_else(|error| panic!("spawning `astrs daemon` for {machine}: {error}"));
        Self { child, runtime_dir }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The real Unix socket this daemon's node listener binds — computed
    /// the same way `astrs daemon`'s own CLI wiring does
    /// (`RuntimePaths::under(&runtime_dir).socket_path()` in
    /// `bins/astrs-cli/src/command/serve.rs::daemon`), so nothing here
    /// depends on parsing the child's own output.
    fn node_socket(&self) -> PathBuf {
        RuntimePaths::under(&self.runtime_dir).socket_path()
    }

    fn endpoint(&self) -> String {
        format!("uds://{}", self.node_socket().display())
    }

    /// Whether the child is still running.
    ///
    /// Deliberately **not** the `rustix`-based external-pid check
    /// `bins/astrs-cli/tests/cluster_e2e.rs::alive` uses: that helper checks
    /// a process this test's own process is *not* the parent of (`astrs
    /// up`'s CLI already exited, so the coordinator/daemon it spawned were
    /// reparented to init, which reaps zombies on its own). This test's
    /// `Command::spawn` makes the test binary itself the parent, so a killed
    /// child is a zombie — still a "valid" pid to `kill(pid, 0)` — until
    /// something calls `wait`/`try_wait` on it. [`std::process::Child::try_wait`]
    /// is that call, and it reaps as it checks.
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn kill(&self) {
        let Some(pid) = rustix::process::Pid::from_raw(self.pid().cast_signed()) else {
            panic!("pid {} is not a legal signal target", self.pid());
        };
        rustix::process::kill_process(pid, rustix::process::Signal::KILL)
            .unwrap_or_else(|error| panic!("SIGKILL on {}: {error}", self.pid()));
    }

    /// Best-effort teardown: kills the child if it is still running, then
    /// reaps it unconditionally — `alive`'s own `try_wait` already reaped it
    /// if it was not, so the final `wait` is a harmless no-op in that case.
    fn stop_if_alive(&mut self) {
        if self.alive() {
            self.kill();
        }
        let _ = self.child.wait();
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        self.stop_if_alive();
    }
}

/// Waits for a real socket file to exist and accept a connection — a daemon
/// process is spawned before its listener is necessarily bound, and dialing
/// too early is a connection-refused this test should retry past rather
/// than fail on.
async fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + SETTLE;
    loop {
        if tokio::net::UnixStream::connect(path).await.is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no daemon ever accepted a connection at {}",
            path.display()
        );
        tokio::time::sleep(STEP).await;
    }
}

// ---------------------------------------------------------------------
// The traffic — real node-api clients over real Unix sockets
// ---------------------------------------------------------------------

/// Connects a dynamic producer named `producer` to `daemon`'s real socket,
/// declaring one output.
async fn connect_producer(daemon: &DaemonProcess, dataflow: DataflowId) -> (Node, EventStream) {
    wait_for_socket(&daemon.node_socket()).await;
    NodeBuilder::new()
        .node_id("producer")
        .expect("a legal node id")
        .dataflow(dataflow)
        .dynamic(true)
        .daemon(daemon.endpoint())
        .auth(token())
        .output("image")
        .expect("a legal output id")
        .connect_async()
        .await
        .unwrap_or_else(|error| panic!("connecting the producer to {}: {error}", daemon.endpoint()))
}

/// Connects a dynamic consumer named `consumer` to `daemon`'s real socket,
/// subscribed to `frames`.
async fn connect_consumer(daemon: &DaemonProcess, dataflow: DataflowId) -> (Node, EventStream) {
    wait_for_socket(&daemon.node_socket()).await;
    NodeBuilder::new()
        .node_id("consumer")
        .expect("a legal node id")
        .dataflow(dataflow)
        .dynamic(true)
        .daemon(daemon.endpoint())
        .auth(token())
        .subscribe("frames")
        .expect("a legal input id")
        .connect_async()
        .await
        .unwrap_or_else(|error| panic!("connecting the consumer to {}: {error}", daemon.endpoint()))
}

/// The split graph one dataflow places across the two machines.
const SPLIT_PIPELINE: &str = "\
nodes:
  - id: producer
    path: dynamic
    deploy:
      machine: robot-a
    outputs: [image]
  - id: consumer
    path: dynamic
    deploy:
      machine: robot-b
    inputs:
      frames:
        source: producer/image
        queue_size: 8
";

/// Starts [`SPLIT_PIPELINE`] and returns its dataflow id.
async fn start_split_pipeline(cli: &mut CliActor, name: &str) -> DataflowId {
    let reply = cli
        .request(ControlRequest::Start {
            source: DataflowSource::Manifest {
                yaml: SPLIT_PIPELINE.to_owned(),
                working_dir: None,
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

/// §12 conformance zoo, scenario 15/16: a real daemon process, `SIGKILL`ed
/// mid-traffic, is noticed by its peer and by the coordinator, and a
/// restart brings the cluster back — see this file's module doc for exactly
/// what "back" does and does not mean in this build.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sigkilled_daemon_process_is_noticed_and_a_restart_recovers_the_cluster() {
    let cluster = Cluster::start().await;

    let mut alpha = DaemonProcess::spawn("robot-a", cluster.addr, "a");
    let mut beta = DaemonProcess::spawn("robot-b", cluster.addr, "b");

    eventually("both real daemons to register", SETTLE, || {
        cluster.daemon_count() == 2
    })
    .await;
    // Both real processes really are alive at this point, not merely
    // registered on a connection some earlier incarnation opened.
    assert!(alpha.alive() && beta.alive());

    let mut cli = CliActor::connect(cluster.addr).await;
    let dataflow = start_split_pipeline(&mut cli, "killroute").await;

    let (producer, _producer_events) = connect_producer(&alpha, dataflow).await;
    let (_consumer, mut consumer_events) = connect_consumer(&beta, dataflow).await;

    let mut producer = producer;
    let mut output = producer
        .raw_output("image")
        .expect("the producer declared this output");

    // ---- continuous traffic, started before the kill and never stopped -
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let traffic = tokio::spawn({
        let stop = std::sync::Arc::clone(&stop);
        async move {
            let mut seq: u64 = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = output.send_bytes(seq.to_le_bytes(), astrs_wire::Metadata::default());
                seq += 1;
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            // Keep the producer's own connection (and the orphan guard it
            // owns) alive for exactly as long as the traffic task runs.
            drop(producer);
            seq
        }
    });

    // Confirm traffic is genuinely flowing before touching anything.
    let mut received_before_kill = 0u64;
    let confirm_deadline = Instant::now() + SETTLE;
    while received_before_kill < 5 && Instant::now() < confirm_deadline {
        if let Some(event) = consumer_events.recv_async_timeout(STEP).await
            && matches!(event, Event::Input { .. })
        {
            received_before_kill += 1;
        }
    }
    assert!(
        received_before_kill >= 5,
        "traffic must be genuinely flowing across the route before the kill: \
         only {received_before_kill} frames arrived"
    );

    // ---- the kill: a real SIGKILL on a real daemon process --------------
    alpha.kill();
    eventually(
        "the killed daemon's process to actually exit",
        SETTLE,
        || !alpha.alive(),
    )
    .await;

    // ---- the survivor notices: InputClosed on the consumer's route ------
    let mut input_closed = false;
    let closed_deadline = Instant::now() + SETTLE;
    while !input_closed && Instant::now() < closed_deadline {
        if let Some(event) = consumer_events.recv_async_timeout(STEP).await {
            match event {
                Event::InputClosed { id, .. } if id.as_str() == "frames" => input_closed = true,
                Event::Input { .. } => {}
                _ => {}
            }
        }
    }
    assert!(
        input_closed,
        "the surviving daemon must flip the affected input to InputClosed \
         once the peer socket dies under it"
    );

    // ---- the coordinator notices and removes the dead daemon -----------
    // In practice this fires within a second or two (a killed process's
    // control-leg socket dies with it, so the coordinator's own connection
    // read fails almost immediately) rather than needing the heartbeat
    // watchdog's own sweep timeout — see this file's module doc. The
    // deadline stays sized against the watchdog's own budget regardless,
    // so the assertion is honest about the slower path this build actually
    // guarantees, not just the fast path this test happens to observe.
    // The predicate names *which* daemon left rather than merely counting:
    // `daemon_count() == 1` would pass just as happily if the watchdog swept
    // the wrong entry — the survivor's — and left the killed daemon's stale
    // registration behind, which `beta.alive()` alone would not catch either
    // (it proves the OS process lives, not that the coordinator kept the
    // right registry entry).
    eventually(
        "the coordinator to declare robot-a lost and keep robot-b",
        LOSS_DEADLINE,
        || !cluster.hosts("robot-a") && cluster.hosts("robot-b"),
    )
    .await;
    assert_eq!(
        cluster.daemon_count(),
        1,
        "no third entry appeared meanwhile"
    );
    // The survivor must not have been swept by the same watchdog — proof
    // the heartbeat budget above was tuned against its real cadence, not
    // just wide enough to let the dead one through eventually.
    assert!(
        beta.alive(),
        "the surviving daemon's own process must still be running"
    );

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let sent_before_stop = traffic.await.expect("the traffic task did not panic");
    assert!(sent_before_stop >= 5, "{sent_before_stop}");

    // ---- the restart: a fresh process, the same machine name ------------
    // A fresh runtime directory, not the dead daemon's own: its old UDS
    // socket file is still on disk (SIGKILL gave it no chance to unlink
    // it), and nothing here needs to reuse it — `DaemonId` is regenerated
    // per process regardless (see this file's module doc), so reusing the
    // directory would buy no continuity, only a stale-socket bind failure.
    let mut restarted = DaemonProcess::spawn("robot-a", cluster.addr, "a-restarted");
    eventually(
        "the restarted daemon to re-register under robot-a, alongside the untouched robot-b",
        SETTLE,
        || cluster.hosts("robot-a") && cluster.hosts("robot-b"),
    )
    .await;
    assert!(restarted.alive());

    // ---- proof the cluster, not just the process, recovered -------------
    // A *fresh* dataflow, placed across the restarted daemon and the
    // never-touched survivor — see the module doc for why the *original*
    // dataflow's producer does not, and should not be expected to, come
    // back on its own.
    let dataflow_2 = start_split_pipeline(&mut cli, "killroute-recovered").await;
    let (producer_2, _producer_2_events) = connect_producer(&restarted, dataflow_2).await;
    let (_consumer_2, mut consumer_2_events) = connect_consumer(&beta, dataflow_2).await;

    let mut producer_2 = producer_2;
    let mut output_2 = producer_2
        .raw_output("image")
        .expect("the second producer declared this output");

    let mut delivered = false;
    let recovery_deadline = Instant::now() + SETTLE;
    while !delivered && Instant::now() < recovery_deadline {
        let _ = output_2.send_bytes(1u64.to_le_bytes(), astrs_wire::Metadata::default());
        if let Some(Event::Input { .. }) = consumer_2_events.recv_async_timeout(STEP).await {
            delivered = true;
        }
    }
    assert!(
        delivered,
        "a fresh dataflow through the restarted daemon must route data end to end — \
         the cluster must be usable again, not merely a process that answers pings"
    );
    drop(producer_2);

    // ---- teardown ---------------------------------------------------
    alpha.stop_if_alive();
    beta.stop_if_alive();
    restarted.stop_if_alive();
    cluster.shutdown().await;
}
