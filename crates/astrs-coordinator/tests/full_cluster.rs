//! Full-cluster integration tests: CLI ↔ coordinator ↔ fake-daemon
//! conversations over a real, in-process TCP socket (blueprint §4.2, §7.2,
//! §7.3, §12, §17).
//!
//! Every module inside `astrs-coordinator` proves its own logic by calling
//! `handlers::*`/`catchup::*` directly against a bare `Coordinator` — no
//! socket, no handshake, fake daemons that are just an `mpsc::Sender` the
//! test holds by hand. That is the right level for FSM edge cases (this
//! crate's unit tests already run the full build/spawn/stop/param/topology
//! matrix that way), but it never proves that a *real* connection — with a
//! real `Hello`/`Welcome`, a real [`astrs_wire::FrameKind`]-tagged frame on
//! the wire, a real second TCP connection standing in for a second CLI
//! session — reaches the same code. This file is that proof: a `CliActor`
//! and a `DaemonActor` script the two ends of the wire protocol exactly as
//! a real `astrs` binary and a real `astrs-daemon` would, dialling a real
//! [`CoordinatorServer`] bound to an ephemeral loopback port.
//!
//! What is deliberately *not* re-tested here: the fine-grained FSM
//! branches (a daemon disconnect mid-build releasing a `WaitForBuild`
//! waiter, `create_only` semantics, every dynamic-topology `NotYetSupported`
//! path, ...) already have a direct unit test next to the code they cover.
//! This file's scenarios are the ones that only exist once a socket is in
//! the loop: the accept → handshake → role-dispatch path itself (covered
//! lightly here, more thoroughly in `server`'s own tests), a whole
//! Build→Start→Stop lifecycle riding real frames end to end, and the two
//! scenarios the task brief names explicitly that no unit test reaches —
//! log fan-out to *two* independent CLI sessions, and `StateCatchUp`
//! continuity across a simulated daemon reconnect.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use astrs_coordinator::{Coordinator, CoordinatorConfig, CoordinatorServer, GLOBAL_SCOPE_DATAFLOW};
use astrs_transport::{ConnectionCounters, FramedStream, HandshakeParams, LocalIdentity, initiate};
use astrs_wire::{
    AuthToken, BuildOutcome, ControlReply, ControlRequest, CoordinatorEvent, DaemonEvent, DaemonId,
    DaemonRegistration, DataFrame, DataflowId, DataflowSource, DataflowStatus, ErrorCode,
    FeatureFlags, FrameKind, FrameLimits, LogFrame, LogQuery, LogRecord, Metadata, NodeExitCause,
    NodeSource, ParamKey, ParamScope, Parameter, PortRef, Role, SpawnOutcome, StateEntry,
    StateEntryKind, SubscriptionId, TopicQuery,
};
use tokio::net::TcpStream;

/// Generous enough that CI scheduling jitter never causes a false
/// failure, short enough that a genuinely stuck exchange still fails the
/// test instead of the suite hanging.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The cluster token every actor in this file authenticates with.
fn token() -> AuthToken {
    AuthToken::from_bytes([0x5a; 32])
}

/// A coordinator config with a fast heartbeat, so any test that wants to
/// exercise the watchdog does not have to wait out the (much longer)
/// production default.
fn config() -> CoordinatorConfig {
    CoordinatorConfig::new(token())
        .with_port(0)
        .with_heartbeat(Duration::from_millis(50), 3)
}

/// A running coordinator, its shared handle (for direct store/registry
/// assertions no CLI verb exposes), and the pieces needed to shut it down
/// cleanly at the end of a test.
struct Cluster {
    addr: SocketAddr,
    coordinator: Coordinator,
    handle: astrs_coordinator::ServerHandle,
    task: tokio::task::JoinHandle<astrs_coordinator::Result<()>>,
}

impl Cluster {
    async fn start() -> Self {
        let coordinator = Coordinator::open_in_memory(config()).expect("in-memory store");
        let server = CoordinatorServer::bind(coordinator.clone())
            .await
            .expect("bind");
        let addr = server.local_addr().expect("local addr");
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
        self.task
            .await
            .expect("server task panicked")
            .expect("server returned an error");
    }
}

/// The CLI side of one connection: `ControlRequest` in, `ControlReply` (or
/// a pushed `Log`/`Data` frame) out — exactly what a real `astrs` process
/// does over its one connection to the coordinator.
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
        initiate(&mut stream, &params, TIMEOUT)
            .await
            .expect("cli handshake");
        Self { stream }
    }

    async fn request(&mut self, request: ControlRequest) -> ControlReply {
        self.stream
            .send_message(&request)
            .await
            .expect("send request");
        tokio::time::timeout(TIMEOUT, self.stream.expect_message(FrameKind::ControlReply))
            .await
            .expect("timed out waiting for a ControlReply")
            .expect("decode ControlReply")
    }

    /// The next pushed `Log` frame for an open `LogSubscribe` — rides the
    /// same connection as replies, but tagged `FrameKind::Log`, never
    /// `FrameKind::ControlReply`.
    async fn expect_log(&mut self) -> LogFrame {
        tokio::time::timeout(TIMEOUT, self.stream.expect_message(FrameKind::Log))
            .await
            .expect("timed out waiting for a pushed Log frame")
            .expect("decode LogFrame")
    }

    /// As [`Self::expect_log`], for a pushed `Data` frame — the frame kind
    /// a `TopicSubscribe` tap streams (blueprint §13).
    async fn expect_data(&mut self) -> DataFrame {
        tokio::time::timeout(TIMEOUT, self.stream.expect_message(FrameKind::Data))
            .await
            .expect("timed out waiting for a pushed Data frame")
            .expect("decode DataFrame")
    }

    /// Polls `Check` until the dataflow reports a terminal
    /// [`astrs_wire::DataflowResult`], or panics after too many attempts.
    /// The build/spawn/stop dispatch this file scripts is asynchronous on
    /// the coordinator's side (a daemon's reply lands on a background
    /// session task, not synchronously inside the request that triggered
    /// it), so a single `Check` right after `Stop` racing that daemon
    /// event landing is expected, not a bug — polling is the correct tool,
    /// not a sleep.
    async fn wait_for_terminal(&mut self, dataflow: DataflowId) -> astrs_wire::DataflowResult {
        for _ in 0..200 {
            match self
                .request(ControlRequest::Check {
                    dataflow: Some(dataflow),
                })
                .await
            {
                ControlReply::DataflowResult { result } => return *result,
                ControlReply::Ok => tokio::time::sleep(Duration::from_millis(10)).await,
                other => panic!("unexpected Check reply while polling: {other:?}"),
            }
        }
        panic!("dataflow {dataflow} never reached a terminal status");
    }
}

