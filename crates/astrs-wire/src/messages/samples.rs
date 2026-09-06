//! Deterministic protocol samples: one value per variant of every family.
//!
//! This table is the input to the protocol snapshot
//! (`tests/golden/protocol.snap`), which freezes the encoding of every variant
//! and therefore the compatibility contract of the whole project (§7.2,
//! §24.1). It is public because the snapshot is an integration test — and
//! because downstream crates building conformance suites (§20.3) need exactly
//! the same fixtures, and a second, divergent copy of them would be worse than
//! useless.
//!
//! # Determinism
//!
//! Every value here is built from constants: fixed UUIDs, a fixed HLC
//! timestamp, a fixed auth token, and a **fixed** [`AstrsVersion`] rather than
//! [`AstrsVersion::current`] — the snapshot must not change when the crate
//! version is bumped. Nothing calls a clock or a random generator.
//!
//! # Fallibility
//!
//! The functions return [`WireResult`] because identifiers validate on
//! construction, and this module refuses to `unwrap` (COOLJAPAN policy: no
//! `unwrap` outside tests). Every table in this module is in fact infallible;
//! `samples_are_infallible` asserts it.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{ControlRequest, WireMessage, samples};
//!
//! let requests = samples::control_requests()?;
//! assert_eq!(requests.len(), ControlRequest::VARIANT_NAMES.len());
//! for (index, request) in requests.iter().enumerate() {
//!     assert_eq!(usize::from(request.variant_index()), index);
//! }
//! # Ok::<(), astrs_wire::WireError>(())
//! ```

use std::collections::BTreeMap;

use astrs_time::HlcTimestamp;
use uuid::Uuid;

use crate::auth::AuthToken;
use crate::common::duration::DurationMs;
use crate::common::log::{LogLevel, LogRecord};
use crate::common::metrics::{
    DaemonStats, MetricBatch, MetricPoint, NodeIoSample, NodeMetricsSample,
};
use crate::common::node::{InputSpec, NodeSource, NodeSpawnSpec, OutputSpec, QueuePolicy};
use crate::common::route::{
    Plane, RouteAcceptance, RouteCloseReason, RouteDowngradeReason, RouteKey, RouteSpec,
    ShmSegmentSpec,
};
use crate::common::status::{
    DaemonInfo, DataflowResult, DataflowStatus, DataflowSummary, NodeExitCause, NodeInfo,
    NodeRunState, StopCause,
};
use crate::common::stream::{
    DataFrame, LogFrame, SpanStatus, TelemetryFrame, TraceData, TraceSpan,
};
use crate::error::WireResult;
use crate::frame::Compression;
use crate::handshake::features::FeatureFlags;
use crate::handshake::limits::NegotiatedLimits;
use crate::handshake::messages::{Hello, RefusalReason, Refused, Welcome};
use crate::handshake::role::Role;
use crate::ids::{
    BuildId, DaemonId, DataId, DataflowId, MachineName, NodeId, OperatorId, ParamKey, PortRef,
    RouteId, SessionId, SubscriptionId, TypeUrn,
};
use crate::messages::control::types::{
    DataflowSource, ErrorCode, LogQuery, ParamScope, TopicQuery,
};
use crate::messages::control::{ControlReply, ControlRequest};
use crate::messages::coordinator_daemon::types::{
    BuildOutcome, BuildStep, DaemonRegistration, PeerRouteDirective, SpawnOutcome, StateEntry,
    StateEntryKind,
};
use crate::messages::coordinator_daemon::{CoordinatorEvent, DaemonEvent};
use crate::messages::daemon_node::types::{
    ExtensionKey, ExtensionNamespace, NodeHandshake, OutputPayload,
};
use crate::messages::daemon_node::{NodeEvent, NodeRequest};
use crate::messages::peer::PeerEvent;
use crate::metadata::{Metadata, Parameter};
use crate::version::AstrsVersion;

/// The AstRS version every sample reports.
///
/// Deliberately **not** [`AstrsVersion::current`]: a snapshot that changed on
/// every version bump would freeze nothing.
///
/// `0.0.0` is chosen precisely because no release ever carries it. A sample
/// version that happened to equal the current crate version would encode
/// identically today and break the freeze at the first version bump, for a
/// reason nobody would remember; with `0.0.0` the protocol snapshot test can
/// assert that the crate's own version string appears nowhere in the frozen
/// bytes.
#[must_use]
pub fn sample_version() -> AstrsVersion {
    AstrsVersion::from_parts(0, 0, 0)
}

/// The dataflow id every sample uses.
#[must_use]
pub fn sample_dataflow() -> DataflowId {
    DataflowId::from_u128(0x0000_0000_0000_0001)
}

/// The build id every sample uses.
#[must_use]
pub fn sample_build() -> BuildId {
    BuildId::from_u128(0x0000_0000_0000_0002)
}

