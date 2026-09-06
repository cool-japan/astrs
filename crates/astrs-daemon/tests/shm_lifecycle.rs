//! The slow-start route handshake against real shared memory (§6.2, §6.3).
//!
//! Real segments, real descriptor passing over the broker's Unix socket, real
//! consumer-table observation, and — for the crash path — a real child process
//! killed with `SIGKILL`. The nodes are scripted
//! ([`astrs_wire::NodeRequest`] frames over an in-process duplex) because the
//! node API is a later wave; everything below the node boundary is the
//! production code path.
//!
//! ```text
//!   detect subscribes ─────────────► segment created, detect expected
//!   detect attaches (SegmentClient) ─► consumer table entry with its pid
//!   daemon tick ───────────────────► RouteUpgrade to camera
//!   camera acknowledges ───────────► the route table says `shm`
//!   detect detaches ───────────────► RouteDowngrade, reliable path resumes
//!   camera killed (SIGKILL) ───────► InputClosed to detect, ring unlinked
//! ```
//!
//! `a_killed_producer_closes_its_consumers_inputs_and_reclaims_the_ring`
//! below is §12's conformance zoo scenario 11/16 (producer-killed-mid-slot,
//! SHM) — see the zoo's full 16-scenario list in §12 and the mapping note in
//! `crates/astrs-daemon/tests/node_roles.rs`. The mirror scenario,
//! consumer-killed-mid-drain (12/16), lives in
//! `crates/astrs-daemon/tests/consumer_killed_mid_drain.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use astrs_daemon::server::NodeListeners;
use astrs_daemon::shm::{OutputKey, UpgradeState};
use astrs_daemon::state::DeliveryPlane;
use astrs_daemon::{Daemon, DaemonConfig, ListenConfig, RuntimePaths, plan_dataflow};
use astrs_manifest::Manifest;
use astrs_shm::{AttachOptions, Consumer, SegmentClient};
use astrs_wire::WireEncode;
use astrs_wire::io::{AsyncFrameReader, AsyncFrameWriter};
use astrs_wire::{
    DataId, DataflowId, FrameLimits, NodeEvent, NodeHandshake, NodeId, NodeRequest, PortRef,
    SessionId,
};
use tokio::io::DuplexStream;

/// A per-thread identity, stable within one test.
///
/// A thread-local counter distinguishes tests that share a process. It is
/// deliberately *not* enough on its own to name anything machine-global — see
/// [`dataflow`], which adds the process identity.
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

/// The dataflow every test in this file works in.
///
/// The POSIX shared-memory name a segment gets is derived from this id, and
/// that namespace is *machine-global*, not per-process. `cargo nextest` runs
/// each test in its own process, where a thread-local counter restarts from
/// the same seed — so on the counter alone every test in every process picks
/// the same id, and two running concurrently fail `shm_open` with `EEXIST`.
/// Mixing in the pid makes the id unique across processes as well as threads,
/// under either test runner.
fn dataflow() -> DataflowId {
    DataflowId::from_u128(
        (u128::from(std::process::id()) << 32) | u128::from(0x5000_0000u32 + test_id()),
    )
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}

fn data(name: &str) -> DataId {
    DataId::new(name).unwrap()
}

fn source() -> PortRef {
    "camera/image".parse().unwrap()
}

fn output_key() -> OutputKey {
    OutputKey::new(dataflow(), node("camera"), data("image"))
}

