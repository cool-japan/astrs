//! §12 conformance zoo, scenario 14/16 (late-dynamic attach) — blueprint
//! §8.3, §11.2, §12, §20.3.
//!
//! A `path: dynamic` node is deliberately excluded from the cluster's
//! `AllNodesReady` barrier (§12: "a node with no daemon-owned process ...
//! is never deferred: no reap will arrive to contradict the closure") so a
//! graph does not block on an operator who has not run `astrs node attach`
//! or `astrs topic pub` yet. What that exclusion is *for* — an operator
//! genuinely showing up late, to an already-active graph, and being
//! admitted correctly — has no coverage anywhere in this crate before this
//! file: `cluster_m2.rs`'s own barrier test
//! (`a_non_detached_start_is_answered_when_a_node_never_registers`) only
//! proves the barrier does not *wait* for a dynamic node; its own dynamic
//! node never attaches at all, so nothing there proves a late attach
//! actually *works*.
//!
//! ```text
//!   camera attaches (dynamic) ─► publishes seq 0..8 (== queue_size) ─► queued, nobody draining yet
//!            │
//!            │  (the graph is already live and producing — detect is late)
//!            ▼
//!   detect attaches (dynamic), subscribes ─► drains the 8 queued frames
//!            │
//!            ▼
//!   camera publishes seq 8..13 ─► delivered as they arrive, same queue
//! ```
//!
//! # What "late" actually means here — corrected against the real daemon
//!
//! This file's first draft assumed a late attach joins a hot stream and
//! gets nothing published before it, the way a fresh subscriber to a
//! live broadcast would. Running it against the real daemon proved that
//! assumption wrong: an input's queue exists and fills from the moment its
//! producer starts publishing (the edge is declared in the manifest, so the
//! daemon does not need the consumer to have registered to know the queue
//! exists), exactly as it would for a consumer that *had* registered but
//! was simply slow to call `NextEvent`. A late dynamic attach is therefore
//! not a different code path from an ordinary slow one — both are §11.2's
//! bounded queue, `queue_size` deep, `DropOldest` beyond that (the crate
//! default). What this test proves is the version of that story that is
//! actually true: a late attacher receives whatever still fits in the
//! queue (here, exactly `queue_size` frames sent before it ever showed up,
//! chosen so nothing is dropped and the count is unambiguous), and then
//! keeps receiving everything published afterward without a gap.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use astrs_daemon::server::NodeListeners;
use astrs_daemon::{Daemon, DaemonConfig, ListenConfig, RuntimePaths, plan_dataflow};
use astrs_manifest::Manifest;
use astrs_wire::io::{AsyncFrameReader, AsyncFrameWriter};
use astrs_wire::{
    DataId, DataflowId, FrameLimits, Metadata, NodeEvent, NodeHandshake, NodeId, NodeRequest,
    OutputPayload, SessionId,
};
use tokio::io::DuplexStream;

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
        (u128::from(std::process::id()) << 32) | u128::from(0x7000_0000u32 + test_id()),
    )
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}

fn data(name: &str) -> DataId {
    DataId::new(name).unwrap()
}

/// `queue_size` for `detect/frames` — also this test's pre-attach send
/// count, chosen so every pre-attach frame fits with nothing dropped and
/// the overflow policy (`DropOldest`, the crate default — see
/// `astrs_manifest::node::io::QueuePolicy`) never has to make a choice.
const QUEUE_SIZE: u8 = 8;

/// Both nodes are `path: dynamic` — neither is ever spawned by the daemon,
/// so the only thing that decides when either joins is when this test
/// registers it.
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

fn runtime_root() -> PathBuf {
    std::env::temp_dir().join(format!("as-late{}-{}", std::process::id(), test_id()))
}

