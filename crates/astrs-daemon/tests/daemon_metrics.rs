//! Every daemon-level series is registered, named and readable (§13).
//!
//! A metric name is a contract with an operator's dashboard, so these tests
//! assert *presence and identity* rather than values: on macOS the CPU and RSS
//! readings are a `ps` fallback or a lifetime-peak
//! ([`astrs_telemetry::sampler::SampleFidelity`]), and a test that asserted a
//! number would be asserting the platform rather than the daemon.
//!
//! What is asserted numerically is what the daemon itself counts: routes by
//! plane, queue drops, restarts, tap frames, heartbeats — figures the daemon
//! computes from its own state and can therefore be held to exactly.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use astrs_daemon::health::RecordingSink;
use astrs_daemon::metrics::{DaemonMetrics, names};
use astrs_daemon::server::NodeListeners;
use astrs_daemon::{Daemon, DaemonConfig, ListenConfig, RuntimePaths, plan_dataflow};
use astrs_manifest::Manifest;
use astrs_time::HlcTimestamp;
use astrs_wire::io::{AsyncFrameReader, AsyncFrameWriter};
use astrs_wire::{
    DataId, DataflowId, FrameLimits, Metadata, MetricBatch, NodeEvent, NodeHandshake, NodeId,
    NodeRequest, OutputPayload, SessionId, SubscriptionId,
};
use tokio::io::DuplexStream;

fn dataflow() -> DataflowId {
    DataflowId::from_u128(0x0E71_C500)
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}

fn data(name: &str) -> DataId {
    DataId::new(name).unwrap()
}

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
        queue_size: 1
        queue_policy: drop_oldest
";

fn daemon(name: &str) -> Daemon {
    let root = std::env::temp_dir().join(format!("astrs-metrics-{}-{name}", std::process::id()));
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
            Duration::from_millis(300),
            self.reader.read_message::<NodeEvent>(),
        )
        .await
        .ok()?
        .ok()?
    }

    async fn register(&mut self, name: &str) {
        self.send(NodeRequest::Register(NodeHandshake::dynamic(
            dataflow(),
            node(name),
        )))
        .await;
    }
}

async fn pump(daemon: &mut Daemon) {
    daemon.pump(Duration::from_millis(80)).await;
}

/// The value of an unlabelled series, if it is registered.
fn value(batch: &MetricBatch, name: &str) -> Option<f64> {
    batch
        .points
        .iter()
        .find(|point| point.name == name && point.labels.is_empty())
        .map(|point| point.value.as_f64())
}

/// The value of a labelled series, if it is registered.
fn labelled(batch: &MetricBatch, name: &str, key: &str, expect: &str) -> Option<f64> {
    batch
        .points
        .iter()
        .find(|point| point.name == name && point.label(key) == Some(expect))
        .map(|point| point.value.as_f64())
}

#[test]
fn every_documented_series_is_registered_at_construction() {
    let metrics = DaemonMetrics::new();
    // The labelled family has no series until a label value is used, so it is
    // exercised first and then asserted alongside the rest.
    metrics.set_routes_on_plane("local", 0);
    let batch = metrics.snapshot(HlcTimestamp::new(1, 0));

    for name in names::ALL {
        assert!(
            batch.points.iter().any(|point| point.name == *name),
            "{name} is documented but not registered"
        );
    }
    assert_eq!(batch.scope, "astrs_daemon");
    assert!(batch.within_limits());
}

#[tokio::test]
async fn a_fresh_daemon_publishes_its_shape() {
    let mut daemon = daemon("shape");
    daemon.tick(Instant::now());
    let batch = daemon.metrics().snapshot(HlcTimestamp::new(1, 0));

    assert_eq!(value(&batch, names::DATAFLOWS), Some(1.0));
    assert_eq!(value(&batch, names::NODES_RUNNING), Some(0.0));
    assert_eq!(value(&batch, names::PEERS_CONNECTED), Some(0.0));
    assert_eq!(value(&batch, names::SEGMENTS_OPEN), Some(0.0));
    assert_eq!(
        labelled(&batch, names::ROUTES, "plane", "local"),
        Some(1.0),
        "one edge, on the reliable path"
    );
    assert_eq!(labelled(&batch, names::ROUTES, "plane", "shm"), Some(0.0));
    assert_eq!(
        labelled(&batch, names::ROUTES, "plane", "remote"),
        Some(0.0)
    );
}

