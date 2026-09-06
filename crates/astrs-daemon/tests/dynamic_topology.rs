//! Live dynamic-topology ops end to end (blueprint §8, §17): `astrs node
//! add/remove/replace/connect/disconnect` against a real
//! [`astrs_daemon::Daemon`], driven the same way
//! `tests/session_conversation.rs` drives the ordinary protocol — a
//! scripted fake node over [`tokio::io::duplex`], no coordinator, no
//! sockets, no spawned processes.
//!
//! What each test proves, matching the task brief's own list:
//!
//! - [`live_add_lets_a_new_consumer_start_receiving`]
//! - [`live_remove_closes_the_downstream_consumer_while_an_unrelated_node_keeps_running`]
//! - [`replace_under_load_preserves_message_continuity_across_the_cutover`]
//! - [`edge_add_and_remove_rewire_delivery_live`]

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use astrs_daemon::server::NodeListeners;
use astrs_daemon::{Daemon, DaemonConfig, ListenConfig, RuntimePaths};
use astrs_wire::io::{AsyncFrameReader, AsyncFrameWriter};
use astrs_wire::{
    DataId, DataflowId, FrameLimits, InputSpec, Metadata, NodeEvent, NodeHandshake, NodeId,
    NodeRequest, NodeSource, NodeSpawnSpec, OutputPayload, OutputSpec, PortRef,
};
use tokio::io::DuplexStream;

/// The dataflow every test in this file admits.
fn dataflow() -> DataflowId {
    DataflowId::from_u128(1)
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}

fn data(name: &str) -> DataId {
    DataId::new(name).unwrap()
}

/// A bare, empty dataflow admitted with no nodes at all — every node in
/// these tests arrives through [`Daemon::apply_add_node`], never a
/// manifest, so the "live add" path is exercised for real rather than
/// only for the nodes it adds on top of a pre-planned graph.
fn daemon(tag: &str) -> Daemon {
    let root = std::env::temp_dir().join(format!("astrs-dyntop-{}-{tag}", std::process::id()));
    let _ = std::fs::create_dir_all(&root);
    let config = DaemonConfig::new(RuntimePaths::under(root)).with_listen(ListenConfig::none());
    let mut daemon = Daemon::new(config).expect("a daemon");
    daemon
        .state_mut()
        .insert_dataflow(astrs_daemon::state::DataflowState::new(
            dataflow(),
            astrs_time::HlcTimestamp::EPOCH,
        ));
    daemon
}

/// A dynamic node spec — every node in this file is `path: dynamic`
/// (§8.3), attached the same way [`FakeNode::attach`] does, so no real
/// process is ever spawned.
fn dynamic_spec(id: &str) -> NodeSpawnSpec {
    NodeSpawnSpec::new(dataflow(), node(id), 0, NodeSource::Dynamic)
}

/// One fake node's end of an in-process connection.
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

    async fn recv(&mut self) -> Option<NodeEvent> {
        tokio::time::timeout(
            Duration::from_secs(2),
            self.reader.read_message::<NodeEvent>(),
        )
        .await
        .ok()?
        .ok()?
    }

    /// Reads events until one matching `pred` arrives, or `None` after the
    /// per-`recv` timeout — used to skip past `Registered`/other
    /// bookkeeping events to the `Input`/`InputClosed` a test actually
    /// wants, without a fixed sleep.
    async fn recv_matching(
        &mut self,
        mut pred: impl FnMut(&NodeEvent) -> bool,
    ) -> Option<NodeEvent> {
        for _ in 0..8 {
            match self.recv().await {
                Some(event) if pred(&event) => return Some(event),
                Some(_) => continue,
                None => return None,
            }
        }
        None
    }
}

/// Runs the daemon's loop for long enough to drain everything pending —
/// identical in spirit to `session_conversation.rs`'s own `pump`.
async fn pump(daemon: &mut Daemon) {
    let _ = tokio::time::timeout(Duration::from_millis(400), daemon.run()).await;
}

/// Attaches `FakeNode` as the already-admitted dynamic node `id`'s
/// registration — the daemon-side half of what a real `path: dynamic`
/// process's `Node::init_from_node_id` does over a socket (§8.3).
async fn attach_dynamic(daemon: &mut Daemon, id: &str) -> FakeNode {
    let mut fake = FakeNode::attach(daemon);
    fake.send(NodeRequest::Register(NodeHandshake::dynamic(
        dataflow(),
        node(id),
    )))
    .await;
    pump(daemon).await;
    let registered = fake
        .recv_matching(|event| matches!(event, NodeEvent::Registered { .. }))
        .await;
    assert!(registered.is_some(), "{id} must register");
    fake
}