/// The session id every sample uses.
#[must_use]
pub fn sample_session() -> SessionId {
    SessionId::from_u128(0x0000_0000_0000_0003)
}

/// The daemon id every sample uses.
///
/// # Errors
///
/// [`WireError::Id`](crate::WireError::Id) — never, for this fixed name.
pub fn sample_daemon() -> WireResult<DaemonId> {
    Ok(DaemonId::new(
        Some(MachineName::new("robot-01")?),
        Uuid::from_u128(0x0000_0000_0000_0004),
    ))
}

/// The HLC timestamp every sample uses.
#[must_use]
pub fn sample_timestamp() -> HlcTimestamp {
    HlcTimestamp::new(1_700_000_000_000_000_000, 7)
}

/// The auth token every sample uses.
#[must_use]
pub fn sample_token() -> AuthToken {
    AuthToken::from_bytes([0x11; 32])
}

/// The metadata every sample uses: a fixed timestamp and one correlation key.
///
/// # Errors
///
/// [`WireError::Id`](crate::WireError::Id) — never, for this fixed key.
pub fn sample_metadata() -> WireResult<Metadata> {
    let mut metadata = Metadata::new(sample_timestamp());
    metadata.set_request_id("req-1");
    metadata.set_seq(42);
    Ok(metadata)
}

/// The producer port every sample uses (`camera/image`).
///
/// # Errors
///
/// [`WireError::Id`](crate::WireError::Id) — never, for this fixed name.
pub fn sample_producer() -> WireResult<PortRef> {
    Ok(PortRef::new(NodeId::new("camera")?, DataId::new("image")?))
}

/// The consumer port every sample uses (`detector/frames`).
///
/// # Errors
///
/// [`WireError::Id`](crate::WireError::Id) — never, for this fixed name.
pub fn sample_consumer() -> WireResult<PortRef> {
    Ok(PortRef::new(
        NodeId::new("detector")?,
        DataId::new("frames")?,
    ))
}

/// The route key every sample uses.
///
/// # Errors
///
/// As [`sample_producer`].
pub fn sample_route_key() -> WireResult<RouteKey> {
    Ok(RouteKey::new(
        sample_dataflow(),
        sample_producer()?,
        sample_consumer()?,
    ))
}

/// The route specification every sample uses.
///
/// # Errors
///
/// As [`sample_route_key`].
pub fn sample_route() -> WireResult<RouteSpec> {
    Ok(RouteSpec::new(sample_route_key()?)
        .with_plane(Plane::Quic)
        .with_compression(Compression::Zstd)
        .with_segment("astrs/00000000-0000-0000-0000-000000000001/camera/3/image"))
}

/// The fully expanded spawn specification every sample uses.
///
/// # Errors
///
/// As [`sample_producer`].
pub fn sample_spawn_spec() -> WireResult<NodeSpawnSpec> {
    let spec = NodeSpawnSpec::new(
        sample_dataflow(),
        NodeId::new("camera")?,
        3,
        NodeSource::Executable {
            path: "./target/release/camera-node".to_owned(),
        },
    )
    .with_output(
        OutputSpec::new(DataId::new("image")?)
            .with_type(TypeUrn::new("std/media/v1/Image[pixel=rgb8]")?),
    )
    .with_input(
        InputSpec::new(DataId::new("tick")?, sample_producer()?)
            .with_queue(10, QueuePolicy::DropOldest),
    );
    Ok(spec)
}

/// The handshake greeting every sample uses.
///
/// # Errors
///
/// Never for this fixed value; the signature matches its siblings.
pub fn sample_hello() -> WireResult<Hello> {
    Ok(Hello {
        protocol: crate::version::PROTOCOL_VERSION,
        astrs_version: sample_version(),
        role: Role::Node,
        auth: sample_token(),
        features: FeatureFlags::SHM_ZERO_COPY | FeatureFlags::TRACING,
        limits: NegotiatedLimits::uds(),
        resume: None,
        label: Some("camera".to_owned()),
    })
}

/// The handshake acceptance every sample uses.
///
/// # Errors
///
/// Never for this fixed value; the signature matches its siblings.
pub fn sample_welcome() -> WireResult<Welcome> {
    Ok(Welcome {
        protocol: crate::version::PROTOCOL_VERSION,
        limits: NegotiatedLimits::network(),
        session_id: sample_session(),
        features: FeatureFlags::SHM_ZERO_COPY,
        peer_role: Role::Node,
        astrs_version: sample_version(),
        resumed: false,
    })
}

/// The handshake refusal every sample uses.
#[must_use]
pub fn sample_refused() -> Refused {
    Refused {
        max_protocol: crate::version::PROTOCOL_VERSION,
        min_protocol: crate::version::MIN_SUPPORTED_PROTOCOL,
        reason: RefusalReason::BadAuth,
    }
}

