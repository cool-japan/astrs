//! `astrs topic echo`/`hz` against a live, tapped pipeline (blueprint
//! §13, §17).
//!
//! `astrs-coordinator`'s own `full_cluster.rs::topic_tap_data_fans_out_to_a_subscriber_over_real_frames`
//! already proves the wire-level half of this loop — `TopicSubscribe` →
//! `CoordinatorEvent::TopicTapStart` routed to the right daemon →
//! `DaemonEvent::TopicTapData` fanned back as real `Data` frames — with
//! synthetic, non-Arrow byte payloads, precisely because that file's own
//! docs point at `command::topic`'s pure unit tests
//! (`summarize_payload`/`HzWindow`) for the decode and rate-math half. This
//! file is the seam neither side reaches alone: [`astrs_cli::command::topic::echo`],
//! [`astrs_cli::command::topic::hz`] and [`astrs_cli::command::topic::info`]
//! *themselves*, called against a live coordinator with a real registered
//! daemon, decoding real Arrow-encoded frames.
//!
//! # Why a hand-scripted daemon, not `MockDaemon`
//!
//! `astrs_node_api::testing::MockDaemon` (as `topic_pub_e2e.rs` uses)
//! speaks the protocol a *node* dials — it stands in for the daemon a node
//! attaches to. The tap this file exercises lives entirely on the
//! coordinator↔daemon leg (`CoordinatorEvent::TopicTapStart` →
//! `DaemonEvent::TopicTapData`, blueprint §24.1), which is a different
//! socket and a different role (`Role::Daemon`) than any `MockDaemon`
//! speaks. This file scripts that leg directly, over a real TCP
//! `Hello`/`Register`, mirroring `astrs-coordinator`'s own
//! `full_cluster.rs::DaemonActor` and `start_one_output_node` — the
//! already-proven shape for exactly this handshake — because `topic::echo`/
//! `hz` themselves are ordinary synchronous CLI entry points (each builds
//! and blocks on its own current-thread runtime; see
//! `command::client::runtime`), so they must be called from a plain,
//! non-async test function, with the coordinator and the scripted daemon
//! each driven from their own thread.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

use astrs_cli::command::client::Endpoint;
use astrs_cli::command::topic::{self, TopicStreamArgs};
use astrs_coordinator::{Coordinator, CoordinatorConfig, CoordinatorServer};
use astrs_data::builder::Int64Builder;
use astrs_data::record_batch::RecordBatch;
use astrs_data::{IntoArrayRef, ipc::encode_payload};
use astrs_transport::{ConnectionCounters, FramedStream, HandshakeParams, LocalIdentity, initiate};
use astrs_wire::{
    AuthToken, ControlReply, ControlRequest, CoordinatorEvent, DaemonEvent, DaemonId,
    DaemonRegistration, DataFrame, DataflowSource, FeatureFlags, FrameKind, FrameLimits, Metadata,
    Role, SpawnOutcome,
};
use tokio::net::TcpStream;

/// Generous enough for CI jitter, short enough that a genuinely stuck
/// exchange fails the test rather than hanging the suite.
const TIMEOUT: Duration = Duration::from_secs(5);

fn token() -> AuthToken {
    AuthToken::from_bytes([0x7e; 32])
}

/// A manifest with one output node, `debug` set as asked — the same shape
/// `full_cluster.rs::manifest_with_output` uses, for the same reason: a
/// real placed node to tap and a real dataflow-level flag to gate on.
fn manifest(debug: bool) -> String {
    format!(
        "name: demo\ndebug: {debug}\nnodes:\n  - id: camera\n    path: ./camera\n    outputs: [image]\n"
    )
}

/// A real Arrow IPC payload — a one-row `Int64` batch — so `echo`'s decode
/// path and `hz`'s frame counting both run against genuine encoded bytes,
/// not a raw byte string standing in for one.
fn int_frame(value: i64) -> Vec<u8> {
    let mut builder = Int64Builder::with_capacity(1);
    builder.append_value(value);
    let batch = RecordBatch::from_payload(builder.finish().into_array_ref());
    encode_payload(&batch)
        .expect("a valid batch encodes")
        .to_vec()
}