/// The daemon side of one connection: `Register`, then whatever
/// `CoordinatorEvent`s the script expects and `DaemonEvent`s it answers
/// with — standing in for a real `astrs-daemon` process exactly as the
/// task brief's "scripted daemon actor" describes.
struct DaemonActor {
    stream: FramedStream<TcpStream>,
}

impl DaemonActor {
    /// Connects, greets as [`Role::Daemon`] and sends `Register` with
    /// `catch_up_seq`, returning the actor and its assigned session id.
    async fn connect(addr: SocketAddr, id: DaemonId, catch_up_seq: u64) -> Self {
        let raw = TcpStream::connect(addr).await.expect("tcp connect");
        let mut stream =
            FramedStream::new(raw, FrameLimits::network(), ConnectionCounters::shared());
        let params = HandshakeParams::new(LocalIdentity::new(Role::Daemon), token())
            .with_features(FeatureFlags::EMPTY);
        let handshake = initiate(&mut stream, &params, TIMEOUT)
            .await
            .expect("daemon handshake");

        let mut actor = Self { stream };
        let mut registration =
            DaemonRegistration::new(id, "127.0.0.1:7408", handshake.session.session_id);
        registration.catch_up_seq = catch_up_seq;
        actor.send(DaemonEvent::Register(registration)).await;
        actor
    }

    async fn send(&mut self, event: DaemonEvent) {
        self.stream.send_message(&event).await.expect("daemon send");
    }

    /// The next `CoordinatorEvent` meaningful to a script, silently
    /// skipping the background watchdog's own `Heartbeat` traffic (§4.3) —
    /// every scripted daemon in this file receives it every 50 ms
    /// regardless of what the script is doing, and no scenario here is
    /// about that event.
    async fn expect_event(&mut self) -> CoordinatorEvent {
        loop {
            let event = tokio::time::timeout(
                TIMEOUT,
                self.stream.expect_message(FrameKind::CoordinatorEvent),
            )
            .await
            .expect("timed out waiting for a CoordinatorEvent")
            .expect("decode CoordinatorEvent");
            if !matches!(event, CoordinatorEvent::Heartbeat { .. }) {
                return event;
            }
        }
    }

    /// Drains every `StateCatchUp` batch pushed right after registration
    /// into one ordered `Vec`, stopping at `final_batch`.
    async fn drain_catch_up(&mut self) -> Vec<StateEntry> {
        let mut entries = Vec::new();
        loop {
            match self.expect_event().await {
                CoordinatorEvent::StateCatchUp {
                    entries: batch,
                    final_batch,
                    ..
                } => {
                    entries.extend(batch);
                    if final_batch {
                        return entries;
                    }
                }
                other => panic!("expected StateCatchUp, got {other:?}"),
            }
        }
    }
}

/// A one-node manifest with a `build:` line, so `Build` actually has
/// something to dispatch instead of going straight to `Ready`.
fn manifest_with_build(id: &str) -> String {
    format!("name: demo\nnodes:\n  - id: {id}\n    path: ./{id}\n    build: \"true\"\n")
}

/// A one-node manifest with no build step, for scenarios that only care
/// about the spawn/stop half of the lifecycle.
fn manifest_no_build(id: &str) -> String {
    format!("name: demo\nnodes:\n  - id: {id}\n    path: ./{id}\n")
}

/// A one-node manifest declaring one output port, with `debug:` set as
/// asked — for the `TopicSubscribe` gate and tap fan-out scenarios, which
/// need a real placed node to tap and a real dataflow-level `debug` flag
/// to gate on (blueprint §13).
fn manifest_with_output(id: &str, output: &str, debug: bool) -> String {
    format!(
        "name: demo\ndebug: {debug}\nnodes:\n  - id: {id}\n    path: ./{id}\n    outputs: [{output}]\n"
    )
}

