//! Params + topic introspection, end to end (blueprint §17): the
//! daemon-side half of `CLI → coordinator → owning daemon → node
//! ParamUpdate/ParamDeleted`.
//!
//! `astrs-coordinator`'s own tests already prove the CLI-to-coordinator leg
//! (`params_round_trip_across_independent_cli_sessions`,
//! `a_param_survives_a_coordinator_restart_against_its_store_file`) and that
//! `handlers::dispatch_param_update` resolves
//! [`astrs_wire::ParamScope::Node`] down to "only the one daemon hosting
//! that node" before ever sending a [`CoordinatorEvent`]
//! (`astrs-coordinator::handlers::dispatch_param_update`'s own doc comment).
//! What only this crate can prove is the *last* hop that narrowing depends
//! on: that a real [`Daemon`] receiving that instruction hands
//! `NodeEvent::ParamUpdate`/`ParamDeleted` to exactly the node(s) the scope
//! named — not, as a fan-out-to-everyone implementation would, to every
//! other live node sharing the same dataflow and daemon.
//!
//! Real `Daemon`, real `NodeRequest`/`NodeEvent` framing over an in-process
//! duplex (`FakeNode`, cribbed from `daemon_metrics.rs`'s own helper of the
//! same shape) — the only thing stood in for is the coordinator's own TCP
//! leg, which `handle_coordinator_frame` cannot tell from a real one: it is
//! handed exactly the [`CoordinatorEvent`] a real coordinator would have
//! sent, over the same typed API `astrs-coordinator`'s session loop calls.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::time::Duration;

use astrs_daemon::server::NodeListeners;
use astrs_daemon::{Daemon, DaemonConfig, ListenConfig, RuntimePaths, plan_dataflow};
use astrs_manifest::Manifest;
use astrs_wire::io::{AsyncFrameReader, AsyncFrameWriter};
use astrs_wire::{
    CoordinatorEvent, DataflowId, FrameLimits, NodeEvent, NodeHandshake, NodeId, NodeRequest,
    ParamKey, ParamScope, Parameter, SessionId,
};

fn dataflow() -> DataflowId {
    DataflowId::from_u128(0x000A_AA2A_DE11)
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}

fn key(name: &str) -> ParamKey {
    ParamKey::new(name).unwrap()
}

/// Two independent dynamic nodes in one dataflow — no wiring between them
/// matters here, only that both are live nodes of the *same* dataflow on
/// the *same* daemon, which is exactly the configuration a fan-out bug
/// would leak across.
const TWO_NODES: &str = "\
nodes:
  - id: camera
    path: dynamic
    outputs: [image]
  - id: planner
    path: dynamic
    outputs: [route]
";