/// The log record every sample uses.
///
/// # Errors
///
/// As [`sample_producer`].
pub fn sample_log_record() -> WireResult<LogRecord> {
    Ok(
        LogRecord::new(sample_timestamp(), LogLevel::Warn, "queue is filling up")
            .with_dataflow(sample_dataflow())
            .with_node(NodeId::new("camera")?)
            .with_target("astrs_daemon::routes"),
    )
}

/// The daemon load figures every sample uses.
#[must_use]
pub fn sample_daemon_stats() -> DaemonStats {
    DaemonStats {
        uptime: DurationMs::from_secs(3_600),
        node_count: 4,
        dataflow_count: 1,
        cpu_percent: 12.5,
        rss_bytes: 64 * 1024 * 1024,
        shm_bytes_mapped: 8 * 1024 * 1024,
        shm_fallback_total: 2,
        frames_sent: 10_000,
        frames_received: 9_000,
        bytes_sent: 1 << 24,
        bytes_received: 1 << 22,
    }
}

/// The per-node metrics sample every sample uses.
///
/// # Errors
///
/// As [`sample_producer`].
pub fn sample_node_metrics() -> WireResult<NodeMetricsSample> {
    let mut metrics = NodeMetricsSample::new(NodeId::new("camera")?, sample_timestamp());
    metrics.cpu_percent = 7.5;
    metrics.rss_bytes = 32 * 1024 * 1024;
    metrics.queue_depths.insert(DataId::new("tick")?, 3);
    metrics.sent_total.insert(DataId::new("image")?, 900);
    metrics.received_total.insert(DataId::new("tick")?, 900);
    metrics.dropped_total.insert(DataId::new("tick")?, 1);
    metrics.shm_slots_in_use = 5;
    Ok(metrics)
}

/// The per-node bandwidth sample every sample uses (§13).
///
/// # Errors
///
/// As [`sample_producer`].
pub fn sample_node_io() -> WireResult<NodeIoSample> {
    let mut io = NodeIoSample::new(NodeId::new("camera")?, sample_timestamp());
    io.sent_bytes_total
        .insert(DataId::new("image")?, 900 * 4_096);
    io.received_bytes_total
        .insert(DataId::new("tick")?, 900 * 8);
    io.shm_slots_in_use = 5;
    io.shm_slots_total = 16;
    io.shm_fallback_total = 2;
    Ok(io)
}

/// The data-plane fan-out frame every sample uses.
///
/// # Errors
///
/// As [`sample_producer`].
pub fn sample_data_frame() -> WireResult<DataFrame> {
    Ok(DataFrame::new(
        SubscriptionId::new(9),
        sample_dataflow(),
        sample_producer()?,
        sample_metadata()?,
        vec![0xDE, 0xAD, 0xBE, 0xEF],
    ))
}

/// The log fan-out frame every sample uses.
///
/// # Errors
///
/// As [`sample_log_record`].
pub fn sample_log_frame() -> WireResult<LogFrame> {
    Ok(LogFrame::new(SubscriptionId::new(9), sample_log_record()?))
}

/// The telemetry fan-out frame every sample uses.
///
/// # Errors
///
/// Never for this fixed value; the signature matches its siblings.
pub fn sample_telemetry_frame() -> WireResult<TelemetryFrame> {
    let batch = MetricBatch::new(sample_timestamp(), "astrs.daemon")
        .with_point(MetricPoint::counter("frames_sent_total", 10_000).with_label("plane", "quic"))
        .with_point(MetricPoint::gauge("queue_depth", 3.0));
    Ok(TelemetryFrame::new(SubscriptionId::new(9), batch))
}