/// The full sequence the task brief names as the minimum scripted-actor
/// scenario: greet → `Register` → `Build` → `BuildResult` → `WaitForBuild`
/// → `Start` → `Spawn` → `SpawnResult` + `AllNodesReady` → `WaitForSpawn`
/// → `Stop` → `StopDataflow` → `AllNodesFinished` → `Check` shows the
/// terminal result. Single-daemon local mode, exercised exactly the way a
/// real `astrs up && astrs start && astrs stop` session would drive it.
#[tokio::test]
async fn build_start_stop_round_trips_over_real_frames() {
    let cluster = Cluster::start().await;
    let mut cli = CliActor::connect(cluster.addr).await;
    let mut daemon = DaemonActor::connect(cluster.addr, DaemonId::generate(None), 0).await;
    daemon.drain_catch_up().await;

    let reply = cli
        .request(ControlRequest::Build {
            manifest: manifest_with_build("camera"),
            working_dir: None,
            name: Some("demo".to_owned()),
            force: false,
        })
        .await;
    let ControlReply::BuildStarted { build } = reply else {
        panic!("unexpected Build reply: {reply:?}");
    };

    match daemon.expect_event().await {
        CoordinatorEvent::Build {
            build: got_build,
            dataflow,
            ..
        } => {
            assert_eq!(got_build, build);
            daemon
                .send(DaemonEvent::BuildResult {
                    build,
                    dataflow,
                    outcome: BuildOutcome::Succeeded {
                        artifacts: Vec::new(),
                        took: astrs_wire::DurationMs::new(1),
                    },
                })
                .await;
        }
        other => panic!("expected CoordinatorEvent::Build, got {other:?}"),
    }

    let reply = cli
        .request(ControlRequest::WaitForBuild {
            build,
            timeout: None,
        })
        .await;
    assert_eq!(reply, ControlReply::Ok, "the build must be reported ready");

    let reply = cli
        .request(ControlRequest::Start {
            source: DataflowSource::Build { build },
            name: Some("demo".to_owned()),
            detach: true,
        })
        .await;
    let ControlReply::Started { dataflow, .. } = reply else {
        panic!("unexpected Start reply: {reply:?}");
    };

    let node = match daemon.expect_event().await {
        CoordinatorEvent::Spawn { node, .. } => {
            let node_id = node.node.clone();
            daemon
                .send(DaemonEvent::SpawnResult {
                    dataflow,
                    node: node_id.clone(),
                    generation: node.generation,
                    outcome: SpawnOutcome::Spawned {
                        pid: Some(4242),
                        started_at: astrs_time::HlcTimestamp::EPOCH,
                    },
                })
                .await;
            daemon
                .send(DaemonEvent::AllNodesReady {
                    dataflow,
                    nodes: vec![node_id.clone()],
                })
                .await;
            node_id
        }
        other => panic!("expected CoordinatorEvent::Spawn, got {other:?}"),
    };
    // Once every hosting daemon (here, the only one) has reported ready,
    // the coordinator confirms it cluster-wide (blueprint §7.3: "a daemon
    // holds deliveries until this arrives") — back to this same daemon.
    match daemon.expect_event().await {
        CoordinatorEvent::AllNodesReady {
            dataflow: got,
            failed,
        } => {
            assert_eq!(got, dataflow);
            assert!(failed.is_empty());
        }
        other => panic!(
            "expected the coordinator's own CoordinatorEvent::AllNodesReady echo, got {other:?}"
        ),
    }

    let reply = cli
        .request(ControlRequest::WaitForSpawn {
            dataflow,
            timeout: None,
        })
        .await;
    assert_eq!(
        reply,
        ControlReply::Ok,
        "every dispatched node reported ready"
    );

    let reply = cli
        .request(ControlRequest::Stop {
            dataflow,
            grace: None,
        })
        .await;
    assert_eq!(reply, ControlReply::Ok);

    match daemon.expect_event().await {
        CoordinatorEvent::StopDataflow {
            dataflow: stopped, ..
        } => assert_eq!(stopped, dataflow),
        other => panic!("expected CoordinatorEvent::StopDataflow, got {other:?}"),
    }
    daemon
        .send(DaemonEvent::NodeStopped {
            dataflow,
            node: node.clone(),
            generation: 1,
            cause: NodeExitCause::Success,
            restarting: false,
        })
        .await;
    daemon
        .send(DaemonEvent::AllNodesFinished {
            dataflow,
            results: BTreeMap::from([(node, NodeExitCause::Success)]),
        })
        .await;

    let result = cli.wait_for_terminal(dataflow).await;
    assert_eq!(result.status, DataflowStatus::Finished);

    cluster.shutdown().await;
}

/// FSM matrix, scenario 1: a build step fails. `WaitForBuild` must answer
/// the typed failure rather than `Ok`, and must never dispatch a spawn.
#[tokio::test]
async fn a_failed_build_is_reported_through_wait_for_build() {
    let cluster = Cluster::start().await;
    let mut cli = CliActor::connect(cluster.addr).await;
    let mut daemon = DaemonActor::connect(cluster.addr, DaemonId::generate(None), 0).await;
    daemon.drain_catch_up().await;

    let ControlReply::BuildStarted { build } = cli
        .request(ControlRequest::Build {
            manifest: manifest_with_build("camera"),
            working_dir: None,
            name: None,
            force: false,
        })
        .await
    else {
        panic!("expected BuildStarted");
    };

    let dataflow = match daemon.expect_event().await {
        CoordinatorEvent::Build { dataflow, .. } => {
            daemon
                .send(DaemonEvent::BuildResult {
                    build,
                    dataflow,
                    outcome: BuildOutcome::Failed {
                        node: None,
                        exit_code: Some(1),
                        message: "cargo build failed".to_owned(),
                        output: "error[E0000]".to_owned(),
                    },
                })
                .await;
            dataflow
        }
        other => panic!("expected CoordinatorEvent::Build, got {other:?}"),
    };

    let reply = cli
        .request(ControlRequest::WaitForBuild {
            build,
            timeout: None,
        })
        .await;
    assert_eq!(reply.error_code(), Some(astrs_wire::ErrorCode::BuildFailed));

    // The dataflow itself is now `Failed` — `Check` reports it as
    // terminal without needing any spawn to ever have been dispatched.
    let result = cli.wait_for_terminal(dataflow).await;
    assert_eq!(result.status, DataflowStatus::Failed);

    cluster.shutdown().await;
}

