//! The daemon↔node conversation, driven end-to-end against a scripted fake
//! node over [`tokio::io::duplex`].
//!
//! No sockets, no ports, no spawned processes: the fake node is a few
//! functions writing [`astrs_wire::NodeRequest`] frames into one end of an
//! in-memory duplex and reading [`astrs_wire::NodeEvent`] frames back. What is
//! exercised is exactly the part that a process-level test cannot isolate —
//! the *protocol*: who may speak before registering, what a publish does to a
//! consumer's queue, whether a crash reclaims an extension entry, and whether
//! a dropped connection is reported exactly once.
//!
//! ```text
//!   fake node ──frames──► duplex ──► SessionActor ──► DaemonHandle ──► Daemon
//!            ◄──frames───────────────────────────────────────────────┘
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use astrs_daemon::server::NodeListeners;
use astrs_daemon::{Daemon, DaemonConfig, ListenConfig, RuntimePaths, plan_dataflow};
use astrs_manifest::Manifest;
use astrs_wire::io::{AsyncFrameReader, AsyncFrameWriter};
use astrs_wire::{
    DataId, DataflowId, ExtensionKey, ExtensionNamespace, FrameLimits, Metadata, NodeEvent,
    NodeHandshake, NodeId, NodeRequest, OutputPayload, SessionId,
};
use tokio::io::DuplexStream;

/// The dataflow every test in this file uses.
fn dataflow() -> DataflowId {
    DataflowId::from_u128(42)
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}

fn data(name: &str) -> DataId {
    DataId::new(name).unwrap()
}

/// A two-node manifest: `camera/image` → `detect/frames`.
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
        queue_size: 4
";

/// A daemon with a private runtime directory and no listeners: every
/// connection in this file is attached in process.
fn daemon(name: &str) -> Daemon {
    daemon_running(name, PIPELINE)
}

/// [`daemon`], admitting an arbitrary manifest.
fn daemon_running(name: &str, manifest: &str) -> Daemon {
    let root = std::env::temp_dir().join(format!("astrs-session-{}-{name}", std::process::id()));
    let _ = std::fs::create_dir_all(&root);
    let config = DaemonConfig::new(RuntimePaths::under(root)).with_listen(ListenConfig::none());
    let mut daemon = Daemon::new(config).expect("a daemon");
    let manifest = Manifest::from_yaml_str(manifest).expect("a valid manifest");
    let plan = plan_dataflow(dataflow(), &manifest, &std::collections::BTreeMap::new())
        .expect("a valid plan");
    daemon.admit(&plan).expect("admitted");
    daemon
}

/// One fake node's end of a connection.
struct FakeNode {
    /// The frame writer.
    writer: AsyncFrameWriter<tokio::io::WriteHalf<DuplexStream>>,
    /// The frame reader.
    reader: AsyncFrameReader<tokio::io::ReadHalf<DuplexStream>>,
    /// The session the daemon assigned.
    session: SessionId,
}

impl FakeNode {
    /// Attaches a fake node to `daemon`.
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

    /// Sends one request.
    async fn send(&mut self, request: NodeRequest) {
        self.writer.send(&request).await.expect("a writable frame");
    }

    /// Reads one event, or `None` if nothing arrives within the budget.
    async fn recv(&mut self) -> Option<NodeEvent> {
        tokio::time::timeout(
            Duration::from_secs(2),
            self.reader.read_message::<NodeEvent>(),
        )
        .await
        .ok()?
        .ok()?
    }

    /// Sends a registration as `name`.
    ///
    /// Does *not* wait for the acknowledgement: nothing the daemon sends can
    /// arrive until the caller pumps its loop, so waiting here would deadlock.
    /// The pattern every test below uses is send → [`pump`] → [`FakeNode::recv`].
    async fn register(&mut self, name: &str) {
        self.send(NodeRequest::Register(NodeHandshake::dynamic(
            dataflow(),
            node(name),
        )))
        .await;
    }
}

/// Runs the daemon's loop for long enough to drain everything pending.
///
/// The loop is fully event-driven, so "long enough" is a handful of idle ticks
/// rather than a fixed sleep: each iteration processes every queued event
/// before it can idle again.
async fn pump(daemon: &mut Daemon) {
    let _ = tokio::time::timeout(Duration::from_millis(400), daemon.run()).await;
}