/// Both nodes are spawned nodes (`path:` names an executable), because a
/// `path: dynamic` consumer is never offered a ring (§8.3, §6.3).
const PIPELINE: &str = "\
nodes:
  - id: camera
    path: /bin/sleep
    args: [\"600\"]
    shm_pool_size: 262144
    outputs: [image]
  - id: detect
    path: /bin/sleep
    args: [\"600\"]
    inputs:
      frames:
        source: camera/image
        queue_size: 8
";

/// A runtime directory short enough for `SUN_LEN` (104 bytes on macOS).
fn runtime_root() -> PathBuf {
    std::env::temp_dir().join(format!("as{}-{}", std::process::id(), test_id()))
}

/// A daemon with the shared-memory plane on and no node listeners.
fn daemon() -> Daemon {
    let root = runtime_root();
    let _ = std::fs::create_dir_all(&root);
    let config = DaemonConfig::new(RuntimePaths::under(root))
        .with_listen(ListenConfig::none())
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

    /// Reads until an event matching `wanted` arrives, or the stream goes
    /// quiet.
    async fn wait_for(&mut self, wanted: fn(&NodeEvent) -> bool) -> Option<NodeEvent> {
        while let Some(event) = self.recv().await {
            if wanted(&event) {
                return Some(event);
            }
        }
        None
    }
}

/// Runs the daemon's loop for a slice.
async fn pump(daemon: &mut Daemon) {
    daemon.pump(Duration::from_millis(120)).await;
}

/// Marks a node as spawned with this process's pid, so the consumer-table
/// entry a test attaches with maps back to the graph node.
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

/// Brings up a daemon with both nodes registered and `detect` subscribed.
async fn subscribed() -> (Daemon, FakeNode, FakeNode) {
    let mut daemon = daemon();
    pretend_spawned(&mut daemon, "camera");
    pretend_spawned(&mut daemon, "detect");

    let mut camera = FakeNode::attach(&mut daemon);
    let mut detect = FakeNode::attach(&mut daemon);
    register(&mut camera, "camera").await;
    register(&mut detect, "detect").await;
    pump(&mut daemon).await;
    let _ = camera.recv().await;
    let _ = detect.recv().await;

    detect
        .send(NodeRequest::Subscribe {
            inputs: vec![data("frames")],
        })
        .await;
    pump(&mut daemon).await;
    (daemon, camera, detect)
}

#[tokio::test]
async fn the_plane_is_brokered_on_a_socket_nodes_can_reach() {
    let daemon = daemon();
    assert!(daemon.shm().is_enabled(), "this platform has a plane");
    let socket = daemon.shm().socket_path().expect("a broker socket");
    assert!(socket.exists(), "{}", socket.display());
    assert!(
        socket.as_os_str().len() < 104,
        "a broker socket must fit SUN_LEN"
    );
}

#[tokio::test]
async fn a_subscription_creates_the_producer_ring() {
    let (daemon, _camera, _detect) = subscribed().await;

    assert_eq!(daemon.shm().segment_count(), 1);
    let spec = daemon
        .shm()
        .registry()
        .spec(&output_key())
        .expect("a segment for camera/image");
    assert_eq!(spec.generation, 0);
    assert!(spec.name.ends_with("/camera/image/0"), "{}", spec.name);
    assert_eq!(
        daemon.shm().ledger().expected(&output_key()),
        vec![node("detect")],
        "the one static same-host consumer is expected to attach"
    );
    assert_eq!(daemon.shm().state(&output_key()), UpgradeState::Reliable);
}

#[tokio::test]
async fn no_upgrade_is_offered_until_the_consumer_attaches() {
    let (mut daemon, mut camera, _detect) = subscribed().await;

    pump(&mut daemon).await;
    assert_eq!(
        daemon.shm().state(&output_key()),
        UpgradeState::Reliable,
        "nobody has mapped the ring yet"
    );
    assert!(
        camera
            .wait_for(|event| event.is_route_change())
            .await
            .is_none(),
        "the producer was told nothing"
    );
}

#[tokio::test]
async fn the_upgrade_is_offered_once_every_consumer_has_attached() {
    let (mut daemon, mut camera, _detect) = subscribed().await;

    // The consumer attaches the way a node does: over the broker socket, with
    // the descriptor passed by `SCM_RIGHTS`.
    let socket = daemon
        .shm()
        .socket_path()
        .expect("a broker socket")
        .to_path_buf();
    let mut client = SegmentClient::connect(&socket).expect("the broker answers");
    let segment = client
        .attach(&output_key().segment_key(0))
        .expect("a descriptor");
    let _consumer = Consumer::attach(segment.shared(), AttachOptions::default())
        .expect("the ring accepts a reader");

    pump(&mut daemon).await;

    assert!(
        daemon.shm().state(&output_key()).is_offered(),
        "{:?}",
        daemon.shm().state(&output_key())
    );
    let offer = camera
        .wait_for(|event| matches!(event, NodeEvent::RouteUpgrade { .. }))
        .await
        .expect("the producer was offered the ring");
    match offer {
        NodeEvent::RouteUpgrade {
            output, consumers, ..
        } => {
            assert_eq!(output, data("image"));
            assert_eq!(consumers.len(), 1);
        }
        other => panic!("unexpected {other:?}"),
    }

    // The daemon keeps brokering until the producer acknowledges (§6.3).
    let plane = daemon
        .dataflow(dataflow())
        .and_then(|state| {
            state
                .routes()
                .consumers(&source())
                .iter()
                .find(|consumer| consumer.node == node("detect"))
                .map(|consumer| consumer.plane.clone())
        })
        .expect("the route exists");
    assert_eq!(plane, DeliveryPlane::Local);
}

#[tokio::test]
async fn an_acknowledged_upgrade_moves_the_route_onto_the_ring() {
    let (mut daemon, mut camera, _detect) = subscribed().await;
    let socket = daemon
        .shm()
        .socket_path()
        .expect("a broker socket")
        .to_path_buf();
    let mut client = SegmentClient::connect(&socket).expect("the broker answers");
    let segment = client
        .attach(&output_key().segment_key(0))
        .expect("a descriptor");
    let _consumer = Consumer::attach(segment.shared(), AttachOptions::default()).expect("a reader");
    pump(&mut daemon).await;
    let _ = camera
        .wait_for(|event| matches!(event, NodeEvent::RouteUpgrade { .. }))
        .await;

    camera
        .send(NodeRequest::RouteUpgradeAck {
            output: data("image"),
            accepted: true,
            reason: None,
        })
        .await;
    pump(&mut daemon).await;

    assert!(daemon.shm().is_upgraded(&output_key()));
    let plane = daemon
        .dataflow(dataflow())
        .and_then(|state| {
            state
                .routes()
                .consumers(&source())
                .iter()
                .find(|consumer| consumer.node == node("detect"))
                .map(|consumer| consumer.plane.clone())
        })
        .expect("the route exists");
    assert!(matches!(plane, DeliveryPlane::Shm { .. }), "{plane:?}");
    assert_eq!(daemon.metrics().ft_stats().route_upgrades, 1);
}

#[tokio::test]
async fn a_refused_upgrade_keeps_the_reliable_path_and_counts_a_fallback() {
    let (mut daemon, mut camera, _detect) = subscribed().await;
    let socket = daemon
        .shm()
        .socket_path()
        .expect("a broker socket")
        .to_path_buf();
    let mut client = SegmentClient::connect(&socket).expect("the broker answers");
    let segment = client
        .attach(&output_key().segment_key(0))
        .expect("a descriptor");
    let _consumer = Consumer::attach(segment.shared(), AttachOptions::default()).expect("a reader");
    pump(&mut daemon).await;
    let _ = camera
        .wait_for(|event| matches!(event, NodeEvent::RouteUpgrade { .. }))
        .await;

    camera
        .send(NodeRequest::RouteUpgradeAck {
            output: data("image"),
            accepted: false,
            reason: Some("cannot map the segment".into()),
        })
        .await;
    pump(&mut daemon).await;

    assert!(!daemon.shm().is_upgraded(&output_key()));
    assert_eq!(daemon.shm().state(&output_key()).as_str(), "refused");
    assert_eq!(daemon.shm().fallbacks(), 1);
}

#[tokio::test]
async fn a_detaching_consumer_downgrades_the_route() {
    let (mut daemon, mut camera, _detect) = subscribed().await;
    let socket = daemon
        .shm()
        .socket_path()
        .expect("a broker socket")
        .to_path_buf();
    let mut client = SegmentClient::connect(&socket).expect("the broker answers");
    let segment = client
        .attach(&output_key().segment_key(0))
        .expect("a descriptor");
    let mut consumer =
        Consumer::attach(segment.shared(), AttachOptions::default()).expect("a reader");
    pump(&mut daemon).await;
    let _ = camera
        .wait_for(|event| matches!(event, NodeEvent::RouteUpgrade { .. }))
        .await;
    camera
        .send(NodeRequest::RouteUpgradeAck {
            output: data("image"),
            accepted: true,
            reason: None,
        })
        .await;
    pump(&mut daemon).await;
    assert!(daemon.shm().is_upgraded(&output_key()));

    // The consumer leaves the ring: the route must flip back before the
    // producer publishes into a segment nobody is reading (§6.3).
    consumer.detach();
    pump(&mut daemon).await;

    assert!(!daemon.shm().is_upgraded(&output_key()));
    let downgrade = camera
        .wait_for(|event| matches!(event, NodeEvent::RouteDowngrade { .. }))
        .await
        .expect("the producer was told to go back");
    match downgrade {
        NodeEvent::RouteDowngrade { output, reason } => {
            assert_eq!(output, data("image"));
            assert_eq!(reason.kind_name(), "consumer_detached");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(daemon.metrics().ft_stats().route_downgrades, 1);

    let plane = daemon
        .dataflow(dataflow())
        .and_then(|state| {
            state
                .routes()
                .consumers(&source())
                .iter()
                .find(|consumer| consumer.node == node("detect"))
                .map(|consumer| consumer.plane.clone())
        })
        .expect("the route exists");
    assert_eq!(plane, DeliveryPlane::Local, "the reliable path resumed");
}

#[tokio::test]
async fn a_dynamic_consumer_is_never_offered_a_ring() {
    const DYNAMIC: &str = "\
nodes:
  - id: camera
    path: /bin/sleep
    args: [\"600\"]
    outputs: [image]
  - id: detect
    path: dynamic
    inputs:
      frames: camera/image
";
    let root = runtime_root();
    let _ = std::fs::create_dir_all(&root);
    let config = DaemonConfig::new(RuntimePaths::under(root)).with_listen(ListenConfig::none());
    let mut daemon = Daemon::new(config).expect("a daemon");
    let manifest = Manifest::from_yaml_str(DYNAMIC).expect("a manifest");
    let plan = plan_dataflow(dataflow(), &manifest, &BTreeMap::new()).expect("a plan");
    daemon.admit(&plan).expect("admitted");
    pretend_spawned(&mut daemon, "camera");

    let mut camera = FakeNode::attach(&mut daemon);
    let mut detect = FakeNode::attach(&mut daemon);
    register(&mut camera, "camera").await;
    detect
        .send(NodeRequest::Register(NodeHandshake::dynamic(
            dataflow(),
            node("detect"),
        )))
        .await;
    pump(&mut daemon).await;
    detect
        .send(NodeRequest::Subscribe {
            inputs: vec![data("frames")],
        })
        .await;
    pump(&mut daemon).await;

    assert_eq!(
        daemon.shm().segment_count(),
        0,
        "a dynamic consumer disqualifies the output"
    );
    assert_eq!(daemon.shm().state(&output_key()), UpgradeState::Reliable);
}

#[tokio::test]
async fn a_tapped_output_stays_on_the_daemon_path() {
    let (mut daemon, _camera, _detect) = subscribed().await;
    assert_eq!(daemon.shm().segment_count(), 1);

    daemon.taps_mut().enable(dataflow());
    assert!(daemon.start_tap(
        astrs_wire::SubscriptionId::new(1),
        dataflow(),
        Some(source()),
        None
    ));

    assert_eq!(
        daemon.shm().segment_count(),
        0,
        "a tap needs the daemon to keep seeing the bytes"
    );
}

/// §12 conformance zoo, scenario 11/16 (producer-killed-mid-slot, SHM).
#[tokio::test]
async fn a_killed_producer_closes_its_consumers_inputs_and_reclaims_the_ring() {
    let mut daemon = daemon();
    pretend_spawned(&mut daemon, "detect");
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

    // A real child process: `/bin/sleep 600`, spawned through the ordinary
    // supervision path, with a real pid.
    daemon.spawn_node(dataflow(), &node("camera"));
    pump(&mut daemon).await;
    let pid = daemon
        .dataflow(dataflow())
        .and_then(|state| state.node(&node("camera")))
        .and_then(astrs_daemon::state::NodeState::pid)
        .expect("the child was spawned");
    assert!(pid > 0);

    // `kill -9`: no goodbye, no chance to close its outputs (§6.2 crash
    // safety, §12 error propagation).
    let raw = i32::try_from(pid).expect("a legal pid");
    let target = rustix::process::Pid::from_raw(raw).expect("a legal pid");
    rustix::process::kill_process(target, rustix::process::Signal::KILL)
        .expect("the child is ours to kill");

    detect
        .send(NodeRequest::NextEvent {
            timeout: None,
            max_batch: 8,
        })
        .await;
    for _ in 0..10 {
        pump(&mut daemon).await;
        let terminal = daemon
            .dataflow(dataflow())
            .and_then(|state| state.node(&node("camera")))
            .is_some_and(astrs_daemon::state::NodeState::is_terminal);
        if terminal {
            break;
        }
    }

    let closed = detect
        .wait_for(|event| matches!(event, NodeEvent::InputClosed { .. }))
        .await
        .expect("the consumer was told its input closed");
    match closed {
        NodeEvent::InputClosed { id, reason, .. } => {
            assert_eq!(id, data("frames"));
            assert!(
                !reason.is_expected(),
                "a killed producer is not a clean close: {reason:?}"
            );
        }
        other => panic!("unexpected {other:?}"),
    }

    assert_eq!(
        daemon.shm().segment_count(),
        0,
        "the dead producer's ring was closed and reclaimed"
    );
    assert!(daemon.shm().ledger().is_empty());
}

#[tokio::test]
async fn a_restart_replaces_the_ring_with_a_new_generation() {
    let (mut daemon, _camera, _detect) = subscribed().await;
    let first = daemon
        .shm()
        .registry()
        .spec(&output_key())
        .expect("a segment");

    {
        let state = daemon
            .state_mut()
            .dataflow_mut(dataflow())
            .expect("admitted");
        let camera = state.node_mut(&node("camera")).expect("declared");
        camera.begin_next_generation();
        camera.mark_spawning(std::process::id());
        camera.mark_registered();
    }
    daemon.plan_shm_output(dataflow(), &source());

    let second = daemon
        .shm()
        .registry()
        .spec(&output_key())
        .expect("a segment");
    assert_ne!(first.name, second.name, "a new incarnation, a new ring");
    assert_eq!(second.generation, 1);
    assert_eq!(daemon.shm().segment_count(), 1);
}

#[tokio::test]
async fn a_slot_reference_is_bridged_with_the_producers_own_metadata() {
    let (mut daemon, mut camera, mut detect) = subscribed().await;

    // Bring the route all the way up.
    let socket = daemon
        .shm()
        .socket_path()
        .expect("a broker socket")
        .to_path_buf();
    let mut client = SegmentClient::connect(&socket).expect("the broker answers");
    let attached = client
        .attach(&output_key().segment_key(0))
        .expect("a descriptor");
    let _reader = Consumer::attach(attached.shared(), AttachOptions::default()).expect("a reader");
    pump(&mut daemon).await;
    let _ = camera
        .wait_for(|event| matches!(event, NodeEvent::RouteUpgrade { .. }))
        .await;
    camera
        .send(NodeRequest::RouteUpgradeAck {
            output: data("image"),
            accepted: true,
            reason: None,
        })
        .await;
    pump(&mut daemon).await;
    assert!(daemon.shm().is_upgraded(&output_key()));

    // Something forces the route back onto the daemon path. The producer has
    // not switched yet, so its next message is still a slot reference — and
    // the daemon must deliver it *with the metadata the producer committed*,
    // because that is what correlates a service exchange (§6.1, §9.4).
    assert!(daemon.downgrade_output(
        dataflow(),
        &source(),
        astrs_wire::RouteDowngradeReason::DaemonRequest {
            message: "a rolling reconfiguration".into(),
        },
    ));
    assert!(daemon.shm().bridge().is_attached(&output_key()));

    let segment = std::sync::Arc::clone(
        daemon
            .shm()
            .registry()
            .record(&output_key())
            .expect("the ring is still there")
            .segment(),
    );
    let mut producer = astrs_shm::Producer::new(segment).expect("the producer role");

    let mut metadata = astrs_wire::Metadata::new(astrs_time::HlcTimestamp::new(42, 7));
    metadata.set_request_id("req-17");
    metadata.set_seq(9);
    let meta_bytes = metadata.encode_to_vec().expect("metadata encodes");
    let seq = producer
        .send(b"an arrow batch", &meta_bytes)
        .expect("the slot is committed");

    camera
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: astrs_wire::Metadata::default(),
            payload: astrs_wire::OutputPayload::Shm {
                segment: output_key().segment_key(0).canonical(),
                slot: u32::try_from(seq).unwrap_or(0),
                len: 14,
                generation: 0,
            },
        })
        .await;
    detect
        .send(NodeRequest::NextEvent {
            timeout: None,
            max_batch: 8,
        })
        .await;
    pump(&mut daemon).await;

    let delivered = detect
        .wait_for(|event| matches!(event, NodeEvent::Input { .. }))
        .await
        .expect("the bridged message reached the consumer");
    match delivered {
        NodeEvent::Input {
            id,
            payload,
            metadata,
            ..
        } => {
            assert_eq!(id, data("frames"));
            assert_eq!(payload, b"an arrow batch");
            assert_eq!(
                metadata.request_id(),
                Some("req-17"),
                "the producer's correlation survived the ring"
            );
            assert_eq!(metadata.seq(), Some(9));
            assert_eq!(metadata.timestamp, astrs_time::HlcTimestamp::new(42, 7));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn a_slot_reference_from_a_replaced_incarnation_is_refused() {
    let (mut daemon, mut camera, _detect) = subscribed().await;

    camera
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: astrs_wire::Metadata::default(),
            payload: astrs_wire::OutputPayload::Shm {
                segment: output_key().segment_key(4).canonical(),
                slot: 0,
                len: 8,
                generation: 4,
            },
        })
        .await;
    pump(&mut daemon).await;

    assert_eq!(
        daemon.shm().fallbacks(),
        1,
        "a reference minted before a restart is counted, not delivered"
    );
    assert_eq!(daemon.metrics().ft_stats().shm_fallbacks, 1);
}