/// FSM matrix, scenario 2: a node fails to spawn. `Start` (blocking,
/// `detach: false`) must answer the failure rather than `Started`.
#[tokio::test]
async fn a_spawn_failure_is_reported_through_start() {
    let cluster = Cluster::start().await;
    let mut daemon = DaemonActor::connect(cluster.addr, DaemonId::generate(None), 0).await;
    daemon.drain_catch_up().await;

    let start = tokio::spawn({
        let addr = cluster.addr;
        async move {
            let mut cli = CliActor::connect(addr).await;
            cli.request(ControlRequest::Start {
                source: DataflowSource::Manifest {
                    yaml: manifest_no_build("camera"),
                    working_dir: None,
                },
                name: None,
                detach: false,
            })
            .await
        }
    });

    let (dataflow, node) = match daemon.expect_event().await {
        CoordinatorEvent::Spawn { node, .. } => (node.dataflow, node.node.clone()),
        other => panic!("expected CoordinatorEvent::Spawn, got {other:?}"),
    };
    daemon
        .send(DaemonEvent::SpawnResult {
            dataflow,
            node: node.clone(),
            generation: 1,
            outcome: SpawnOutcome::Failed {
                message: "no such file or directory".to_owned(),
                errno: Some(2),
            },
        })
        .await;
    daemon
        .send(DaemonEvent::AllNodesReady {
            dataflow,
            nodes: Vec::new(),
        })
        .await;

    let reply = start.await.expect("start task panicked");
    assert!(
        reply.error_code().is_some(),
        "a failed spawn must be reported as a typed error, got {reply:?}"
    );

    cluster.shutdown().await;
}

/// FSM matrix, scenario 3: a node crashes mid-run, unprompted (no `Stop`
/// was ever sent). `NodeStopped` alone records the cause; the dataflow
/// only reaches a terminal, checkable status once `AllNodesFinished`
/// confirms nothing is left running — mirroring
/// `handlers::lifecycle`'s own `a_node_crash_mid_run_is_visible_through_check_after_all_nodes_finished`
/// unit test, here over a real socket instead of a direct call.
#[tokio::test]
async fn a_node_crash_mid_run_is_visible_through_check() {
    let cluster = Cluster::start().await;
    let mut cli = CliActor::connect(cluster.addr).await;
    let mut daemon = DaemonActor::connect(cluster.addr, DaemonId::generate(None), 0).await;
    daemon.drain_catch_up().await;

    let ControlReply::Started { dataflow, .. } = cli
        .request(ControlRequest::Start {
            source: DataflowSource::Manifest {
                yaml: manifest_no_build("camera"),
                working_dir: None,
            },
            name: None,
            detach: true,
        })
        .await
    else {
        panic!("expected Started");
    };

    let node = match daemon.expect_event().await {
        CoordinatorEvent::Spawn { node, .. } => {
            let node_id = node.node.clone();
            daemon
                .send(DaemonEvent::SpawnResult {
                    dataflow,
                    node: node_id.clone(),
                    generation: node.generation,
                    outcome: SpawnOutcome::Spawned {
                        pid: Some(1),
                        started_at: astrs_time::HlcTimestamp::EPOCH,
                    },
                })
                .await;
            daemon
                .send(DaemonEvent::AllNodesReady {
                    dataflow,
                    nodes: vec![node_id.clone()],
                })
                .await;
            node_id
        }
        other => panic!("expected CoordinatorEvent::Spawn, got {other:?}"),
    };

    // No `Stop` was ever sent — the daemon reports a crash on its own.
    let crash = NodeExitCause::Signal {
        signal: 11,
        name: "SIGSEGV".to_owned(),
    };
    daemon
        .send(DaemonEvent::NodeStopped {
            dataflow,
            node: node.clone(),
            generation: 1,
            cause: crash.clone(),
            restarting: false,
        })
        .await;
    daemon
        .send(DaemonEvent::AllNodesFinished {
            dataflow,
            results: BTreeMap::from([(node, crash)]),
        })
        .await;

    let result = cli.wait_for_terminal(dataflow).await;
    assert_eq!(result.status, DataflowStatus::Failed);

    cluster.shutdown().await;
}

