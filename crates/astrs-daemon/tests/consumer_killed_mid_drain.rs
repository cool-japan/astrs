//! §12 conformance zoo, scenario 12/16 (consumer-killed-mid-drain) —
//! blueprint §12, §20.3.
//!
//! The mirror of `shm_lifecycle.rs`'s scenario 11
//! (`a_killed_producer_closes_its_consumers_inputs_and_reclaims_the_ring`),
//! with the roles reversed: here the **consumer** is a real child process,
//! spawned through the daemon's own supervision path so `SIGKILL` targets a
//! genuine pid, while the producer keeps publishing into a queue the
//! consumer never drains before it dies.
//!
//! ```text
//!   camera registers, publishes 3 frames ──► detect's queue (queue_size 4):
//!                                             none of them ever read
//!   detect killed (SIGKILL) ────────────────► daemon reaps it, typed cause
//!   camera keeps running, keeps publishing ──► no hang, no error, no cascade
//! ```
//!
//! Unlike a producer's death, a consumer's death has nothing to close
//! downstream of it (`detect` has no outputs) and nothing to notify
//! upstream of it either — §11.2's "both `queue_policy` values drop rather
//! than stall" is exactly the property this is: a producer is never made to
//! wait on a consumer, dead or slow, so this scenario's proof is an absence
//! — the daemon's own pump loop keeps completing, and `camera` is left
//! exactly as it was.
//!
//! As in `shm_lifecycle.rs`, both nodes are scripted over an in-process
//! duplex ([`FakeNode`]) rather than through real node-API processes talking
//! the wire protocol — the node API is a later wave than this daemon-level
//! harness — but `detect` is *also* a real `/bin/sleep 600` child, spawned
//! through [`Daemon::spawn_node`] purely to give the daemon's own process
//! supervision (and this test's `SIGKILL`) a genuine target. There is no
//! node listener configured (`ListenConfig::none()`), so the real child
//! never itself speaks the protocol — nothing here does, except the scripted
//! duplex — which is exactly how `shm_lifecycle.rs`'s own crash path rigs
//! `camera`, just mirrored onto the other node.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use astrs_daemon::server::NodeListeners;
use astrs_daemon::state::NodeState;
use astrs_daemon::{Daemon, DaemonConfig, ListenConfig, RuntimePaths, plan_dataflow};
use astrs_manifest::Manifest;
use astrs_wire::io::{AsyncFrameReader, AsyncFrameWriter};
use astrs_wire::{
    DataId, DataflowId, FrameLimits, Metadata, NodeEvent, NodeExitCause, NodeHandshake, NodeId,
    NodeRequest, OutputPayload, SessionId,
};
use std::collections::BTreeMap;
use tokio::io::DuplexStream;

/// A per-thread identity, stable within one test — see `shm_lifecycle.rs`'s
/// identical helper for why the pid is mixed in too.
fn test_id() -> u32 {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    thread_local! {
        static ID: Cell<u32> = const { Cell::new(0) };
    }
    ID.with(|id| {
        if id.get() == 0 {
            id.set(NEXT.fetch_add(1, Ordering::Relaxed));
        }
        id.get()
    })
}

fn dataflow() -> DataflowId {
    DataflowId::from_u128(
        (u128::from(std::process::id()) << 32) | u128::from(0x6000_0000u32 + test_id()),
    )
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}

fn data(name: &str) -> DataId {
    DataId::new(name).unwrap()
}

/// Both nodes declare a real `path:` so [`Daemon::spawn_node`] has
/// something to exec — only `detect` is ever actually spawned that way in
/// this file; `camera` is scripted directly (see [`pretend_spawned`]).
const PIPELINE: &str = "\
nodes:
  - id: camera
    path: /bin/sleep
    args: [\"600\"]
    outputs: [image]
  - id: detect
    path: /bin/sleep
    args: [\"600\"]
    inputs:
      frames:
        source: camera/image
        queue_size: 4
";

/// A runtime directory short enough for `SUN_LEN` (104 bytes on macOS).
fn runtime_root() -> PathBuf {
    std::env::temp_dir().join(format!("as-drain{}-{}", std::process::id(), test_id()))
}

/// A daemon with no node listener — every node in this file reaches it
/// through [`FakeNode`]'s in-process duplex, real child process or not.
fn daemon() -> Daemon {
    let root = runtime_root();
    let _ = std::fs::create_dir_all(&root);
    let config = DaemonConfig::new(RuntimePaths::under(root))
        .with_listen(ListenConfig::none())
        .with_shm(false)
        .with_spawn_deadline(Duration::from_secs(120));
    let mut daemon = Daemon::new(config).expect("a daemon");
    let manifest = Manifest::from_yaml_str(PIPELINE).expect("a manifest");
    let plan = plan_dataflow(dataflow(), &manifest, &BTreeMap::new()).expect("a plan");
    daemon.admit(&plan).expect("admitted");
    daemon
}

/// One scripted node.
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
            Duration::from_millis(400),
            self.reader.read_message::<NodeEvent>(),
        )
        .await
        .ok()?
        .ok()?
    }
}