#[tokio::test]
async fn live_add_lets_a_new_consumer_start_receiving() {
    let mut daemon = daemon("add");
    let mut producer = dynamic_spec("camera");
    producer.outputs.push(OutputSpec::new(data("image")));
    daemon.apply_add_node(producer, true).unwrap();
    let mut camera = attach_dynamic(&mut daemon, "camera").await;

    // No consumer exists yet: the message is published into a graph with
    // nobody subscribed, which must not error.
    camera
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![1]),
        })
        .await;
    pump(&mut daemon).await;

    // Now the dataflow is live: a brand-new node, never in any manifest,
    // is added and wired to the existing producer.
    let mut consumer = dynamic_spec("detect");
    consumer.inputs.push(InputSpec::new(
        data("frames"),
        PortRef::from_parts("camera", "image").unwrap(),
    ));
    daemon.apply_add_node(consumer, true).unwrap();
    let mut detect = attach_dynamic(&mut daemon, "detect").await;

    camera
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![2]),
        })
        .await;
    pump(&mut daemon).await;

    let event = detect
        .recv_matching(|event| matches!(event, NodeEvent::Input { .. }))
        .await
        .expect("the newly added consumer receives the message published after it joined");
    match event {
        NodeEvent::Input { id, payload, .. } => {
            assert_eq!(id, data("frames"));
            assert_eq!(payload, vec![2], "not the message sent before it existed");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn live_remove_closes_the_downstream_consumer_while_an_unrelated_node_keeps_running() {
    let mut daemon = daemon("remove");
    let mut camera = dynamic_spec("camera");
    camera.outputs.push(OutputSpec::new(data("image")));
    daemon.apply_add_node(camera, true).unwrap();
    let mut camera_conn = attach_dynamic(&mut daemon, "camera").await;

    let mut detect = dynamic_spec("detect");
    detect.inputs.push(InputSpec::new(
        data("frames"),
        PortRef::from_parts("camera", "image").unwrap(),
    ));
    daemon.apply_add_node(detect, true).unwrap();
    let mut detect_conn = attach_dynamic(&mut daemon, "detect").await;

    let planner = dynamic_spec("planner");
    daemon.apply_add_node(planner, true).unwrap();
    let mut planner_conn = attach_dynamic(&mut daemon, "planner").await;

    // Remove the producer the consumer depends on.
    daemon
        .apply_remove_node(dataflow(), &node("camera"))
        .unwrap();
    // The cooperative `Stop` reaches the fake "camera" process; a real one
    // would exit and close its socket, which is what actually triggers
    // `close_outputs_of` for a spawned node — for this `path: dynamic`
    // node, closing the connection here is that same signal.
    let stop = camera_conn
        .recv_matching(|event| matches!(event, NodeEvent::Stop { .. }))
        .await;
    assert!(stop.is_some(), "camera is asked to stop");
    drop(camera_conn);
    pump(&mut daemon).await;

    let closed = detect_conn
        .recv_matching(|event| matches!(event, NodeEvent::InputClosed { .. }))
        .await
        .expect("detect is told its input closed");
    match closed {
        NodeEvent::InputClosed { id, .. } => assert_eq!(id, data("frames")),
        other => panic!("unexpected {other:?}"),
    }

    // The unrelated node was never touched: it is still registered and
    // can still exchange events with the daemon.
    planner_conn
        .send(NodeRequest::NextEvent {
            timeout: None,
            max_batch: 1,
        })
        .await;
    pump(&mut daemon).await;
    assert!(
        daemon
            .dataflow(dataflow())
            .unwrap()
            .node(&node("planner"))
            .unwrap()
            .is_live(),
        "an unrelated node keeps running"
    );
}

#[tokio::test]
async fn a_superseded_incarnation_s_teardown_does_not_end_the_consumer() {
    // The continuity guarantee, on the path the tests above never take.
    //
    // `replace_under_load_preserves_message_continuity_across_the_cutover`
    // retires the outgoing incarnation by dropping its socket, which reaches
    // `handle_session_closed` — a path that has always compared the closing
    // session's generation against the node's. A *real* node retires
    // differently: `astrs_node_api::Node`'s `Drop` (and `RawOutput`'s) sends
    // `CloseOutputs` first, and that frame used to close the ports the
    // replacement had already reopened, ending every consumer at the cutover.
    //
    // Observed end to end with `astrs node replace` on a live graph before
    // the guard: the consumer finished at the cutover instead of carrying on
    // through it, and `astrs replay <file> <dataflow>` (built on the same
    // machinery) lost every replayed message as a result.
    let mut daemon = daemon("superseded-close");
    let mut producer = dynamic_spec("camera");
    producer.outputs.push(OutputSpec::new(data("image")));
    daemon.apply_add_node(producer, true).unwrap();
    let mut camera_v1 = attach_dynamic(&mut daemon, "camera").await;

    let mut consumer = dynamic_spec("detect");
    consumer.inputs.push(InputSpec::new(
        data("frames"),
        PortRef::from_parts("camera", "image").unwrap(),
    ));
    daemon.apply_add_node(consumer, true).unwrap();
    let mut detect = attach_dynamic(&mut daemon, "detect").await;

    let mut replacement = dynamic_spec("camera");
    replacement.outputs.push(OutputSpec::new(data("image")));
    daemon.apply_replace_node(replacement).unwrap();
    let mut camera_v2 = attach_dynamic(&mut daemon, "camera").await;

    // Now the outgoing incarnation tears itself down the way a real node
    // does, on a session that is still bound to the superseded generation.
    camera_v1
        .send(NodeRequest::CloseOutputs { outputs: vec![] })
        .await;
    pump(&mut daemon).await;
    drop(camera_v1);
    pump(&mut daemon).await;

    // The replacement's port survived it.
    camera_v2
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![9]),
        })
        .await;
    pump(&mut daemon).await;

    let mut seen: Vec<NodeEvent> = Vec::new();
    for _ in 0..4 {
        detect
            .send(NodeRequest::NextEvent {
                timeout: None,
                max_batch: 8,
            })
            .await;
        pump(&mut daemon).await;
        while let Some(event) = detect
            .recv_matching(|event| {
                matches!(
                    event,
                    NodeEvent::Input { .. } | NodeEvent::InputClosed { .. }
                )
            })
            .await
        {
            let last = matches!(event, NodeEvent::InputClosed { .. });
            seen.push(event);
            if last {
                break;
            }
        }
    }

    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, NodeEvent::InputClosed { .. })),
        "the superseded incarnation must not close the replacement's port: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|event| matches!(event, NodeEvent::Input { payload, .. } if payload == &vec![9])),
        "and the replacement's message must still arrive: {seen:?}"
    );
}