/// A parameter written by one CLI session is visible to another, and a
/// delete really removes it — the wire-level round trip on top of what
/// `handlers::params`'s own unit tests already prove about revision
/// ordering underneath.
#[tokio::test]
async fn params_round_trip_across_independent_cli_sessions() {
    let cluster = Cluster::start().await;
    let mut writer = CliActor::connect(cluster.addr).await;
    let mut reader = CliActor::connect(cluster.addr).await;
    let key = ParamKey::new("gain").expect("valid key");

    let reply = writer
        .request(ControlRequest::SetParam {
            scope: ParamScope::Global,
            key: key.clone(),
            value: Parameter::Integer(7),
            create_only: false,
        })
        .await;
    assert_eq!(reply, ControlReply::Ok);

    let reply = reader
        .request(ControlRequest::GetParam {
            scope: ParamScope::Global,
            key: key.clone(),
            inherited: false,
        })
        .await;
    assert_eq!(
        reply,
        ControlReply::ParamValue {
            key: key.clone(),
            value: Some(Parameter::Integer(7)),
            scope: ParamScope::Global
        }
    );

    // The wire round trip above proves the *value* is visible across
    // sessions; `astrs-store`'s own tests already prove a revision
    // increments and `created_at` is pinned on the first write in
    // isolation (`record::param`'s doctest, `store::params`'s
    // `revision_increments_and_created_at_is_pinned_to_the_first_write`).
    // What only this level can prove is that a *second write reaching the
    // same coordinator over the wire* lands as the next revision against
    // its HLC clock, not a fresh one — the ordering guarantee `SetParam`
    // actually leans on.
    let first_record = cluster
        .coordinator
        .store
        .get_param(GLOBAL_SCOPE_DATAFLOW, key.clone())
        .await
        .expect("read back the record")
        .expect("the value just written");
    assert_eq!(first_record.revision, 1);
    assert_eq!(first_record.created_at, first_record.updated_at);

    let reply = writer
        .request(ControlRequest::SetParam {
            scope: ParamScope::Global,
            key: key.clone(),
            value: Parameter::Integer(8),
            create_only: false,
        })
        .await;
    assert_eq!(reply, ControlReply::Ok);
    let second_record = cluster
        .coordinator
        .store
        .get_param(GLOBAL_SCOPE_DATAFLOW, key.clone())
        .await
        .expect("read back the record")
        .expect("the value just overwritten");
    assert_eq!(
        second_record.revision, 2,
        "a second write to the same key is the next revision, not a fresh one"
    );
    assert_eq!(
        second_record.created_at, first_record.created_at,
        "created_at is pinned to the first write"
    );
    assert!(
        second_record.updated_at >= first_record.updated_at,
        "the HLC clock does not run backwards between two writes on the \
         same coordinator: {:?} then {:?}",
        first_record.updated_at,
        second_record.updated_at
    );

    let reply = writer
        .request(ControlRequest::DeleteParam {
            scope: ParamScope::Global,
            key: key.clone(),
        })
        .await;
    assert_eq!(reply, ControlReply::Ok);

    let reply = reader
        .request(ControlRequest::GetParam {
            scope: ParamScope::Global,
            key: key.clone(),
            inherited: false,
        })
        .await;
    assert_eq!(
        reply,
        ControlReply::ParamValue {
            key,
            value: None,
            scope: ParamScope::Global
        }
    );

    cluster.shutdown().await;
}

/// Params live in `astrs-store`, not in the coordinator's in-memory
/// registries — so a value set before a coordinator process ends must
/// still answer `GetParam` once a *new* `Coordinator` reopens the same
/// store file. `Cluster::start` (every other test in this file) uses
/// [`Coordinator::open_in_memory`] precisely because those tests do not
/// want durability; this one opens a real file twice, with a full
/// server shutdown and a fresh `Coordinator`/`CoordinatorServer` pair in
/// between, to prove the boundary `open_in_memory` papers over.
#[tokio::test]
async fn a_param_survives_a_coordinator_restart_against_its_store_file() {
    let store_path = std::env::temp_dir().join(format!(
        "astrs-coordinator-param-persistence-{}-{:?}.redb",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&store_path);
    let key = ParamKey::new("gain").expect("valid key");

    // redb's file lock is released when the last `Arc<CoordinatorStore>`
    // clone drops — which happens when every per-connection task holding
    // one finishes, not the instant `serve()`'s own accept-loop future
    // does. Reopening the same path is therefore deadline-polled (per
    // this crate's own test conventions) rather than attempted once.
    async fn open_coordinator_when_free(store_path: &std::path::Path) -> Coordinator {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match astrs_store::CoordinatorStore::open(store_path) {
                Ok(store) => {
                    return Coordinator::new(config(), astrs_store::AsyncStore::new(store));
                }
                Err(_) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!("open store file: {error}"),
            }
        }
    }

    async fn start_server(
        store_path: &std::path::Path,
    ) -> (
        SocketAddr,
        astrs_coordinator::ServerHandle,
        tokio::task::JoinHandle<astrs_coordinator::Result<()>>,
    ) {
        let coordinator = open_coordinator_when_free(store_path).await;
        let server = CoordinatorServer::bind(coordinator).await.expect("bind");
        let addr = server.local_addr().expect("local addr");
        let handle = server.handle();
        let task = tokio::spawn(server.serve());
        (addr, handle, task)
    }

    async fn stop_server(
        cli: CliActor,
        handle: astrs_coordinator::ServerHandle,
        task: tokio::task::JoinHandle<astrs_coordinator::Result<()>>,
    ) {
        // Ends this session's socket *before* asking the server to shut
        // down, so the per-connection task sees a clean EOF and exits
        // (and drops its `Coordinator`/store clone) rather than racing
        // the accept loop's own shutdown.
        let _ = cli.stream.into_halves().1.shutdown().await;
        handle.shutdown();
        task.await
            .expect("server task panicked")
            .expect("server returned an error");
    }

    // Epoch 1: a fresh coordinator over the file, one write, then a full
    // shutdown — not just dropping the value, ending the whole server task.
    {
        let (addr, handle, task) = start_server(&store_path).await;
        let mut cli = CliActor::connect(addr).await;
        let reply = cli
            .request(ControlRequest::SetParam {
                scope: ParamScope::Global,
                key: key.clone(),
                value: Parameter::Integer(42),
                create_only: false,
            })
            .await;
        assert_eq!(reply, ControlReply::Ok);
        stop_server(cli, handle, task).await;
    }

    // Epoch 2: a *new* `Coordinator` reopening the same file must already
    // know the value — nothing in this process carried it over in memory.
    {
        let (addr, handle, task) = start_server(&store_path).await;
        let mut cli = CliActor::connect(addr).await;
        let reply = cli
            .request(ControlRequest::GetParam {
                scope: ParamScope::Global,
                key: key.clone(),
                inherited: false,
            })
            .await;
        assert_eq!(
            reply,
            ControlReply::ParamValue {
                key,
                value: Some(Parameter::Integer(42)),
                scope: ParamScope::Global,
            }
        );
        stop_server(cli, handle, task).await;
    }

    let _ = std::fs::remove_file(&store_path);
}