/// A coordinator on its own thread and runtime — `command::topic`'s
/// verbs dial it exactly as a real `astrs` process would.
struct Cluster {
    addr: SocketAddr,
    shutdown: astrs_coordinator::ServerHandle,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Cluster {
    fn start() -> Self {
        let config = CoordinatorConfig::new(token()).with_port(0);
        let hub = Coordinator::open_in_memory(config).expect("an in-memory store");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        let server = runtime
            .block_on(CoordinatorServer::bind(hub))
            .expect("a bound coordinator");
        let addr = server.local_addr().expect("its address");
        let shutdown = server.handle();
        let thread = std::thread::spawn(move || {
            let _ = runtime.block_on(server.serve());
        });
        Self {
            addr,
            shutdown,
            thread: Some(thread),
        }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::new(self.addr, token())
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        self.shutdown.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Scripts one daemon connection on its own thread: greet as
/// [`Role::Daemon`], `Register`, drain `StateCatchUp`, signal `ready` (so
/// the caller knows a `Start` now has somewhere to place a node), answer
/// the resulting `Spawn` with `SpawnResult`/`AllNodesReady`, then — only
/// when `frames` is non-empty — wait for `TopicTapStart` and answer it
/// with one `TopicTapData` per frame, spaced `frame_gap` apart so
/// `astrs topic hz`'s trailing-window rate has more than an instant of
/// span to divide by, then wait for the resulting `TopicTapStop` before
/// returning.
///
/// A dataflow whose manifest never set `debug: true` (this file's refusal
/// test) never sends a `TopicTapStart` at all — the coordinator refuses
/// `TopicSubscribe` on its own manifest check, before the daemon is ever
/// consulted (`astrs-coordinator::handlers::logs::topic_subscribe`'s own
/// doc comment) — so `frames: &[]` makes this function return right after
/// the spawn handshake, exactly as that scenario needs.
fn run_daemon_actor(
    addr: SocketAddr,
    frames: Vec<Vec<u8>>,
    frame_gap: Duration,
    ready: SyncSender<()>,
) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(async move {
        let raw = TcpStream::connect(addr).await.expect("tcp connect");
        let mut stream =
            FramedStream::new(raw, FrameLimits::network(), ConnectionCounters::shared());
        let params = HandshakeParams::new(LocalIdentity::new(Role::Daemon), token())
            .with_features(FeatureFlags::EMPTY);
        let handshake = initiate(&mut stream, &params, TIMEOUT)
            .await
            .expect("daemon handshake");

        let registration = DaemonRegistration::new(
            DaemonId::generate(None),
            "127.0.0.1:7408",
            handshake.session.session_id,
        );
        stream
            .send_message(&DaemonEvent::Register(registration))
            .await
            .expect("register");

        // Drain the (empty, on a fresh in-memory store) `StateCatchUp`
        // batch — the same shape `full_cluster.rs::DaemonActor::drain_catch_up`
        // proves, inlined here rather than shared because this file has no
        // access to that crate-private helper.
        loop {
            let event = expect_event(&mut stream).await;
            match event {
                CoordinatorEvent::StateCatchUp { final_batch, .. } => {
                    if final_batch {
                        break;
                    }
                }
                other => panic!("expected StateCatchUp while draining, got {other:?}"),
            }
        }

        // `Start` can now place a node on this daemon.
        ready.send(()).expect("signal readiness to the main thread");

        let (dataflow, node_id, generation) = match expect_event(&mut stream).await {
            CoordinatorEvent::Spawn { node, .. } => {
                (node.dataflow, node.node.clone(), node.generation)
            }
            other => panic!("expected Spawn, got {other:?}"),
        };
        stream
            .send_message(&DaemonEvent::SpawnResult {
                dataflow,
                node: node_id.clone(),
                generation,
                outcome: SpawnOutcome::Spawned {
                    pid: Some(1),
                    started_at: astrs_time::HlcTimestamp::EPOCH,
                },
            })
            .await
            .expect("send SpawnResult");
        stream
            .send_message(&DaemonEvent::AllNodesReady {
                dataflow,
                nodes: vec![node_id.clone()],
            })
            .await
            .expect("send AllNodesReady");

        if frames.is_empty() {
            // The coordinator's own `AllNodesReady` echo is the only thing
            // left to drain; no `TopicSubscribe` is coming (the
            // `debug: false` refusal never reaches this daemon at all —
            // see this function's own doc comment).
            match expect_event(&mut stream).await {
                CoordinatorEvent::AllNodesReady { .. } => {}
                other => panic!("expected the coordinator's AllNodesReady echo, got {other:?}"),
            }
            return;
        }

        // The coordinator's own `AllNodesReady` echo and a `TopicTapStart`
        // triggered by a `TopicSubscribe` racing in from the CLI connection
        // are two independent messages queued by two independent handlers
        // on the coordinator's side — either may reach this daemon first,
        // so both are accepted in whichever order they arrive rather than
        // assuming the echo always leads.
        let (port, subscription) = loop {
            match expect_event(&mut stream).await {
                CoordinatorEvent::AllNodesReady { .. } => {}
                CoordinatorEvent::TopicTapStart {
                    port, subscription, ..
                } => break (port, subscription),
                other => panic!("expected AllNodesReady or TopicTapStart, got {other:?}"),
            }
        };

        for (index, payload) in frames.into_iter().enumerate() {
            if index > 0 {
                tokio::time::sleep(frame_gap).await;
            }
            stream
                .send_message(&DaemonEvent::TopicTapData {
                    frame: Box::new(DataFrame::new(
                        subscription,
                        dataflow,
                        port.clone(),
                        Metadata::new(astrs_time::HlcTimestamp::new(index as u64, 0)),
                        payload,
                    )),
                    dropped: 0,
                })
                .await
                .expect("send TopicTapData");
        }

        // `echo`/`hz`/`info` unsubscribe once they have what they came
        // for, which this daemon eventually sees as `TopicTapStop` — but
        // the dataflow's own readiness bookkeeping is free to re-announce
        // `AllNodesReady` in between (a level-triggered re-check, not a
        // second spawn), so anything other than `TopicTapStop` here is
        // drained rather than treated as a protocol violation. And once
        // the CLI side has what it came for, it is free to drop its own
        // connection at any point in this drain — which this coordinator
        // may react to by tearing down *this* daemon connection too
        // (a session teardown racing this exact wait) — so a transport
        // error or a closed stream ends this loop exactly as cleanly as
        // `TopicTapStop` would: every assertion this file makes has
        // already run against the main thread's `topic::echo`/`hz`/`info`
        // return value by the time this daemon script is even watched via
        // `.join()`, so nothing past that point can turn a passing
        // scenario into a failing one — except this loop panicking on
        // exactly the kind of hang-up it has no reason to.
        loop {
            match try_expect_event(&mut stream).await {
                Some(CoordinatorEvent::TopicTapStop { .. }) | None => break,
                Some(CoordinatorEvent::AllNodesReady { .. }) => {}
                Some(other) => panic!("expected TopicTapStop, got {other:?}"),
            }
        }
    });
}

/// The next [`CoordinatorEvent`], silently skipping the background
/// heartbeat traffic no scenario here is about.
async fn expect_event<S>(stream: &mut FramedStream<S>) -> CoordinatorEvent
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let event = tokio::time::timeout(
            TIMEOUT,
            stream.expect_message::<CoordinatorEvent>(FrameKind::CoordinatorEvent),
        )
        .await
        .expect("timed out waiting for a CoordinatorEvent")
        .expect("decode CoordinatorEvent");
        if !matches!(event, CoordinatorEvent::Heartbeat { .. }) {
            return event;
        }
    }
}