#[tokio::test]
async fn replace_under_load_preserves_message_continuity_across_the_cutover() {
    let mut daemon = daemon("replace");
    let mut producer = dynamic_spec("camera");
    producer.outputs.push(OutputSpec::new(data("image")));
    daemon.apply_add_node(producer, true).unwrap();
    let mut camera_v1 = attach_dynamic(&mut daemon, "camera").await;

    let mut consumer = dynamic_spec("detect");
    consumer.inputs.push(InputSpec::new(
        data("frames"),
        PortRef::from_parts("camera", "image").unwrap(),
    ));
    daemon.apply_add_node(consumer, true).unwrap();
    let mut detect = attach_dynamic(&mut daemon, "detect").await;

    // The old incarnation publishes before the cutover.
    camera_v1
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![1]),
        })
        .await;
    pump(&mut daemon).await;

    // Replace: a new generation is admitted and spawned (here: awaits
    // attach, since it is dynamic) *before* `camera_v1` is asked to leave.
    let mut replacement = dynamic_spec("camera");
    replacement.outputs.push(OutputSpec::new(data("image")));
    daemon.apply_replace_node(replacement).unwrap();
    assert_eq!(
        daemon
            .dataflow(dataflow())
            .unwrap()
            .node(&node("camera"))
            .unwrap()
            .generation(),
        1,
        "the generation advanced"
    );

    // The dual-run window: the *old* incarnation's connection is still
    // open and can still publish — nothing tore its route down.
    camera_v1
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![2]),
        })
        .await;
    pump(&mut daemon).await;

    // The new incarnation attaches and, once registered, also publishes.
    let mut camera_v2 = attach_dynamic(&mut daemon, "camera").await;
    camera_v2
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![3]),
        })
        .await;
    pump(&mut daemon).await;

    // The consumer's queue holds every message from both incarnations, in
    // order, with no gap — the reliable path's continuity guarantee.
    let mut seen = Vec::new();
    for _ in 0..3 {
        detect
            .send(NodeRequest::NextEvent {
                timeout: None,
                max_batch: 8,
            })
            .await;
        pump(&mut daemon).await;
        if let Some(NodeEvent::Input { payload, .. }) = detect
            .recv_matching(|event| matches!(event, NodeEvent::Input { .. }))
            .await
        {
            seen.push(payload);
        }
    }
    assert_eq!(
        seen,
        vec![vec![1], vec![2], vec![3]],
        "every message from both incarnations arrived, none lost, none reordered"
    );
}