/// One [`ControlRequest`] per variant, in wire-index order.
///
/// # Errors
///
/// As [`sample_producer`].
pub fn control_requests() -> WireResult<Vec<ControlRequest>> {
    let dataflow = sample_dataflow();
    let camera = NodeId::new("camera")?;
    Ok(vec![
        ControlRequest::Hello(sample_hello()?),
        ControlRequest::Build {
            manifest: "name: demo\nnodes: []\n".to_owned(),
            working_dir: Some("/workspace/demo".to_owned()),
            name: Some("demo".to_owned()),
            force: false,
        },
        ControlRequest::WaitForBuild {
            build: sample_build(),
            timeout: Some(DurationMs::from_secs(300)),
        },
        ControlRequest::Start {
            source: DataflowSource::Build {
                build: sample_build(),
            },
            name: Some("demo".to_owned()),
            detach: false,
        },
        ControlRequest::WaitForSpawn {
            dataflow,
            timeout: Some(DurationMs::from_secs(30)),
        },
        ControlRequest::Check {
            dataflow: Some(dataflow),
        },
        ControlRequest::Stop {
            dataflow,
            grace: Some(DurationMs::from_secs(5)),
        },
        ControlRequest::StopByName {
            name: "demo".to_owned(),
            grace: None,
        },
        ControlRequest::Restart {
            dataflow,
            rebuild: false,
        },
        ControlRequest::RestartByName {
            name: "demo".to_owned(),
            rebuild: true,
        },
        ControlRequest::Logs {
            dataflow,
            node: Some(camera.clone()),
            query: LogQuery::new().with_min_level(LogLevel::Info),
        },
        ControlRequest::LogSubscribe {
            dataflow: Some(dataflow),
            node: None,
            query: LogQuery::new(),
            subscription: SubscriptionId::new(9),
        },
        ControlRequest::List { all: true },
        ControlRequest::Info {
            dataflow,
            include_nodes: true,
        },
        ControlRequest::Destroy { force: false },
        ControlRequest::Clean {
            dataflow: Some(dataflow),
            artifacts: true,
            logs: false,
        },
        ControlRequest::ConnectedDaemons {
            include_unreachable: false,
        },
        ControlRequest::GetNodeInfo {
            dataflow,
            node: camera.clone(),
        },
        ControlRequest::TopicSubscribe {
            dataflow,
            port: sample_producer()?,
            query: TopicQuery::new().with_max_rate_hz(Some(2)),
            subscription: SubscriptionId::new(9),
        },
        ControlRequest::TopicUnsubscribe {
            subscription: SubscriptionId::new(9),
        },
        ControlRequest::TopicPublish {
            dataflow,
            port: sample_producer()?,
            metadata: sample_metadata()?,
            payload: vec![1, 2, 3, 4],
        },
        ControlRequest::GetParams {
            scope: ParamScope::dataflow_scope(dataflow),
            prefix: Some("camera.".to_owned()),
            inherited: true,
        },
        ControlRequest::GetParam {
            scope: ParamScope::node(dataflow, camera.clone()),
            key: ParamKey::new("exposure")?,
            inherited: true,
        },
        ControlRequest::SetParam {
            scope: ParamScope::node(dataflow, camera.clone()),
            key: ParamKey::new("exposure")?,
            value: Parameter::Integer(12),
            create_only: false,
        },
        ControlRequest::DeleteParam {
            scope: ParamScope::Global,
            key: ParamKey::new("exposure")?,
        },
        ControlRequest::RestartNode {
            dataflow,
            node: camera.clone(),
        },
        ControlRequest::StopNode {
            dataflow,
            node: camera.clone(),
            grace: Some(DurationMs::from_secs(2)),
        },
        ControlRequest::AddNode {
            dataflow,
            node: Box::new(sample_spawn_spec()?),
            start: true,
        },
        ControlRequest::RemoveNode {
            dataflow,
            node: camera.clone(),
            grace: None,
        },
        ControlRequest::ReplaceNode {
            dataflow,
            node: Box::new(sample_spawn_spec()?),
            drain: true,
        },
        ControlRequest::AddEdge {
            dataflow,
            consumer: NodeId::new("detector")?,
            input: InputSpec::new(DataId::new("frames")?, sample_producer()?),
        },
        ControlRequest::RemoveEdge {
            dataflow,
            consumer: NodeId::new("detector")?,
            input: DataId::new("frames")?,
        },
        ControlRequest::RecordStart {
            dataflow,
            path: "recordings/demo.arec".to_owned(),
            ports: vec![sample_producer()?],
            overwrite: false,
        },
        ControlRequest::RecordStop { dataflow },
        ControlRequest::GetTraces {
            dataflow: Some(dataflow),
            node: Some(camera.clone()),
            since: Some(sample_timestamp()),
            limit: Some(100),
        },
        ControlRequest::GetNodeMetrics {
            dataflow,
            node: Some(camera),
        },
        ControlRequest::GetManifest { dataflow },
        ControlRequest::GetNodeIoMetrics {
            dataflow,
            node: Some(NodeId::new("camera")?),
        },
    ])
}