/// The scenario the task brief names explicitly: one pushed log record
/// fans out to *two* independent CLI sessions, each with its own
/// `SubscriptionId`, over two entirely separate TCP connections.
#[tokio::test]
async fn a_log_push_fans_out_to_two_independent_cli_sessions() {
    let cluster = Cluster::start().await;
    let mut subscriber_a = CliActor::connect(cluster.addr).await;
    let mut subscriber_b = CliActor::connect(cluster.addr).await;
    let mut daemon = DaemonActor::connect(cluster.addr, DaemonId::generate(None), 0).await;
    daemon.drain_catch_up().await;

    let sub_a = SubscriptionId::new(1);
    let sub_b = SubscriptionId::new(2);
    for (cli, id) in [(&mut subscriber_a, sub_a), (&mut subscriber_b, sub_b)] {
        let reply = cli
            .request(ControlRequest::LogSubscribe {
                dataflow: None,
                node: None,
                query: LogQuery::new(),
                subscription: id,
            })
            .await;
        assert_eq!(reply, ControlReply::Ok);
    }

    daemon
        .send(DaemonEvent::Log {
            request: None,
            records: vec![LogRecord::new(
                astrs_time::HlcTimestamp::EPOCH,
                astrs_wire::LogLevel::Warn,
                "disk 90% full",
            )],
            truncated: false,
        })
        .await;

    let frame_a = subscriber_a.expect_log().await;
    let frame_b = subscriber_b.expect_log().await;
    assert_eq!(frame_a.subscription, sub_a);
    assert_eq!(frame_b.subscription, sub_b);
    assert_eq!(frame_a.record.message, "disk 90% full");
    assert_eq!(frame_b.record.message, "disk 90% full");

    cluster.shutdown().await;
}

/// Starts a one-node dataflow (`camera`, one declared output) over real
/// frames and reports it ready, returning the dataflow id and the port the
/// node exposes — the setup every `TopicSubscribe` scenario below shares.
async fn start_one_output_node(
    cli: &mut CliActor,
    daemon: &mut DaemonActor,
    output: &str,
    debug: bool,
) -> (DataflowId, PortRef) {
    let ControlReply::Started { dataflow, .. } = cli
        .request(ControlRequest::Start {
            source: DataflowSource::Manifest {
                yaml: manifest_with_output("camera", output, debug),
                working_dir: None,
            },
            name: None,
            detach: true,
        })
        .await
    else {
        panic!("expected Started");
    };

    match daemon.expect_event().await {
        CoordinatorEvent::Spawn { node, .. } => {
            let node_id = node.node.clone();
            daemon
                .send(DaemonEvent::SpawnResult {
                    dataflow,
                    node: node_id.clone(),
                    generation: node.generation,
                    outcome: SpawnOutcome::Spawned {
                        pid: Some(1),
                        started_at: astrs_time::HlcTimestamp::EPOCH,
                    },
                })
                .await;
            daemon
                .send(DaemonEvent::AllNodesReady {
                    dataflow,
                    nodes: vec![node_id.clone()],
                })
                .await;
        }
        other => panic!("expected CoordinatorEvent::Spawn, got {other:?}"),
    }
    // The coordinator echoes `AllNodesReady` cluster-wide once every
    // hosting daemon (here, the only one) has reported it (blueprint
    // §7.3) — drained here so a caller's next `expect_event` cannot
    // mistake it for something else.
    match daemon.expect_event().await {
        CoordinatorEvent::AllNodesReady { dataflow: got, .. } => assert_eq!(got, dataflow),
        other => panic!("expected the coordinator's AllNodesReady echo, got {other:?}"),
    }

    let port: PortRef = format!("camera/{output}")
        .parse()
        .expect("a valid port ref");
    (dataflow, port)
}

/// Blueprint §13: a dataflow whose manifest never set `debug: true`
/// refuses `TopicSubscribe` outright, over a real connection — the wire
/// proof for what `handlers::logs`'s own
/// `topic_subscribe_refuses_a_dataflow_without_debug_true` unit test
/// already covers by calling the handler directly.
#[tokio::test]
async fn topic_subscribe_is_refused_without_debug_true_over_real_frames() {
    let cluster = Cluster::start().await;
    let mut cli = CliActor::connect(cluster.addr).await;
    let mut daemon = DaemonActor::connect(cluster.addr, DaemonId::generate(None), 0).await;
    daemon.drain_catch_up().await;

    let (dataflow, port) = start_one_output_node(&mut cli, &mut daemon, "frames", false).await;

    let reply = cli
        .request(ControlRequest::TopicSubscribe {
            dataflow,
            port,
            query: TopicQuery::new(),
            subscription: SubscriptionId::new(1),
        })
        .await;
    assert_eq!(
        reply.error_code(),
        Some(ErrorCode::InvalidArgument),
        "a dataflow without `debug: true` must refuse the tap, got {reply:?}"
    );

    cluster.shutdown().await;
}