/// As [`expect_event`], but `None` on a timeout, a decode error, or the
/// stream ending — for a wait where those outcomes are as acceptable as
/// the event actually being looked for (see this function's one call
/// site for why).
async fn try_expect_event<S>(stream: &mut FramedStream<S>) -> Option<CoordinatorEvent>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let event = tokio::time::timeout(
            TIMEOUT,
            stream.expect_message::<CoordinatorEvent>(FrameKind::CoordinatorEvent),
        )
        .await
        .ok()?
        .ok()?;
        if !matches!(event, CoordinatorEvent::Heartbeat { .. }) {
            return Some(event);
        }
    }
}

/// Starts a dataflow from `manifest` over a plain, one-shot blocking
/// connection — a small runtime built and dropped before the caller goes
/// on to call any `command::topic` verb, so there is never a moment where
/// two runtimes are both trying to own this thread.
fn start_dataflow(endpoint: &Endpoint, manifest_yaml: String) -> astrs_wire::DataflowId {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    runtime.block_on(async move {
        let mut client = astrs_cli::command::client::Client::connect(endpoint)
            .await
            .expect("connect to the coordinator");
        match client
            .request(
                "start",
                &ControlRequest::Start {
                    source: DataflowSource::Manifest {
                        yaml: manifest_yaml,
                        working_dir: None,
                    },
                    name: None,
                    detach: true,
                },
            )
            .await
            .expect("Start must be answered")
        {
            ControlReply::Started { dataflow, .. } => dataflow,
            other => panic!("expected Started, got {other:?}"),
        }
    })
}