fn daemon(name: &str) -> Daemon {
    // Short on purpose: the daemon's Unix socket lives inside this
    // directory and a socket path has a hard length limit (104 bytes on
    // macOS) that a longer temporary directory name plus `daemon.sock`
    // can exceed (see `bins/astrs-cli/tests/cluster_e2e.rs::scratch`, the
    // same constraint).
    let root = std::env::temp_dir().join(format!("as-pd-{name}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&root);
    let config = DaemonConfig::new(RuntimePaths::under(root))
        .with_listen(ListenConfig::none())
        .with_shm(false);
    let mut daemon = Daemon::new(config).expect("a daemon");
    let manifest = Manifest::from_yaml_str(TWO_NODES).expect("a manifest");
    let plan = plan_dataflow(dataflow(), &manifest, &BTreeMap::new()).expect("a plan");
    daemon.admit(&plan).expect("admitted");
    daemon
}

struct FakeNode {
    writer: AsyncFrameWriter<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    reader: AsyncFrameReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
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

    async fn register(&mut self, name: &str) {
        self.send(NodeRequest::Register(NodeHandshake::dynamic(
            dataflow(),
            node(name),
        )))
        .await;
    }

    /// Registers, pumps once, and drains the `Registered` acknowledgement
    /// the daemon answers with — so a caller's next [`Self::recv`] sees
    /// only what the *test* provoked, not the handshake's own echo.
    async fn join(daemon: &mut Daemon, name: &str) -> Self {
        let mut fake = Self::attach(daemon);
        fake.register(name).await;
        pump(daemon).await;
        match fake.recv().await {
            Some(NodeEvent::Registered { .. }) => {}
            other => panic!("expected a `Registered` acknowledgement, got {other:?}"),
        }
        fake
    }

    /// Waits up to a short, generous deadline for one [`NodeEvent`] —
    /// generous enough that a loaded CI machine never fails spuriously,
    /// bounded so a genuine "this must never arrive" assertion actually
    /// terminates rather than hanging the suite.
    async fn recv(&mut self) -> Option<NodeEvent> {
        tokio::time::timeout(
            Duration::from_millis(300),
            self.reader.read_message::<NodeEvent>(),
        )
        .await
        .ok()?
        .ok()?
    }
}

async fn pump(daemon: &mut Daemon) {
    daemon.pump(Duration::from_millis(80)).await;
}

/// The heart of the loop proof: a node-scoped `SetParam` reaches the node
/// it named, on its `astrs.status` port, as a real, framed
/// `NodeEvent::ParamUpdate` — and reaches *only* that node, not the other
/// live node sharing its dataflow and daemon.
#[tokio::test]
async fn a_node_scoped_param_update_reaches_only_the_named_node() {
    let mut daemon = daemon("node-scope-update");
    let mut camera = FakeNode::join(&mut daemon, "camera").await;
    let mut planner = FakeNode::join(&mut daemon, "planner").await;

    let scope = ParamScope::node(dataflow(), node("camera"));
    daemon.handle_coordinator_frame(CoordinatorEvent::SetParam {
        scope: scope.clone(),
        key: key("exposure"),
        value: Parameter::Integer(30),
    });
    pump(&mut daemon).await;

    match camera.recv().await.expect("camera must see its own update") {
        NodeEvent::ParamUpdate {
            scope: got_scope,
            key: got_key,
            value,
        } => {
            assert_eq!(got_scope, scope);
            assert_eq!(got_key.as_str(), "exposure");
            assert_eq!(value, Parameter::Integer(30));
        }
        other => panic!("expected ParamUpdate, got {other:?}"),
    }

    assert!(
        planner.recv().await.is_none(),
        "a node-scoped update for `camera` must not reach `planner`, a sibling \
         node on the same daemon and dataflow that the scope never named"
    );
}

/// The delete half of the same narrowing, proven independently: a
/// node-scoped `DeleteParam` is exactly as targeted as `SetParam`.
#[tokio::test]
async fn a_node_scoped_param_delete_reaches_only_the_named_node() {
    let mut daemon = daemon("node-scope-delete");
    let mut camera = FakeNode::join(&mut daemon, "camera").await;
    let mut planner = FakeNode::join(&mut daemon, "planner").await;

    let scope = ParamScope::node(dataflow(), node("planner"));
    daemon.handle_coordinator_frame(CoordinatorEvent::DeleteParam {
        scope: scope.clone(),
        key: key("goal"),
    });
    pump(&mut daemon).await;

    match planner
        .recv()
        .await
        .expect("planner must see its own delete")
    {
        NodeEvent::ParamDeleted {
            scope: got_scope,
            key: got_key,
        } => {
            assert_eq!(got_scope, scope);
            assert_eq!(got_key.as_str(), "goal");
        }
        other => panic!("expected ParamDeleted, got {other:?}"),
    }

    assert!(
        camera.recv().await.is_none(),
        "a node-scoped delete for `planner` must not reach `camera`"
    );
}

/// The other two scopes are deliberately *not* narrowed: a dataflow-wide
/// default is relevant to every node of that dataflow that has not
/// overridden it, so both nodes must see it.
#[tokio::test]
async fn a_dataflow_scoped_param_update_reaches_every_live_node_of_that_dataflow() {
    let mut daemon = daemon("dataflow-scope");
    let mut camera = FakeNode::join(&mut daemon, "camera").await;
    let mut planner = FakeNode::join(&mut daemon, "planner").await;

    let scope = ParamScope::dataflow_scope(dataflow());
    daemon.handle_coordinator_frame(CoordinatorEvent::SetParam {
        scope: scope.clone(),
        key: key("rate_hz"),
        value: Parameter::Float(30.0),
    });
    pump(&mut daemon).await;

    for fake in [&mut camera, &mut planner] {
        match fake
            .recv()
            .await
            .expect("every node of the dataflow must see it")
        {
            NodeEvent::ParamUpdate { scope: got, .. } => assert_eq!(got, scope),
            other => panic!("expected ParamUpdate, got {other:?}"),
        }
    }
}