/// One [`ControlReply`] per variant, in wire-index order.
///
/// # Errors
///
/// As [`sample_producer`].
pub fn control_replies() -> WireResult<Vec<ControlReply>> {
    let dataflow = sample_dataflow();
    let camera = NodeId::new("camera")?;

    let mut result = DataflowResult::new(dataflow, sample_timestamp());
    result.record(camera.clone(), NodeExitCause::Success);
    result.status = DataflowStatus::Finished;
    result.message = "finished cleanly".to_owned();

    let node_info = NodeInfo {
        dataflow,
        node: camera.clone(),
        daemon: sample_daemon()?,
        state: NodeRunState::Running,
        pid: Some(4_242),
        generation: 3,
        restart_count: 1,
        inputs: BTreeMap::from([(DataId::new("tick")?, None)]),
        outputs: BTreeMap::from([(
            DataId::new("image")?,
            Some(TypeUrn::new("std/media/v1/Image[pixel=rgb8]")?),
        )]),
        started_at: Some(sample_timestamp()),
        exit_cause: None,
    };

    let daemon_info = DaemonInfo {
        id: sample_daemon()?,
        version: sample_version(),
        address: "quic://10.0.0.4:7407".to_owned(),
        connected_at: sample_timestamp(),
        node_count: 4,
        labels: BTreeMap::from([("zone".to_owned(), "front".to_owned())]),
        reachable: true,
    };

    let span = TraceSpan::new("trace-1", "span-1", "camera.tick", sample_timestamp())
        .with_end(sample_timestamp())
        .with_status(SpanStatus::Ok)
        .with_attribute("node", "camera");

    Ok(vec![
        ControlReply::Ok,
        ControlReply::Error {
            code: ErrorCode::NotFound,
            message: "no such dataflow".to_owned(),
            context: vec!["looked up by name".to_owned()],
        },
        ControlReply::DataflowList {
            dataflows: vec![DataflowSummary {
                id: dataflow,
                name: Some("demo".to_owned()),
                status: DataflowStatus::Running,
                daemons: vec![sample_daemon()?],
                node_count: 2,
                running_nodes: 2,
                started_at: Some(sample_timestamp()),
            }],
            nodes: vec![node_info.clone()],
        },
        ControlReply::DataflowResult {
            result: Box::new(result),
        },
        ControlReply::NodeInfo {
            nodes: vec![node_info],
        },
        ControlReply::DaemonList {
            daemons: vec![daemon_info],
        },
        ControlReply::Logs {
            records: vec![sample_log_record()?],
            truncated: true,
        },
        ControlReply::ParamValue {
            key: ParamKey::new("exposure")?,
            value: Some(Parameter::Integer(12)),
            scope: ParamScope::node(dataflow, camera),
        },
        ControlReply::ParamList {
            scope: ParamScope::dataflow_scope(dataflow),
            params: vec![(ParamKey::new("exposure")?, Parameter::Integer(12))],
        },
        ControlReply::TraceData {
            traces: TraceData {
                spans: vec![span],
                truncated: false,
            },
        },
        ControlReply::Refused(sample_refused()),
        ControlReply::Welcome(sample_welcome()?),
        ControlReply::BuildStarted {
            build: sample_build(),
        },
        ControlReply::Started {
            dataflow,
            name: Some("demo".to_owned()),
        },
        ControlReply::NodeMetrics {
            samples: vec![sample_node_metrics()?],
        },
        ControlReply::Manifest {
            yaml: "nodes:\n  - id: camera\n    path: ./camera\n".to_owned(),
            working_dir: Some("/workspace/demo".to_owned()),
        },
        ControlReply::NodeIoMetrics {
            samples: vec![sample_node_io()?],
        },
    ])
}