/// Waits for the scripted daemon to finish registering before `Start` is
/// issued — `Start` needs a daemon already on the roster to place a node
/// on.
fn wait_ready(ready: &Receiver<()>) {
    ready
        .recv_timeout(TIMEOUT)
        .expect("the scripted daemon must finish registering");
}

/// Retries `topic::info` while the coordinator has not yet learned the
/// node it names exists.
///
/// `Start`'s `Started` reply (what [`start_dataflow`] waits for) comes
/// back as soon as placement decides where a node goes — it does not wait
/// for that daemon to answer back. `GetNodeInfo` (what `topic::info` asks
/// first) answers from a *different* piece of state: the coordinator's
/// per-node registry, populated only once it has actually heard
/// `SpawnResult` from the daemon — a real round trip this file's own
/// scripted actor is still mid-flight on the instant `start_dataflow`
/// returns. `echo`/`hz` never call `GetNodeInfo` at all, so neither of
/// them ever meets this race; `info` does, and the fix is the same one
/// this project's own tests reach for everywhere else a background step
/// has to catch up: poll on a deadline, never a fixed sleep.
fn info_once_the_node_is_known(
    out: &mut Vec<u8>,
    endpoint: &Endpoint,
    topic: &str,
    dataflow: &str,
) -> topic::InfoReport {
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        out.clear();
        match topic::info(out, endpoint, topic, Some(dataflow), None, false) {
            Ok(report) => return report,
            Err(astrs_cli::error::CliError::Refused { message, .. })
                if message.contains("no such node") =>
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the coordinator never learned about {topic} within {TIMEOUT:?}: {message}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(other) => panic!("astrs topic info must return: {other}"),
        }
    }
}

#[test]
fn topic_echo_decodes_real_arrow_frames_from_a_live_tapped_pipeline() {
    let cluster = Cluster::start();
    let (ready_tx, ready_rx) = sync_channel(0);
    let frames = vec![int_frame(10), int_frame(20), int_frame(30)];
    let daemon = std::thread::spawn({
        let addr = cluster.addr;
        move || run_daemon_actor(addr, frames, Duration::from_millis(5), ready_tx)
    });
    wait_ready(&ready_rx);

    let dataflow = start_dataflow(&cluster.endpoint(), manifest(true));

    let args = TopicStreamArgs {
        topic: "camera/image".to_owned(),
        dataflow: Some(dataflow.to_string()),
        json: false,
        count: Some(3),
    };
    let mut out = Vec::new();
    let report =
        topic::echo(&mut out, &cluster.endpoint(), &args).expect("astrs topic echo must return");
    assert_eq!(report.received, 3);

    let text = String::from_utf8(out).expect("utf8 output");
    // Proves the decode path actually ran (blueprint §17: "decode via
    // astrs-data with URN-aware summaries") rather than falling back to
    // the raw-hex path a non-Arrow payload would print.
    assert!(text.contains("1 row(s)"), "{text}");
    assert!(text.contains("Int64"), "{text}");
    assert!(
        !text.contains("not decodable"),
        "a real Arrow payload must never read as undecodable: {text}"
    );

    daemon.join().expect("the scripted daemon thread");
}