#[tokio::test]
async fn a_node_registers_and_is_acknowledged() {
    let mut daemon = daemon("register");
    let mut camera = FakeNode::attach(&mut daemon);

    camera
        .send(NodeRequest::Register(NodeHandshake::dynamic(
            dataflow(),
            node("camera"),
        )))
        .await;
    pump(&mut daemon).await;

    match camera.recv().await {
        Some(NodeEvent::Registered { spec, session }) => {
            assert_eq!(spec.node, node("camera"));
            assert_eq!(session, camera.session);
            assert_eq!(spec.outputs.len(), 1);
        }
        other => panic!("expected a registration, got {other:?}"),
    }

    let state = daemon.state().dataflow(dataflow()).expect("admitted");
    assert!(
        state
            .node(&node("camera"))
            .expect("present")
            .is_registered()
    );
}

#[tokio::test]
async fn an_unknown_node_is_refused() {
    let mut daemon = daemon("unknown");
    let mut ghost = FakeNode::attach(&mut daemon);

    ghost
        .send(NodeRequest::Register(NodeHandshake::dynamic(
            dataflow(),
            node("ghost"),
        )))
        .await;
    pump(&mut daemon).await;

    assert!(
        ghost.recv().await.is_none(),
        "a node the manifest does not declare gets no acknowledgement"
    );
}

#[tokio::test]
async fn a_published_message_reaches_a_local_consumer() {
    let mut daemon = daemon("publish");
    let mut camera = FakeNode::attach(&mut daemon);
    let mut detect = FakeNode::attach(&mut daemon);

    camera.register("camera").await;
    detect.register("detect").await;
    pump(&mut daemon).await;
    let _ = camera.recv().await;
    let _ = detect.recv().await;

    detect
        .send(NodeRequest::Subscribe {
            inputs: vec![data("frames")],
        })
        .await;
    camera
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![1, 2, 3]),
        })
        .await;
    detect
        .send(NodeRequest::NextEvent {
            timeout: None,
            max_batch: 8,
        })
        .await;
    pump(&mut daemon).await;

    let mut received = None;
    while let Some(event) = detect.recv().await {
        if let NodeEvent::Input { id, payload, .. } = event {
            received = Some((id, payload));
            break;
        }
    }
    let (id, payload) = received.expect("the message was delivered");
    assert_eq!(id, data("frames"));
    assert_eq!(payload, [1, 2, 3]);
}

#[tokio::test]
async fn publishing_on_an_undeclared_output_is_refused() {
    let mut daemon = daemon("undeclared");
    let mut camera = FakeNode::attach(&mut daemon);
    let mut detect = FakeNode::attach(&mut daemon);

    camera.register("camera").await;
    detect.register("detect").await;
    pump(&mut daemon).await;
    let _ = camera.recv().await;
    let _ = detect.recv().await;

    camera
        .send(NodeRequest::SendMessage {
            output: data("nonexistent"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![9]),
        })
        .await;
    detect
        .send(NodeRequest::NextEvent {
            timeout: None,
            max_batch: 8,
        })
        .await;
    pump(&mut daemon).await;

    while let Some(event) = detect.recv().await {
        assert!(
            !matches!(event, NodeEvent::Input { .. }),
            "nothing should have been routed"
        );
    }
}

#[tokio::test]
async fn closing_an_output_closes_the_consumer_input() {
    let mut daemon = daemon("close");
    let mut camera = FakeNode::attach(&mut daemon);
    let mut detect = FakeNode::attach(&mut daemon);

    camera.register("camera").await;
    detect.register("detect").await;
    pump(&mut daemon).await;
    let _ = camera.recv().await;
    let _ = detect.recv().await;

    camera
        .send(NodeRequest::OutputDone {
            output: data("image"),
        })
        .await;
    detect
        .send(NodeRequest::NextEvent {
            timeout: None,
            max_batch: 8,
        })
        .await;
    pump(&mut daemon).await;

    let mut saw_close = false;
    while let Some(event) = detect.recv().await {
        if let NodeEvent::InputClosed { id, .. } = event {
            assert_eq!(id, data("frames"));
            saw_close = true;
        }
    }
    assert!(saw_close, "the consumer was told its input closed");
}

#[tokio::test]
async fn an_extension_entry_round_trips() {
    let mut daemon = daemon("ext");
    let mut camera = FakeNode::attach(&mut daemon);

    camera.register("camera").await;
    pump(&mut daemon).await;
    let _ = camera.recv().await;

    let key = ExtensionKey::user("frame_pool").unwrap();
    camera
        .send(NodeRequest::ExtStore {
            key: key.clone(),
            value: vec![7, 7, 7],
            ttl: None,
        })
        .await;
    camera.send(NodeRequest::ExtLoad { key: key.clone() }).await;
    pump(&mut daemon).await;

    let mut value = None;
    while let Some(event) = camera.recv().await {
        if let NodeEvent::ExtValue { value: got, .. } = event {
            value = got;
            break;
        }
    }
    assert_eq!(value, Some(vec![7, 7, 7]));
}