/// One [`CoordinatorEvent`] per variant, in wire-index order.
///
/// # Errors
///
/// As [`sample_producer`].
pub fn coordinator_events() -> WireResult<Vec<CoordinatorEvent>> {
    let dataflow = sample_dataflow();
    let camera = NodeId::new("camera")?;
    Ok(vec![
        CoordinatorEvent::Heartbeat {
            seq: 17,
            sent_at: sample_timestamp(),
        },
        CoordinatorEvent::Build {
            build: sample_build(),
            dataflow,
            steps: vec![
                BuildStep::new(camera.clone(), ["cargo", "build", "--release"])
                    .with_working_dir("nodes/camera")
                    .with_timeout(DurationMs::from_secs(600)),
            ],
            working_dir: Some("/workspace/demo".to_owned()),
        },
        CoordinatorEvent::Spawn {
            node: Box::new(sample_spawn_spec()?),
            routes: vec![sample_route()?],
            dataflow_name: Some("demo".to_owned()),
        },
        CoordinatorEvent::AllNodesReady {
            dataflow,
            failed: Vec::new(),
        },
        CoordinatorEvent::StopDataflow {
            dataflow,
            grace: Some(DurationMs::from_secs(5)),
            cause: StopCause::Requested,
        },
        CoordinatorEvent::ReloadNode {
            dataflow,
            node: camera.clone(),
            operator: Some(OperatorId::new("crop")?),
        },
        CoordinatorEvent::Logs {
            request: 5,
            dataflow: Some(dataflow),
            node: Some(camera.clone()),
            query: LogQuery::new().with_min_level(LogLevel::Warn),
        },
        CoordinatorEvent::RestartNode {
            dataflow,
            node: camera.clone(),
            generation: 4,
        },
        CoordinatorEvent::StopNode {
            dataflow,
            node: camera,
            grace: None,
            cause: StopCause::Requested,
        },
        CoordinatorEvent::SetParam {
            scope: ParamScope::dataflow_scope(dataflow),
            key: ParamKey::new("exposure")?,
            value: Parameter::Integer(12),
        },
        CoordinatorEvent::DeleteParam {
            scope: ParamScope::Global,
            key: ParamKey::new("exposure")?,
        },
        CoordinatorEvent::Destroy {
            grace: Some(DurationMs::from_secs(10)),
        },
        CoordinatorEvent::PeerDisconnected {
            daemon: sample_daemon()?,
            dataflows: vec![dataflow],
        },
        CoordinatorEvent::StateCatchUp {
            seq: 41,
            entries: vec![StateEntry::new(
                41,
                sample_timestamp(),
                StateEntryKind::DataflowStatus {
                    dataflow,
                    status: DataflowStatus::Running,
                    name: Some("demo".to_owned()),
                },
            )],
            final_batch: true,
        },
        CoordinatorEvent::TopicTapStart {
            dataflow,
            port: sample_producer()?,
            query: TopicQuery::new().with_latest_only(true),
            subscription: SubscriptionId::new(9),
        },
        CoordinatorEvent::TopicTapStop {
            subscription: SubscriptionId::new(9),
        },
        CoordinatorEvent::PeerRoutes {
            dataflow,
            directives: vec![PeerRouteDirective::new(
                sample_route()?,
                sample_daemon()?,
                "tcp:10.0.0.4:7409",
            )],
        },
        CoordinatorEvent::ReplaceNode {
            dataflow,
            node: Box::new(sample_spawn_spec()?),
        },
        CoordinatorEvent::AddEdge {
            dataflow,
            consumer: NodeId::new("detector")?,
            input: InputSpec::new(DataId::new("frames")?, sample_producer()?),
        },
        CoordinatorEvent::RemoveEdge {
            dataflow,
            consumer: NodeId::new("detector")?,
            input: DataId::new("frames")?,
        },
    ])
}

/// One [`DaemonEvent`] per variant, in wire-index order.
///
/// # Errors
///
/// As [`sample_producer`].
pub fn daemon_events() -> WireResult<Vec<DaemonEvent>> {
    let dataflow = sample_dataflow();
    let camera = NodeId::new("camera")?;
    Ok(vec![
        DaemonEvent::Register(DaemonRegistration {
            daemon: sample_daemon()?,
            machine: Some(MachineName::new("robot-01")?),
            address: "quic://10.0.0.4:7407".to_owned(),
            version: sample_version(),
            labels: BTreeMap::from([("zone".to_owned(), "front".to_owned())]),
            session: sample_session(),
            running_nodes: 0,
            catch_up_seq: 0,
        }),
        DaemonEvent::Heartbeat {
            seq: 17,
            sent_at: sample_timestamp(),
            stats: sample_daemon_stats(),
        },
        DaemonEvent::BuildResult {
            build: sample_build(),
            dataflow,
            outcome: BuildOutcome::Succeeded {
                artifacts: vec!["target/release/camera-node".to_owned()],
                took: DurationMs::from_secs(12),
            },
        },
        DaemonEvent::SpawnResult {
            dataflow,
            node: camera.clone(),
            generation: 3,
            outcome: SpawnOutcome::Spawned {
                pid: Some(4_242),
                started_at: sample_timestamp(),
            },
        },
        DaemonEvent::AllNodesReady {
            dataflow,
            nodes: vec![camera.clone()],
        },
        DaemonEvent::AllNodesFinished {
            dataflow,
            results: BTreeMap::from([(camera.clone(), NodeExitCause::Success)]),
        },
        DaemonEvent::NodeStopped {
            dataflow,
            node: camera.clone(),
            generation: 3,
            cause: NodeExitCause::ExitCode { code: 1 },
            restarting: true,
        },
        DaemonEvent::NodeMetrics {
            dataflow,
            samples: vec![sample_node_metrics()?],
        },
        DaemonEvent::Log {
            request: Some(5),
            records: vec![sample_log_record()?],
            truncated: false,
        },
        DaemonEvent::TopicTapData {
            frame: Box::new(sample_data_frame()?),
            dropped: 3,
        },
        DaemonEvent::StateCatchUpAck {
            seq: 41,
            applied: 1,
        },
        DaemonEvent::Exit {
            graceful: true,
            message: "asked to shut down".to_owned(),
        },
        DaemonEvent::NodeIoMetrics {
            dataflow: sample_dataflow(),
            samples: vec![sample_node_io()?],
        },
    ])
}