#[test]
fn topic_hz_reports_a_positive_rate_from_live_frames() {
    let cluster = Cluster::start();
    let (ready_tx, ready_rx) = sync_channel(0);
    let frames = vec![int_frame(1), int_frame(2), int_frame(3)];
    let daemon = std::thread::spawn({
        let addr = cluster.addr;
        move || run_daemon_actor(addr, frames, Duration::from_millis(30), ready_tx)
    });
    wait_ready(&ready_rx);

    let dataflow = start_dataflow(&cluster.endpoint(), manifest(true));

    let args = TopicStreamArgs {
        topic: "camera/image".to_owned(),
        dataflow: Some(dataflow.to_string()),
        json: true,
        count: Some(3),
    };
    let mut out = Vec::new();
    let report = topic::hz(
        &mut out,
        &cluster.endpoint(),
        &args,
        Duration::from_secs(10),
    )
    .expect("astrs topic hz must return");
    assert_eq!(report.received, 3);

    let text = String::from_utf8(out).expect("utf8 output");
    let last_line = text.lines().last().expect("at least one printed update");
    let value: serde_json::Value = serde_json::from_str(last_line).expect("json line");
    assert_eq!(value["count"], 3);
    // Deliberately not an exact figure (blueprint §17's own rate math is
    // already exactly asserted, with synthetic instants, by
    // `HzWindow::rate_per_sec`'s own unit tests in `command::topic`) — a
    // live pipeline's real inter-arrival spacing is CI-jitter-sensitive by
    // nature, so only "a rate was computed, and it is not nonsense" is
    // asserted here.
    let hz = value["hz"]
        .as_f64()
        .expect("hz must be a number once >=2 frames arrived");
    assert!(hz > 0.0, "{value}");

    daemon.join().expect("the scripted daemon thread");
}

#[test]
fn topic_echo_is_refused_with_a_message_naming_debug_true_when_the_manifest_never_set_it() {
    let cluster = Cluster::start();
    let (ready_tx, ready_rx) = sync_channel(0);
    let daemon = std::thread::spawn({
        let addr = cluster.addr;
        move || run_daemon_actor(addr, Vec::new(), Duration::from_millis(5), ready_tx)
    });
    wait_ready(&ready_rx);

    let dataflow = start_dataflow(&cluster.endpoint(), manifest(false));

    let args = TopicStreamArgs {
        topic: "camera/image".to_owned(),
        dataflow: Some(dataflow.to_string()),
        json: false,
        count: Some(1),
    };
    let mut out = Vec::new();
    let error = topic::echo(&mut out, &cluster.endpoint(), &args)
        .expect_err("a dataflow without `debug: true` must refuse the tap");
    let message = error.to_string();
    assert!(message.contains("debug: true"), "{message}");
    assert!(message.contains("topic echo"), "{message}");

    daemon.join().expect("the scripted daemon thread");
}

/// `astrs topic info` against the same live, tapped pipeline: the
/// declared type (absent here — the manifest never annotates
/// `output_types`), and the schema hash of one genuinely observed,
/// genuinely decoded Arrow frame (blueprint §17: "type URN/schema-hash/
/// plane"). `command::topic`'s own unit tests
/// (`summarize_with_hash`) already prove the hash is computed correctly
/// in isolation; what only a live pipeline proves is that `info` actually
/// reaches a real tap and carries a real frame's hash all the way out to
/// its report.
#[test]
fn topic_info_reports_the_observed_schema_hash_from_a_live_tapped_frame() {
    let cluster = Cluster::start();
    let (ready_tx, ready_rx) = sync_channel(0);
    let daemon = std::thread::spawn({
        let addr = cluster.addr;
        move || {
            run_daemon_actor(
                addr,
                vec![int_frame(42)],
                Duration::from_millis(5),
                ready_tx,
            )
        }
    });
    wait_ready(&ready_rx);

    let dataflow = start_dataflow(&cluster.endpoint(), manifest(true));

    let mut out = Vec::new();
    let report = info_once_the_node_is_known(
        &mut out,
        &cluster.endpoint(),
        "camera/image",
        &dataflow.to_string(),
    );

    assert!(
        report.type_urn.is_none(),
        "this manifest never annotates `output_types`"
    );
    let expected_hash = {
        let mut builder = Int64Builder::with_capacity(1);
        builder.append_value(42);
        let batch = RecordBatch::from_payload(builder.finish().into_array_ref());
        astrs_data::SchemaHash::of(batch.schema())
    };
    assert_eq!(
        report.schema_hash,
        Some(expected_hash),
        "the observed frame really is a one-row Int64 batch"
    );

    let text = String::from_utf8(out).expect("utf8 output");
    assert!(text.contains(&expected_hash.to_string()), "{text}");

    daemon.join().expect("the scripted daemon thread");
}