/// Runs the daemon's loop for a slice.
async fn pump(daemon: &mut Daemon) {
    daemon.pump(Duration::from_millis(120)).await;
}

/// Marks a node as spawned with this process's pid — used only for `camera`
/// here, which is never killed, exactly as `shm_lifecycle.rs` uses it for
/// whichever side of its own crash test is not the kill target.
fn pretend_spawned(daemon: &mut Daemon, name: &str) {
    daemon
        .state_mut()
        .dataflow_mut(dataflow())
        .expect("admitted")
        .node_mut(&node(name))
        .expect("declared")
        .mark_spawning(std::process::id());
}

/// Registers a scripted node at generation zero.
async fn register(fake: &mut FakeNode, name: &str) {
    fake.send(NodeRequest::Register(NodeHandshake::new(
        dataflow(),
        node(name),
        0,
    )))
    .await;
}

/// A killed consumer is reaped with a typed cause, its unread queue is not
/// leaked into a hang, and — the property that matters most — its producer
/// notices nothing at all: no error, no block, no cascade.
#[tokio::test]
async fn a_killed_consumer_does_not_wedge_its_producer_or_leak_its_queue() {
    let mut daemon = daemon();

    // `camera`: scripted, never killed, the node whose behaviour this test
    // is really about.
    pretend_spawned(&mut daemon, "camera");
    let mut camera = FakeNode::attach(&mut daemon);
    register(&mut camera, "camera").await;
    pump(&mut daemon).await;
    let _ = camera.recv().await;

    // `detect`: a real child process (so `SIGKILL` has a genuine pid),
    // spawned through the ordinary supervision path exactly as
    // `shm_lifecycle.rs`'s `a_killed_producer_...` spawns its own kill
    // target — then scripted over its own separate duplex, because nothing
    // in this harness has a node listener for the real child to ever dial.
    daemon.spawn_node(dataflow(), &node("detect"));
    pump(&mut daemon).await;
    let pid = daemon
        .dataflow(dataflow())
        .and_then(|state| state.node(&node("detect")))
        .and_then(NodeState::pid)
        .expect("the child was spawned");
    assert!(pid > 0);

    let mut detect = FakeNode::attach(&mut daemon);
    register(&mut detect, "detect").await;
    pump(&mut daemon).await;
    let _ = detect.recv().await;

    detect
        .send(NodeRequest::Subscribe {
            inputs: vec![data("frames")],
        })
        .await;
    pump(&mut daemon).await;

    // `camera` publishes three frames — inside `queue_size: 4`, so this is
    // genuinely "queued and undrained", not "dropped by ordinary overflow"
    // — and `detect` never once asks for them.
    for value in 0u8..3 {
        camera
            .send(NodeRequest::SendMessage {
                output: data("image"),
                metadata: Metadata::default(),
                payload: OutputPayload::Inline { bytes: vec![value] },
            })
            .await;
        pump(&mut daemon).await;
    }

    // `kill -9`: no goodbye, no chance to drain what is already queued
    // (§12: a truthful crash, not a clean close).
    let raw = i32::try_from(pid).expect("a legal pid");
    let target = rustix::process::Pid::from_raw(raw).expect("a legal pid");
    rustix::process::kill_process(target, rustix::process::Signal::KILL)
        .expect("the child is ours to kill");

    let mut terminal = false;
    for _ in 0..10 {
        pump(&mut daemon).await;
        terminal = daemon
            .dataflow(dataflow())
            .and_then(|state| state.node(&node("detect")))
            .is_some_and(NodeState::is_terminal);
        if terminal {
            break;
        }
    }
    assert!(
        terminal,
        "the daemon must reap a killed consumer rather than leave it Spawning/Running forever"
    );

    match daemon
        .dataflow(dataflow())
        .and_then(|state| state.node(&node("detect")))
        .and_then(NodeState::exit_cause)
    {
        Some(NodeExitCause::Signal { name, .. }) => {
            assert!(name.starts_with("SIG"), "{name}");
        }
        other => panic!("expected a signal death for the killed consumer, got {other:?}"),
    }

    // The property this scenario exists to prove: the producer is entirely
    // unaffected by a downstream death — still tracked, still running, and
    // a further publish neither blocks nor errors (§11.2: queues drop
    // rather than stall, and a producer is never blocked by what happens to
    // a consumer's queue).
    assert!(
        !daemon
            .dataflow(dataflow())
            .and_then(|state| state.node(&node("camera")))
            .is_some_and(NodeState::is_terminal),
        "a dead downstream consumer must not take its producer down with it"
    );
    camera
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::Inline { bytes: vec![99] },
        })
        .await;
    pump(&mut daemon).await;
    assert!(
        !daemon
            .dataflow(dataflow())
            .and_then(|state| state.node(&node("camera")))
            .is_some_and(NodeState::is_terminal),
        "publishing into a dead consumer's now-gone queue must not fail the producer"
    );
}