/// One [`NodeRequest`] per variant, in wire-index order.
///
/// # Errors
///
/// As [`sample_producer`].
pub fn node_requests() -> WireResult<Vec<NodeRequest>> {
    let key = ExtensionKey::new(ExtensionNamespace::User, "calibration")?;
    Ok(vec![
        NodeRequest::Register(NodeHandshake {
            dataflow: sample_dataflow(),
            node: NodeId::new("camera")?,
            generation: 3,
            pid: Some(4_242),
            version: sample_version(),
            dynamic: false,
            inputs: vec![DataId::new("tick")?],
            outputs: vec![DataId::new("image")?],
        }),
        NodeRequest::Subscribe {
            inputs: vec![DataId::new("tick")?],
        },
        NodeRequest::SendMessage {
            output: DataId::new("image")?,
            metadata: sample_metadata()?,
            payload: OutputPayload::inline(vec![0xAA, 0xBB, 0xCC]),
        },
        NodeRequest::OutputDone {
            output: DataId::new("image")?,
        },
        NodeRequest::CloseOutputs {
            outputs: vec![DataId::new("image")?],
        },
        NodeRequest::NextEvent {
            timeout: Some(DurationMs::new(100)),
            max_batch: 32,
        },
        NodeRequest::EventStreamDropped,
        NodeRequest::ExtStore {
            key: key.clone(),
            value: vec![1, 2, 3],
            ttl: Some(DurationMs::from_secs(60)),
        },
        NodeRequest::ExtLoad { key: key.clone() },
        NodeRequest::ExtDrop { key },
        NodeRequest::RouteUpgradeAck {
            output: DataId::new("image")?,
            accepted: true,
            reason: None,
        },
        NodeRequest::ReportDeadlineViolation {
            input: DataId::new("frames")?,
            budget: DurationMs::new(50),
            latency: DurationMs::new(80),
        },
    ])
}

/// One [`NodeEvent`] per variant, in wire-index order.
///
/// # Errors
///
/// As [`sample_producer`].
pub fn node_events() -> WireResult<Vec<NodeEvent>> {
    Ok(vec![
        NodeEvent::Input {
            id: DataId::new("frames")?,
            source: sample_producer()?,
            metadata: sample_metadata()?,
            payload: vec![0xDE, 0xAD, 0xBE, 0xEF],
        },
        NodeEvent::InputClosed {
            id: DataId::new("frames")?,
            source: sample_producer()?,
            reason: RouteCloseReason::ProducerFinished,
        },
        NodeEvent::InputRecovered {
            id: DataId::new("frames")?,
            source: sample_producer()?,
            generation: 4,
        },
        NodeEvent::Stop {
            cause: StopCause::Requested,
            grace: Some(DurationMs::from_secs(5)),
        },
        NodeEvent::Reload {
            operator: Some(OperatorId::new("crop")?),
            path: Some("./target/release/libcrop.so".to_owned()),
        },
        NodeEvent::AllInputsClosed,
        NodeEvent::NodeFailed {
            peer: NodeId::new("camera")?,
            cause: NodeExitCause::Panic {
                message: "index out of bounds".to_owned(),
            },
        },
        NodeEvent::Restarted {
            peer: NodeId::new("camera")?,
            generation: 4,
        },
        NodeEvent::ParamUpdate {
            scope: ParamScope::node(sample_dataflow(), NodeId::new("camera")?),
            key: ParamKey::new("exposure")?,
            value: Parameter::Integer(12),
        },
        NodeEvent::ParamDeleted {
            scope: ParamScope::Global,
            key: ParamKey::new("exposure")?,
        },
        NodeEvent::ExtDropped {
            key: ExtensionKey::new(ExtensionNamespace::PinnedMemory, "frame-pool")?,
            reason: "time to live expired".to_owned(),
        },
        NodeEvent::RouteUpgrade {
            output: DataId::new("image")?,
            segment: ShmSegmentSpec::new(
                "astrs/00000000-0000-0000-0000-000000000001/camera/3/image",
                3,
                32,
                1 << 20,
            ),
            consumers: vec![sample_consumer()?],
        },
        NodeEvent::RouteDowngrade {
            output: DataId::new("image")?,
            reason: RouteDowngradeReason::PoolExhausted,
        },
        NodeEvent::Registered {
            spec: Box::new(sample_spawn_spec()?),
            session: sample_session(),
        },
        NodeEvent::ExtValue {
            key: ExtensionKey::new(ExtensionNamespace::User, "calibration")?,
            value: Some(vec![1, 2, 3]),
        },
        NodeEvent::InputRouteUpgrade {
            input: DataId::new("frames")?,
            source: sample_producer()?,
            segment: ShmSegmentSpec::new(
                "astrs/00000000-0000-0000-0000-000000000001/camera/3/image",
                3,
                32,
                1 << 20,
            ),
            consumer: sample_consumer()?,
        },
        NodeEvent::InputRouteDowngrade {
            input: DataId::new("frames")?,
            reason: RouteDowngradeReason::SegmentClosed { generation: 3 },
        },
        NodeEvent::DeadlineViolated {
            peer: NodeId::new("camera")?,
            input: DataId::new("frames")?,
            budget: DurationMs::new(50),
            latency: DurationMs::new(80),
        },
    ])
}