#[tokio::test]
async fn a_reserved_namespace_store_is_refused() {
    let mut daemon = daemon("reserved");
    let mut camera = FakeNode::attach(&mut daemon);

    camera.register("camera").await;
    pump(&mut daemon).await;
    let _ = camera.recv().await;

    let key = ExtensionKey::new(ExtensionNamespace::GpuHandle, "pool").unwrap();
    camera
        .send(NodeRequest::ExtStore {
            key: key.clone(),
            value: vec![1],
            ttl: None,
        })
        .await;
    pump(&mut daemon).await;

    let state = daemon.state().dataflow(dataflow()).expect("admitted");
    assert!(
        state.extensions().peek(&key).is_none(),
        "a node may not write a reserved namespace"
    );
}

#[tokio::test]
async fn a_dropped_session_reclaims_its_extension_entries() {
    let mut daemon = daemon("reclaim");
    let mut camera = FakeNode::attach(&mut daemon);
    let mut detect = FakeNode::attach(&mut daemon);

    camera.register("camera").await;
    detect.register("detect").await;
    pump(&mut daemon).await;
    let _ = camera.recv().await;
    let _ = detect.recv().await;

    let key = ExtensionKey::user("pool").unwrap();
    camera
        .send(NodeRequest::ExtStore {
            key: key.clone(),
            value: vec![1],
            ttl: None,
        })
        .await;
    detect.send(NodeRequest::ExtLoad { key: key.clone() }).await;
    pump(&mut daemon).await;

    assert!(
        daemon
            .state()
            .dataflow(dataflow())
            .expect("admitted")
            .extensions()
            .peek(&key)
            .is_some(),
        "the entry was stored"
    );

    // The producer's socket goes away, as a crash would leave it.
    drop(camera);
    pump(&mut daemon).await;

    assert!(
        daemon
            .state()
            .dataflow(dataflow())
            .expect("admitted")
            .extensions()
            .peek(&key)
            .is_none(),
        "the owner's entries were reclaimed"
    );

    let mut saw_drop = false;
    while let Some(event) = detect.recv().await {
        if let NodeEvent::ExtDropped { key: got, reason } = event {
            assert_eq!(got, key);
            assert!(reason.contains("camera"), "{reason}");
            saw_drop = true;
        }
    }
    assert!(saw_drop, "the reader that held a handle was told");
}

#[tokio::test]
async fn a_request_before_registration_is_refused() {
    let mut daemon = daemon("early");
    let mut early = FakeNode::attach(&mut daemon);

    early
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![1]),
        })
        .await;
    pump(&mut daemon).await;

    assert!(
        early.recv().await.is_none(),
        "an unregistered session gets nothing back"
    );
    assert_eq!(
        daemon.state().session_count(),
        0,
        "and is not bound to anything"
    );
}

#[tokio::test]
async fn a_second_registration_on_one_connection_changes_nothing() {
    let mut daemon = daemon("twice");
    let mut camera = FakeNode::attach(&mut daemon);

    camera.register("camera").await;
    pump(&mut daemon).await;
    let _ = camera.recv().await;

    camera
        .send(NodeRequest::Register(NodeHandshake::dynamic(
            dataflow(),
            node("detect"),
        )))
        .await;
    pump(&mut daemon).await;

    let binding = daemon
        .state()
        .session(camera.session)
        .expect("still bound to the first node");
    assert_eq!(binding.node, node("camera"));
}

#[tokio::test]
async fn dropping_the_event_stream_unbinds_the_session() {
    let mut daemon = daemon("drop-stream");
    let mut camera = FakeNode::attach(&mut daemon);

    camera.register("camera").await;
    pump(&mut daemon).await;
    let _ = camera.recv().await;
    assert_eq!(daemon.state().session_count(), 1);

    camera.send(NodeRequest::EventStreamDropped).await;
    pump(&mut daemon).await;

    assert_eq!(
        daemon.state().session_count(),
        0,
        "the session was unbound exactly once"
    );
}