/// Blueprint §13/§17: with `debug: true`, `TopicSubscribe` opens a real
/// tap — the coordinator gates it, routes `TopicTapStart` to the daemon
/// actually hosting the node, and every `TopicTapData` push that daemon
/// answers with fans back out to the subscriber framed as a real `Data`
/// frame, byte-for-byte, in order. This is the wire-level half of `astrs
/// topic echo`/`hz`'s loop; `astrs-cli`'s own tests prove the client-side
/// decode/rate-window math over frames shaped exactly like these.
#[tokio::test]
async fn topic_tap_data_fans_out_to_a_subscriber_over_real_frames() {
    let cluster = Cluster::start().await;
    let mut cli = CliActor::connect(cluster.addr).await;
    let mut daemon = DaemonActor::connect(cluster.addr, DaemonId::generate(None), 0).await;
    daemon.drain_catch_up().await;

    let (dataflow, port) = start_one_output_node(&mut cli, &mut daemon, "frames", true).await;

    let subscription = SubscriptionId::new(7);
    let reply = cli
        .request(ControlRequest::TopicSubscribe {
            dataflow,
            port: port.clone(),
            query: TopicQuery::new(),
            subscription,
        })
        .await;
    assert_eq!(reply, ControlReply::Ok, "debug is on; the tap must open");

    match daemon.expect_event().await {
        CoordinatorEvent::TopicTapStart {
            dataflow: got_dataflow,
            port: got_port,
            subscription: got_subscription,
            ..
        } => {
            assert_eq!(got_dataflow, dataflow);
            assert_eq!(got_port, port);
            assert_eq!(got_subscription, subscription);
        }
        other => panic!("expected CoordinatorEvent::TopicTapStart, got {other:?}"),
    }

    // Three frames, as if the daemon's own tap had copied three real
    // outputs — the plumbing `astrs topic hz` counts on (`received == N`)
    // does not care what is inside them, only that all three arrive, in
    // order, unmodified.
    let payloads: [&[u8]; 3] = [b"frame-0", b"frame-1", b"frame-2"];
    for (index, payload) in payloads.iter().enumerate() {
        daemon
            .send(DaemonEvent::TopicTapData {
                frame: Box::new(DataFrame::new(
                    subscription,
                    dataflow,
                    port.clone(),
                    Metadata::new(astrs_time::HlcTimestamp::new(index as u64, 0)),
                    payload.to_vec(),
                )),
                dropped: 0,
            })
            .await;
    }

    for expected in &payloads {
        let received = cli.expect_data().await;
        assert_eq!(received.subscription, subscription);
        assert_eq!(received.dataflow, dataflow);
        assert_eq!(received.source, port);
        assert_eq!(received.payload, expected.to_vec());
    }

    let reply = cli
        .request(ControlRequest::TopicUnsubscribe { subscription })
        .await;
    assert_eq!(reply, ControlReply::Ok);
    match daemon.expect_event().await {
        CoordinatorEvent::TopicTapStop {
            subscription: stopped,
        } => assert_eq!(stopped, subscription),
        other => panic!("expected CoordinatorEvent::TopicTapStop, got {other:?}"),
    }

    cluster.shutdown().await;
}