/// One [`PeerEvent`] per variant, in wire-index order.
///
/// # Errors
///
/// As [`sample_producer`].
pub fn peer_events() -> WireResult<Vec<PeerEvent>> {
    Ok(vec![
        PeerEvent::RouteSetup {
            route_id: RouteId::FIRST,
            route: sample_route()?,
            generation: 3,
            type_urn: Some(TypeUrn::new("std/media/v1/Image[pixel=rgb8]")?),
            max_payload_bytes: 4 * 1024 * 1024,
            pool_hint_bytes: Some(8 * 1024 * 1024),
        },
        PeerEvent::RouteAccept {
            route_id: RouteId::FIRST,
            acceptance: RouteAcceptance::accepted(Plane::Quic, Compression::Zstd, 4 * 1024 * 1024),
        },
        PeerEvent::RouteTeardown {
            route_id: RouteId::FIRST,
            reason: RouteCloseReason::DataflowStopped,
        },
        PeerEvent::Output {
            route_id: RouteId::FIRST,
            seq: 900,
            metadata: sample_metadata()?,
            payload: vec![0xDE, 0xAD, 0xBE, 0xEF],
        },
        PeerEvent::OutputClosed {
            route_id: RouteId::FIRST,
            final_seq: 900,
            reason: RouteCloseReason::ProducerFinished,
        },
        PeerEvent::Ping {
            nonce: 0xFEED,
            sent_at: sample_timestamp(),
            is_reply: false,
        },
    ])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::messages::WireMessage;

    #[test]
    fn samples_are_infallible() {
        assert!(control_requests().is_ok());
        assert!(control_replies().is_ok());
        assert!(coordinator_events().is_ok());
        assert!(daemon_events().is_ok());
        assert!(node_requests().is_ok());
        assert!(node_events().is_ok());
        assert!(peer_events().is_ok());
        assert!(sample_data_frame().is_ok());
        assert!(sample_log_frame().is_ok());
        assert!(sample_telemetry_frame().is_ok());
        assert!(sample_daemon().is_ok());
    }

    #[test]
    fn every_family_table_is_complete_and_ordered() {
        macro_rules! check {
            ($samples:expr, $ty:ty) => {{
                let samples = $samples.unwrap();
                assert_eq!(
                    samples.len(),
                    <$ty as WireMessage>::VARIANT_NAMES.len(),
                    "{} sample count",
                    stringify!($ty)
                );
                for (index, sample) in samples.iter().enumerate() {
                    assert_eq!(
                        usize::from(WireMessage::variant_index(sample)),
                        index,
                        "{} sample {index} is out of order",
                        stringify!($ty)
                    );
                }
            }};
        }

        check!(control_requests(), ControlRequest);
        check!(control_replies(), ControlReply);
        check!(coordinator_events(), CoordinatorEvent);
        check!(daemon_events(), DaemonEvent);
        check!(node_requests(), NodeRequest);
        check!(node_events(), NodeEvent);
        check!(peer_events(), PeerEvent);
    }

    #[test]
    fn samples_never_call_a_clock_or_a_generator() {
        // Two independent calls must produce identical values, which is what
        // makes the snapshot a snapshot.
        assert_eq!(sample_timestamp(), sample_timestamp());
        assert_eq!(sample_daemon().unwrap(), sample_daemon().unwrap());
        assert_eq!(sample_session(), sample_session());
        assert_eq!(control_requests().unwrap(), control_requests().unwrap());
        assert_eq!(peer_events().unwrap(), peer_events().unwrap());
    }

    #[test]
    fn the_sample_version_is_pinned_and_unreachable_by_a_release() {
        assert_eq!(sample_version(), AstrsVersion::from_parts(0, 0, 0));
        assert_ne!(
            sample_version(),
            AstrsVersion::current(),
            "the sample version must never coincide with a released one, or a \
             version bump would silently change the protocol snapshot"
        );
    }

    #[test]
    fn the_handshake_samples_agree_with_each_other() {
        let hello = sample_hello().unwrap();
        let welcome = sample_welcome().unwrap();
        assert_eq!(welcome.peer_role, hello.role);
        assert!(hello.features.contains(welcome.features));
        assert_eq!(
            crate::handshake::negotiate::accept_welcome(&hello, &welcome)
                .map(|session| session.session_id),
            Ok(sample_session())
        );
    }
}