#[tokio::test]
async fn edge_add_and_remove_rewire_delivery_live() {
    let mut daemon = daemon("edge");
    let mut camera = dynamic_spec("camera");
    camera.outputs.push(OutputSpec::new(data("image")));
    daemon.apply_add_node(camera, true).unwrap();
    let mut camera_conn = attach_dynamic(&mut daemon, "camera").await;

    let mut camera2 = dynamic_spec("camera2");
    camera2.outputs.push(OutputSpec::new(data("image")));
    daemon.apply_add_node(camera2, true).unwrap();
    let mut camera2_conn = attach_dynamic(&mut daemon, "camera2").await;

    // `detect` starts with no wiring at all: `apply_add_edge` both adds
    // and (for an already-wired input) rewires, and this exercises the
    // pure-add half.
    let detect = dynamic_spec("detect");
    daemon.apply_add_node(detect, true).unwrap();
    let mut detect_conn = attach_dynamic(&mut daemon, "detect").await;

    daemon
        .apply_add_edge(
            dataflow(),
            node("detect"),
            InputSpec::new(
                data("frames"),
                PortRef::from_parts("camera", "image").unwrap(),
            ),
        )
        .unwrap();

    camera_conn
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![10]),
        })
        .await;
    pump(&mut daemon).await;
    let first = detect_conn
        .recv_matching(|event| matches!(event, NodeEvent::Input { .. }))
        .await
        .expect("connected to camera");
    assert!(matches!(first, NodeEvent::Input { payload, .. } if payload == vec![10]));

    // Live rewire: the same input now reads from `camera2` instead.
    daemon
        .apply_add_edge(
            dataflow(),
            node("detect"),
            InputSpec::new(
                data("frames"),
                PortRef::from_parts("camera2", "image").unwrap(),
            ),
        )
        .unwrap();

    camera_conn
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![11]),
        })
        .await;
    camera2_conn
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![12]),
        })
        .await;
    pump(&mut daemon).await;
    detect_conn
        .send(NodeRequest::NextEvent {
            timeout: None,
            max_batch: 8,
        })
        .await;
    pump(&mut daemon).await;
    let second = detect_conn
        .recv_matching(|event| matches!(event, NodeEvent::Input { .. }))
        .await
        .expect("connected to camera2 now");
    assert!(
        matches!(second, NodeEvent::Input { payload, .. } if payload == vec![12]),
        "the old producer's message must not arrive: the edge moved, it did not duplicate"
    );

    // Disconnect entirely.
    daemon
        .apply_remove_edge(dataflow(), &node("detect"), &data("frames"))
        .unwrap();
    let closed = detect_conn
        .recv_matching(|event| matches!(event, NodeEvent::InputClosed { .. }))
        .await
        .expect("removing the edge tells the consumer");
    match closed {
        NodeEvent::InputClosed { id, reason, .. } => {
            assert_eq!(id, data("frames"));
            assert_eq!(reason.kind_name(), "disconnected");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn replace_under_load_with_drop_oldest_bounds_ordinary_loss_but_spares_correlated_messages() {
    // The other half of the task brief's continuity clause:
    // [`replace_under_load_preserves_message_continuity_across_the_cutover`]
    // above proves the *reliable* path (the §24.2 default queue) loses
    // nothing across a cutover. `drop_oldest`'s own contract
    // (`astrs_scheduler::queue`'s module docs) is different by design: at
    // capacity it evicts the oldest *non-immune* message to make room, but
    // never evicts — and never refuses — a message correlated via
    // `request_id`/`goal_id`/`goal_status` (§11.2). This proves that
    // contract holds through the real mailbox path during exactly the
    // scenario §11.2 exists for: a producer publishing faster than a
    // `queue_size: 1` consumer drains, spanning a live `ReplaceNode`
    // cutover.
    let mut daemon = daemon("drop-oldest");
    let mut camera_v1 = dynamic_spec("camera");
    camera_v1.outputs.push(OutputSpec::new(data("image")));
    daemon.apply_add_node(camera_v1, true).unwrap();
    let mut camera_v1_conn = attach_dynamic(&mut daemon, "camera").await;

    let mut consumer = dynamic_spec("detect");
    consumer.inputs.push(
        InputSpec::new(
            data("frames"),
            PortRef::from_parts("camera", "image").unwrap(),
        )
        .with_queue(1, astrs_wire::QueuePolicy::DropOldest),
    );
    // `apply_add_node` wires the declared input as a route immediately,
    // whether or not `start` spawns anything (`add_node_wires_its_inputs_as_routes_without_starting_it`,
    // this crate's own `dataflow::topology` unit test) — but `detect`
    // itself does *not* attach yet. That is deliberate: a consumer with a
    // live session gets each publish delivered opportunistically
    // (`Daemon::push_to_waiting_consumers` -> `Daemon::deliver_now`,
    // draining the mailbox to the wire the instant it is pushed), which
    // would keep this queue's live depth at 0 or 1 throughout and never
    // actually reach the size-1 ceiling this test means to exercise. With
    // no session bound yet, that opportunistic delivery has nothing to
    // deliver *to* and is skipped entirely, so the three publishes below
    // land purely in `InputQueue::push`'s own enqueue-with-eviction logic,
    // undisturbed, and the only drain is the one `detect`'s own
    // registration triggers once it finally attaches.
    daemon.apply_add_node(consumer, true).unwrap();

    // Three publishes, all queued before `detect` ever attaches: an
    // ordinary message, a second ordinary message that (at `queue_size:
    // 1`) must evict the first, and a *correlated* one (carrying a
    // `request_id`) that must survive regardless of the queue already
    // being at capacity — worked out against `InputQueue::push`'s own
    // documented algorithm: [1] enqueued (queue empty) -> [2] arrives at
    // the size-1 ceiling, [1] is the non-immune victim, evicted -> [3]
    // arrives immune, accepted unconditionally on top, past the nominal
    // ceiling. Queue after these three, oldest first: `[2, 3]` — not yet
    // the final state; the cutover's own publish below evicts once more.
    camera_v1_conn
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![1]),
        })
        .await;
    camera_v1_conn
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![2]),
        })
        .await;
    let mut correlated = Metadata::default();
    correlated.set_request_id("req-1");
    camera_v1_conn
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: correlated,
            payload: OutputPayload::inline(vec![3]),
        })
        .await;

    // The replace itself: a fresh generation admitted and spawned while
    // `camera_v1` is still connected (the dual-run window), exactly as
    // `replace_under_load_preserves_message_continuity_across_the_cutover`
    // exercises it — this test's own point is that the *drop_oldest*
    // consumer downstream of it is unaffected by which incarnation
    // produced a message, only by the order it queued in.
    pump(&mut daemon).await;
    let mut replacement = dynamic_spec("camera");
    replacement.outputs.push(OutputSpec::new(data("image")));
    daemon.apply_replace_node(replacement).unwrap();
    let mut camera_v2 = attach_dynamic(&mut daemon, "camera").await;
    camera_v2
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![4]),
        })
        .await;
    pump(&mut daemon).await;

    // [4] (non-immune) arrives at a queue already over its nominal
    // ceiling ([2, 3]: two items, one of them immune) — `InputQueue::push`
    // still finds a non-immune victim to evict ([2]; `immune_count(1) <
    // items.len(2)`), so this cutover's own traffic evicts a *second*
    // ordinary message on top of [1]'s. Expected surviving queue, oldest
    // first: `[3, 4]` — only ever the correlated message and whatever
    // ordinary message queued most recently.
    //
    // Only now does `detect` attach at all: `apply_register`'s
    // `deliver_now` is the *only* drain this queue sees anywhere in this
    // test, so what it hands over on registration is exactly — and only
    // — what survived every eviction above undisturbed.
    let mut detect = attach_dynamic(&mut daemon, "detect").await;

    let mut seen = Vec::new();
    for _ in 0..4 {
        detect
            .send(NodeRequest::NextEvent {
                timeout: None,
                max_batch: 8,
            })
            .await;
        pump(&mut daemon).await;
        match detect
            .recv_matching(|event| matches!(event, NodeEvent::Input { .. }))
            .await
        {
            Some(NodeEvent::Input { payload, .. }) => seen.push(payload),
            _ => break,
        }
    }

    assert!(
        !seen.contains(&vec![1]) && !seen.contains(&vec![2]),
        "both ordinary messages queued at the size-1 ceiling may be dropped \
         (bounded loss, not unbounded — at most one ordinary message ever \
         survives alongside the correlated one): {seen:?}"
    );
    assert!(
        seen.contains(&vec![3]),
        "a correlated message is evict-immune even at the drop_oldest ceiling, \
         across the replace's own cutover: {seen:?}"
    );
    assert_eq!(
        seen,
        vec![vec![3], vec![4]],
        "[1] evicted by [2], which is itself evicted by [4] (published after \
         the cutover) — only the correlated [3] and the most recent ordinary \
         message ever survive the size-1 drop_oldest ceiling: {seen:?}"
    );
}