#[tokio::test]
async fn a_running_node_moves_the_gauge() {
    let mut daemon = daemon("running");
    let mut camera = FakeNode::attach(&mut daemon);
    camera.register("camera").await;
    pump(&mut daemon).await;

    let batch = daemon.metrics().snapshot(HlcTimestamp::new(2, 0));
    assert_eq!(value(&batch, names::NODES_RUNNING), Some(1.0));
}

#[tokio::test]
async fn a_full_queue_is_counted_as_a_drop() {
    let mut daemon = daemon("drops");
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
    pump(&mut daemon).await;

    // Then the consumer stops reading its socket altogether — the one
    // condition under which the daemon's own per-input queue is what stands
    // between a fast producer and unbounded memory. On the ordinary path the
    // daemon pushes each message straight on to a connected consumer as it
    // arrives (blueprint §11.2 puts the per-input bounded queues *in the node
    // API*, which is where a healthy consumer's `queue_size`/`queue_policy`
    // therefore applies), so the daemon-side queue only ever fills for a
    // consumer that is gone or wedged — exactly this one.
    drop(detect);
    pump(&mut daemon).await;

    // The input's queue holds one message under `drop_oldest`; a burst with
    // nobody draining it must be visible as a drop rather than vanish (§11.2).
    for index in 0..12u8 {
        camera
            .send(NodeRequest::SendMessage {
                output: data("image"),
                metadata: Metadata::default(),
                payload: OutputPayload::inline(vec![index; 8]),
            })
            .await;
    }
    pump(&mut daemon).await;

    let dropped = daemon.metrics().ft_stats().queue_drops;
    assert!(dropped > 0, "a burst into a one-slot queue dropped nothing");
    let batch = daemon.metrics().snapshot(HlcTimestamp::new(3, 0));
    assert_eq!(
        value(&batch, names::QUEUE_DROPS_TOTAL),
        Some(dropped as f64)
    );
}

#[tokio::test]
async fn tap_frames_are_counted_and_reported() {
    let mut daemon = daemon("taps");
    let sink = Arc::new(RecordingSink::new());
    daemon.set_sink(sink.clone());
    daemon.taps_mut().enable(dataflow());
    assert!(daemon.start_tap(SubscriptionId::new(1), dataflow(), None, None));

    let mut camera = FakeNode::attach(&mut daemon);
    camera.register("camera").await;
    pump(&mut daemon).await;
    camera
        .send(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: OutputPayload::inline(b"tapped".to_vec()),
        })
        .await;
    pump(&mut daemon).await;

    let batch = daemon.metrics().snapshot(HlcTimestamp::new(4, 0));
    assert_eq!(value(&batch, names::TAP_MESSAGES_TOTAL), Some(1.0));
    assert_eq!(sink.count_of("TopicTapData"), 1);
}