/// The other scenario the task brief names explicitly: a daemon
/// disconnects and reconnects with the sequence it last applied, and the
/// `StateCatchUp` it receives on the new connection covers exactly the
/// mutations that happened while it was gone — no gap, no repeat.
#[tokio::test]
async fn state_catch_up_is_continuous_across_a_simulated_daemon_reconnect() {
    let cluster = Cluster::start().await;
    let daemon_id = DaemonId::generate(None);

    let mut first_connection = DaemonActor::connect(cluster.addr, daemon_id.clone(), 0).await;
    first_connection.drain_catch_up().await;
    let after = cluster
        .coordinator
        .store
        .sync()
        .last_seq()
        .expect("last_seq")
        .get();

    let mut cli = CliActor::connect(cluster.addr).await;
    for (key, value) in [("a", 1_i64), ("b", 2_i64)] {
        let reply = cli
            .request(ControlRequest::SetParam {
                scope: ParamScope::Global,
                key: ParamKey::new(key).expect("valid key"),
                value: Parameter::Integer(value),
                create_only: false,
            })
            .await;
        assert_eq!(reply, ControlReply::Ok);
        // Each `SetParam` also reaches the still-connected daemon as a
        // live push; drain it so it never gets mistaken for part of the
        // *next* connection's catch-up batch below.
        match first_connection.expect_event().await {
            CoordinatorEvent::SetParam { .. } => {}
            other => panic!("expected a live SetParam push, got {other:?}"),
        }
    }

    // Simulate the connection dropping (a crash, a network blip) —
    // literally drop the socket, exactly as the daemon process itself
    // disappearing would.
    drop(first_connection);
    for _ in 0..200 {
        if !cluster.coordinator.daemons().is_connected(&daemon_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!cluster.coordinator.daemons().is_connected(&daemon_id));

    // One more mutation while the daemon is gone — this is the one a
    // correct catch-up must not skip.
    let reply = cli
        .request(ControlRequest::SetParam {
            scope: ParamScope::Global,
            key: ParamKey::new("c").expect("valid key"),
            value: Parameter::Integer(3),
            create_only: false,
        })
        .await;
    assert_eq!(reply, ControlReply::Ok);

    let mut reconnected = DaemonActor::connect(cluster.addr, daemon_id, after).await;
    let entries = reconnected.drain_catch_up().await;

    let params: Vec<(ParamKey, Parameter)> = entries
        .into_iter()
        .filter_map(|entry| match entry.kind {
            StateEntryKind::ParamSet { key, value, .. } => Some((key, value)),
            _ => None,
        })
        .collect();
    assert_eq!(
        params,
        vec![
            (
                ParamKey::new("a").expect("valid key"),
                Parameter::Integer(1)
            ),
            (
                ParamKey::new("b").expect("valid key"),
                Parameter::Integer(2)
            ),
            (
                ParamKey::new("c").expect("valid key"),
                Parameter::Integer(3)
            ),
        ],
        "the reconnect must see every mutation after `after`, in order, exactly once"
    );

    cluster.shutdown().await;
}

/// `RecordStart`/`RecordStop` (blueprint §14) over real frames: `handlers::
/// misc`'s own unit tests already prove the FSM logic against a bare
/// `Coordinator` with a hand-held `mpsc` sender standing in for a daemon;
/// this is the one proof that a real `RecordStart` request, wire-encoded
/// and decoded exactly as `astrs record start` would send it, really makes
/// the coordinator dispatch a real `CoordinatorEvent::Spawn` frame —
/// `NodeSource::Recorder`, wired to the running dataflow's own declared
/// output — to a real connected daemon, and that `RecordStop` follows up
/// with a real `StopNode` for exactly that node.
#[tokio::test]
async fn record_start_stop_round_trips_over_real_frames() {
    let cluster = Cluster::start().await;
    let mut cli = CliActor::connect(cluster.addr).await;
    let mut daemon = DaemonActor::connect(cluster.addr, DaemonId::generate(None), 0).await;
    daemon.drain_catch_up().await;

    let ControlReply::Started { dataflow, .. } = cli
        .request(ControlRequest::Start {
            source: DataflowSource::Manifest {
                yaml: "name: demo\nnodes:\n  - id: camera\n    path: ./camera\n    \
                       outputs: [frames]\n"
                    .to_owned(),
                working_dir: None,
            },
            name: None,
            detach: true,
        })
        .await
    else {
        panic!("expected Started");
    };

    match daemon.expect_event().await {
        CoordinatorEvent::Spawn { node, .. } => {
            let node_id = node.node.clone();
            daemon
                .send(DaemonEvent::SpawnResult {
                    dataflow,
                    node: node_id.clone(),
                    generation: node.generation,
                    outcome: SpawnOutcome::Spawned {
                        pid: Some(1),
                        started_at: astrs_time::HlcTimestamp::EPOCH,
                    },
                })
                .await;
            daemon
                .send(DaemonEvent::AllNodesReady {
                    dataflow,
                    nodes: vec![node_id],
                })
                .await;
        }
        other => panic!("expected the camera's CoordinatorEvent::Spawn, got {other:?}"),
    };
    // The coordinator echoes `AllNodesReady` cluster-wide once every
    // hosting daemon has reported it (blueprint §7.3) — drained here so the
    // next `expect_event` cannot mistake it for the recorder's own `Spawn`.
    match daemon.expect_event().await {
        CoordinatorEvent::AllNodesReady { dataflow: got, .. } => assert_eq!(got, dataflow),
        other => panic!("expected the coordinator's AllNodesReady echo, got {other:?}"),
    }

    let output = std::env::temp_dir().join(format!(
        "astrs-full-cluster-record-test-{}-{}.arec",
        std::process::id(),
        dataflow
    ));

    let reply = cli
        .request(ControlRequest::RecordStart {
            dataflow,
            path: output.display().to_string(),
            ports: Vec::new(),
            overwrite: false,
        })
        .await;
    assert_eq!(reply, ControlReply::Ok, "record start must be accepted");

    let recorder = match daemon.expect_event().await {
        CoordinatorEvent::Spawn { node, .. } => {
            assert!(
                matches!(node.source, NodeSource::Recorder { .. }),
                "the synthesized node must be a Recorder, got {:?}",
                node.source
            );
            assert_eq!(
                node.inputs.len(),
                1,
                "one input per recorded port: {:?}",
                node.inputs
            );
            assert_eq!(node.inputs[0].source.to_string(), "camera/frames");
            let node_id = node.node.clone();
            daemon
                .send(DaemonEvent::SpawnResult {
                    dataflow,
                    node: node_id.clone(),
                    generation: node.generation,
                    outcome: SpawnOutcome::Spawned {
                        pid: Some(2),
                        started_at: astrs_time::HlcTimestamp::EPOCH,
                    },
                })
                .await;
            node_id
        }
        other => panic!("expected the recorder's own CoordinatorEvent::Spawn, got {other:?}"),
    };

    // Starting a second recording at the same path without `overwrite` is
    // refused — the coordinator's own state, not just a file-level check.
    let reply = cli
        .request(ControlRequest::RecordStart {
            dataflow,
            path: output.display().to_string(),
            ports: Vec::new(),
            overwrite: false,
        })
        .await;
    assert_eq!(
        reply.error_code(),
        Some(ErrorCode::AlreadyExists),
        "{reply:?}"
    );

    let reply = cli.request(ControlRequest::RecordStop { dataflow }).await;
    assert_eq!(reply, ControlReply::Ok, "record stop must be accepted");

    match daemon.expect_event().await {
        CoordinatorEvent::StopNode {
            dataflow: got_dataflow,
            node,
            ..
        } => {
            assert_eq!(got_dataflow, dataflow);
            assert_eq!(node, recorder);
        }
        other => panic!("expected a CoordinatorEvent::StopNode for the recorder, got {other:?}"),
    }

    // With nothing left recording, a second `RecordStop` is refused rather
    // than silently accepted.
    let reply = cli.request(ControlRequest::RecordStop { dataflow }).await;
    assert_eq!(
        reply.error_code(),
        Some(ErrorCode::InvalidArgument),
        "{reply:?}"
    );

    cluster.shutdown().await;
}