fn daemon() -> Daemon {
    let root = runtime_root();
    let _ = std::fs::create_dir_all(&root);
    let config = DaemonConfig::new(RuntimePaths::under(root))
        .with_listen(ListenConfig::none())
        .with_shm(false);
    let mut daemon = Daemon::new(config).expect("a daemon");
    let manifest = Manifest::from_yaml_str(PIPELINE).expect("a manifest");
    let plan = plan_dataflow(dataflow(), &manifest, &BTreeMap::new()).expect("a plan");
    daemon.admit(&plan).expect("admitted");
    daemon
}

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

    /// Drains whatever is ready right now, giving up after one short
    /// timeout finds nothing further.
    async fn drain(&mut self) -> Vec<NodeEvent> {
        let mut events = Vec::new();
        while let Some(event) = self.recv().await {
            events.push(event);
        }
        events
    }

    async fn attach_dynamic(daemon: &mut Daemon, name: &str) -> Self {
        let mut fake = Self::attach(daemon);
        fake.send(NodeRequest::Register(NodeHandshake::dynamic(
            dataflow(),
            node(name),
        )))
        .await;
        fake
    }
}

async fn pump(daemon: &mut Daemon) {
    daemon.pump(Duration::from_millis(60)).await;
}

fn seq_payload(seq: u8) -> Vec<u8> {
    vec![seq]
}

async fn publish(daemon: &mut Daemon, camera: &mut FakeNode, seq: u8) {
    camera
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::Inline {
                bytes: seq_payload(seq),
            },
        })
        .await;
    pump(daemon).await;
}

/// Every `frames` sequence number in `events`, in the order they arrived.
fn received_seqs(events: &[NodeEvent]) -> Vec<u8> {
    events
        .iter()
        .filter_map(|event| match event {
            NodeEvent::Input { id, payload, .. } if *id == data("frames") => Some(payload[0]),
            _ => None,
        })
        .collect()
}

/// A dynamic node that attaches well after its graph is already live and
/// producing joins correctly, is handed what still fits in its queue from
/// before it existed, and keeps receiving everything published afterward.
#[tokio::test]
async fn a_dynamic_node_that_attaches_late_joins_the_already_running_graph() {
    let mut daemon = daemon();

    // camera attaches first, and the graph is already "running" as far as
    // it is concerned — it publishes exactly `QUEUE_SIZE` frames with
    // nobody yet registered as `detect`.
    let mut camera = FakeNode::attach_dynamic(&mut daemon, "camera").await;
    pump(&mut daemon).await;
    let _ = camera.recv().await;

    for seq in 0..QUEUE_SIZE {
        publish(&mut daemon, &mut camera, seq).await;
    }
    assert!(
        daemon.dataflow(dataflow()).is_some(),
        "the dataflow must stay live while only camera has ever attached"
    );

    // detect is late: it only shows up now, well after camera has been
    // producing — and the attach itself must not be refused or delayed by
    // the graph already being active.
    let mut detect = FakeNode::attach_dynamic(&mut daemon, "detect").await;
    pump(&mut daemon).await;
    let registered = detect.recv().await;
    assert!(
        registered.is_some(),
        "a late dynamic attach must still be answered, not silently ignored"
    );

    detect
        .send(NodeRequest::Subscribe {
            inputs: vec![data("frames")],
        })
        .await;
    pump(&mut daemon).await;

    // What was already queued before the late attach — up to `QUEUE_SIZE`,
    // nothing dropped, since exactly `QUEUE_SIZE` were sent (§11.2: the
    // bound is the point of the scenario, not an incidental detail).
    let caught_up = received_seqs(&detect.drain().await);
    assert_eq!(
        caught_up,
        (0..QUEUE_SIZE).collect::<Vec<_>>(),
        "a late attach must receive whatever still fits in its queue, in order, \
         with nothing dropped when the backlog is within `queue_size`"
    );

    // Traffic published after the attach keeps arriving without a gap.
    for seq in QUEUE_SIZE..QUEUE_SIZE + 5 {
        publish(&mut daemon, &mut camera, seq).await;
    }
    let after_attach = received_seqs(&detect.drain().await);
    assert_eq!(
        after_attach,
        (QUEUE_SIZE..QUEUE_SIZE + 5).collect::<Vec<_>>(),
        "every frame published after the late attach must be delivered, in order, \
         picking up exactly where the catch-up left off"
    );
}