#[tokio::test]
async fn heartbeats_are_counted_and_carry_the_fault_summary() {
    let mut daemon = daemon("heartbeats");
    let sink = Arc::new(RecordingSink::new());
    daemon.set_sink(sink.clone());

    let start = Instant::now();
    daemon.tick(start + Duration::from_secs(6));
    daemon.tick(start + Duration::from_secs(12));

    let batch = daemon.metrics().snapshot(HlcTimestamp::new(5, 0));
    assert_eq!(value(&batch, names::HEARTBEATS_TOTAL), Some(2.0));
    assert_eq!(sink.count_of("Heartbeat"), 2);

    match sink
        .snapshot()
        .into_iter()
        .find(|event| matches!(event, astrs_wire::DaemonEvent::Heartbeat { .. }))
        .expect("a heartbeat")
    {
        astrs_wire::DaemonEvent::Heartbeat { seq, stats, .. } => {
            assert_eq!(seq, 1, "the sequence starts at one and counts up");
            assert_eq!(stats.dataflow_count, 1);
            assert_eq!(stats.shm_fallback_total, 0);
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn node_metric_batches_are_counted() {
    let mut daemon = daemon("samples");
    let sink = Arc::new(RecordingSink::new());
    daemon.set_sink(sink.clone());
    let mut camera = FakeNode::attach(&mut daemon);
    camera.register("camera").await;
    pump(&mut daemon).await;

    daemon.tick(Instant::now() + Duration::from_secs(3));

    let batch = daemon.metrics().snapshot(HlcTimestamp::new(6, 0));
    assert_eq!(value(&batch, names::METRIC_SAMPLES_TOTAL), Some(1.0));

    let reported = sink
        .snapshot()
        .into_iter()
        .find_map(|event| match event {
            astrs_wire::DaemonEvent::NodeMetrics { samples, .. } => Some(samples),
            _ => None,
        })
        .expect("a metrics batch");
    assert!(
        reported.iter().any(|sample| sample.node == node("camera")),
        "the registered node was sampled"
    );
    // Values are platform-dependent (macOS has no `/proc`), so only their
    // presence is asserted.
    assert!(reported.iter().all(|sample| sample.cpu_percent >= 0.0));
}

#[tokio::test]
async fn a_daemon_with_no_coordinator_still_counts_what_it_would_have_sent() {
    let mut daemon = daemon("nullsink");
    assert!(!daemon.sink().is_open(), "the default sink has no far end");

    let start = Instant::now();
    daemon.tick(start + Duration::from_secs(6));

    assert!(
        daemon.sink().dropped() >= 1,
        "everything reported upward was discarded"
    );
    let batch = daemon.metrics().snapshot(HlcTimestamp::new(7, 0));
    assert_eq!(
        value(&batch, names::HEARTBEATS_TOTAL),
        Some(1.0),
        "the daemon counts what it produced, not what was delivered"
    );
}

#[tokio::test]
async fn the_registry_can_be_shared_with_an_exporter() {
    let daemon = daemon("registry");
    let registry = daemon.metrics().registry();
    let batch = registry.snapshot(HlcTimestamp::new(8, 0), "external");

    assert_eq!(batch.scope, "external");
    assert!(
        batch
            .points
            .iter()
            .any(|point| point.name == names::HEARTBEATS_TOTAL),
        "an exporter sees the daemon's own series"
    );
}

#[tokio::test]
async fn a_dataflow_whose_nodes_have_all_finished_does_not_spin() {
    let mut daemon = daemon("finished");
    for name in ["camera", "detect"] {
        daemon
            .state_mut()
            .dataflow_mut(dataflow())
            .expect("admitted")
            .node_mut(&node(name))
            .expect("declared")
            .mark_exited(astrs_wire::NodeExitCause::Success);
    }

    // The dataflow is still admitted but has nothing to sample. The round must
    // still consume its slot, or the sampling deadline stays permanently in
    // the past and the event loop's `select!` never sleeps.
    let start = Instant::now();
    daemon.tick(start + Duration::from_secs(3));
    let after_first = daemon.metrics().snapshot(HlcTimestamp::new(9, 0));
    let first = value(&after_first, names::METRIC_SAMPLES_TOTAL);

    daemon.tick(start + Duration::from_secs(3));
    let after_second = daemon.metrics().snapshot(HlcTimestamp::new(10, 0));
    assert_eq!(
        value(&after_second, names::METRIC_SAMPLES_TOTAL),
        first,
        "a second tick at the same instant is not a second round"
    );

    // And the loop itself must be able to idle rather than spin.
    let processed = daemon.processed();
    daemon.pump(Duration::from_millis(60)).await;
    assert_eq!(
        daemon.processed(),
        processed,
        "an idle daemon processes no events"
    );
}

#[tokio::test]
async fn a_publish_is_counted_in_messages_and_in_bytes() {
    // §13's bandwidth half, end to end through the daemon: three publishes of
    // known size must appear as `sent_total` (which nothing populated before)
    // on `NodeMetrics`, and as `sent_bytes_total`/`received_bytes_total` on
    // the tail-appended `NodeIoMetrics` beside it.
    let mut daemon = daemon("bandwidth");
    let sink = Arc::new(RecordingSink::new());
    daemon.set_sink(sink.clone());
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
    pump(&mut daemon).await;

    const PAYLOAD: usize = 100;
    const PUBLISHES: usize = 3;
    for index in 0..PUBLISHES {
        camera
            .send(NodeRequest::SendMessage {
                output: data("image"),
                metadata: Metadata::default(),
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "the loop bound is 3; the byte is only fill"
                )]
                payload: OutputPayload::inline(vec![index as u8; PAYLOAD]),
            })
            .await;
    }
    pump(&mut daemon).await;

    daemon.tick(Instant::now() + Duration::from_secs(3));
    let events = sink.snapshot();

    let metrics = events
        .iter()
        .find_map(|event| match event {
            astrs_wire::DaemonEvent::NodeMetrics { samples, .. } => Some(samples),
            _ => None,
        })
        .expect("a metrics batch");
    let camera_metrics = metrics
        .iter()
        .find(|sample| sample.node == node("camera"))
        .expect("the producer was sampled");
    assert_eq!(
        camera_metrics.sent_total.get(&data("image")),
        Some(&(PUBLISHES as u64)),
        "`sent_total` must carry the per-output publish count: {:?}",
        camera_metrics.sent_total
    );

    let io = events
        .iter()
        .find_map(|event| match event {
            astrs_wire::DaemonEvent::NodeIoMetrics { samples, .. } => Some(samples),
            _ => None,
        })
        .expect("a bandwidth batch beside it");
    let camera_io = io
        .iter()
        .find(|sample| sample.node == node("camera"))
        .expect("the producer was sampled");
    assert_eq!(
        camera_io.sent_bytes_total.get(&data("image")),
        Some(&((PUBLISHES * PAYLOAD) as u64)),
        "egress bytes are charged once per publish, not once per consumer"
    );
    assert_eq!(camera_io.total_sent_bytes(), (PUBLISHES * PAYLOAD) as u64);

    let detect_io = io
        .iter()
        .find(|sample| sample.node == node("detect"))
        .expect("the consumer was sampled");
    assert_eq!(
        detect_io.received_bytes_total.get(&data("frames")),
        Some(&((PUBLISHES * PAYLOAD) as u64)),
        "ingress bytes are counted where a queue accepted them"
    );
    assert_eq!(
        detect_io.total_sent_bytes(),
        0,
        "a pure consumer publishes nothing"
    );
}

#[tokio::test]
async fn a_message_a_queue_refused_is_absent_from_the_received_bytes() {
    // The pair `received_bytes_total` forms with §11.2's `dropped_total`: what
    // reached the consumer, and what the policy discarded. Counting a refused
    // message as received would make a saturated link indistinguishable from a
    // healthy one.
    let mut daemon = daemon("refused-bytes");
    let sink = Arc::new(RecordingSink::new());
    daemon.set_sink(sink.clone());
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
    pump(&mut daemon).await;
    // The consumer stops reading, so the daemon-side one-slot `drop_oldest`
    // queue is what stands between the burst and memory (as in
    // `a_full_queue_is_counted_as_a_drop`).
    drop(detect);
    pump(&mut daemon).await;

    const PAYLOAD: usize = 64;
    const PUBLISHES: usize = 20;
    for index in 0..PUBLISHES {
        camera
            .send(NodeRequest::SendMessage {
                output: data("image"),
                metadata: Metadata::default(),
                #[expect(clippy::cast_possible_truncation, reason = "fill byte only")]
                payload: OutputPayload::inline(vec![index as u8; PAYLOAD]),
            })
            .await;
    }
    pump(&mut daemon).await;
    daemon.tick(Instant::now() + Duration::from_secs(3));

    let events = sink.snapshot();
    let io = events
        .iter()
        .find_map(|event| match event {
            astrs_wire::DaemonEvent::NodeIoMetrics { samples, .. } => Some(samples),
            _ => None,
        })
        .expect("a bandwidth batch");
    let camera_io = io
        .iter()
        .find(|sample| sample.node == node("camera"))
        .expect("the producer was sampled");
    assert_eq!(
        camera_io.total_sent_bytes(),
        (PUBLISHES * PAYLOAD) as u64,
        "every publish left the producer, whatever became of it downstream"
    );

    if let Some(detect_io) = io.iter().find(|sample| sample.node == node("detect")) {
        assert!(
            detect_io.total_received_bytes() < (PUBLISHES * PAYLOAD) as u64,
            "a one-slot queue cannot have accepted all {PUBLISHES} messages: {:?}",
            detect_io.received_bytes_total
        );
    }
    assert!(
        daemon.metrics().ft_stats().queue_drops > 0,
        "the drops the missing bytes correspond to"
    );
}