#[tokio::test]
async fn a_subscribe_with_no_inputs_takes_every_declared_one() {
    let mut daemon = daemon("subscribe-all");
    let mut detect = FakeNode::attach(&mut daemon);

    detect.register("detect").await;
    pump(&mut daemon).await;
    let _ = detect.recv().await;

    detect
        .send(NodeRequest::Subscribe { inputs: Vec::new() })
        .await;
    pump(&mut daemon).await;

    let state = daemon.state().dataflow(dataflow()).expect("admitted");
    let node_state = state.node(&node("detect")).expect("present");
    assert!(node_state.is_subscribed(&data("frames")));
}

/// A node that logs keeps its session (§7.1, §7.3, §9.1).
///
/// The regression this guards was total: the read task decoded *every* frame
/// as a [`NodeRequest`] regardless of its `kind`, so the first `Log` frame a
/// node sent — which is what every `Node::log_info` call produces — looked
/// like a corrupt request, failed the session and dropped the connection. Any
/// node that used the flagship logging API lost its daemon within
/// milliseconds of registering.
#[tokio::test]
async fn a_log_frame_is_fanned_out_and_leaves_the_session_open() {
    use astrs_wire::{DaemonEvent, LogFrame, LogLevel, LogRecord, SubscriptionId};

    /// Collects everything the daemon reports upward.
    #[derive(Debug, Default)]
    struct Recorder {
        records: std::sync::Mutex<Vec<LogRecord>>,
    }

    impl astrs_daemon::health::ReportSink for Recorder {
        fn report(&self, event: DaemonEvent) {
            if let DaemonEvent::Log { records, .. } = event
                && let Ok(mut seen) = self.records.lock()
            {
                seen.extend(records);
            }
        }
    }

    let mut daemon = daemon("node-log");
    let recorder = std::sync::Arc::new(Recorder::default());
    daemon.set_sink(recorder.clone());

    let mut camera = FakeNode::attach(&mut daemon);
    camera.register("camera").await;
    pump(&mut daemon).await;
    let _ = camera.recv().await;

    let record = LogRecord::new(
        astrs_time::HlcTimestamp::new(7, 0),
        LogLevel::Warn,
        "lens is dirty",
    )
    .with_target("camera")
    .with_field("frame", "12")
    .unwrap();
    camera
        .writer
        .send(&LogFrame::new(SubscriptionId::FIRST, record))
        .await
        .expect("a writable log frame");
    pump(&mut daemon).await;

    let seen = recorder.records.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        1,
        "the record reached the report sink: {seen:?}"
    );
    assert_eq!(seen[0].message, "lens is dirty");
    assert_eq!(seen[0].node.as_ref(), Some(&node("camera")));
    assert_eq!(seen[0].fields.get("frame").map(String::as_str), Some("12"));

    // And the session is still usable: the ordinary request path answers.
    camera
        .send(NodeRequest::Subscribe {
            inputs: vec![data("image")],
        })
        .await;
    camera
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(vec![9]),
        })
        .await;
    camera
        .send(NodeRequest::NextEvent {
            timeout: None,
            max_batch: 4,
        })
        .await;
    pump(&mut daemon).await;

    // The socket is still there to be written to, which is the property the
    // regression destroyed: `send` on a dropped session panics on the
    // `expect` inside `FakeNode::send`, long before this assertion.
    camera
        .send(NodeRequest::CloseOutputs {
            outputs: Vec::new(),
        })
        .await;
    pump(&mut daemon).await;
}

/// A timer tick reaches a node that never asked for one (§8.4, §9.1).
///
/// The regression this guards: `fire_timers` pushed the tick into the node's
/// mailbox and stopped there, while every *published* message additionally
/// pushed it to the consumer's session. A tick therefore waited for an
/// explicit `NextEvent` — which §9.1's canonical loop
/// (`while let Some(event) = events.recv()`) never sends — so a node whose
/// only input was `astrs/timer/*` waited forever and the graph made no
/// progress at all.
#[tokio::test]
async fn a_timer_tick_is_pushed_without_a_next_event_request() {
    const TIMED: &str = "\
nodes:
  - id: ticker
    path: dynamic
    inputs:
      tick: astrs/timer/millis/10
";

    let mut daemon = daemon_running("timer-push", TIMED);
    let mut ticker = FakeNode::attach(&mut daemon);
    ticker.register("ticker").await;
    ticker
        .send(NodeRequest::Subscribe {
            inputs: vec![data("tick")],
        })
        .await;
    pump(&mut daemon).await;

    let mut ticked = false;
    while let Some(event) = ticker.recv().await {
        if let NodeEvent::Input { id, .. } = event
            && id == data("tick")
        {
            ticked = true;
            break;
        }
    }
    assert!(ticked, "the timer tick was never delivered");
}
