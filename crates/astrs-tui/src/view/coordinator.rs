//! [`CoordinatorSource`]: a [`crate::ClusterView`] fed by a live
//! coordinator connection, polled on every
//! [`crate::ClusterView::refresh`] (blueprint §17), with a live log tail
//! (`LogSubscribe`, blueprint §13).
//!
//! # Why a background thread
//!
//! [`crate::ClusterView::refresh`] is synchronous and must not block the
//! render loop; the coordinator connection is necessarily asynchronous
//! network I/O. [`CoordinatorSource::connect`] dials and completes the
//! handshake synchronously (so a bad address or a refused connection is
//! reported from the constructor, not silently as a later `Degraded`
//! status), then spawns a dedicated thread running its own
//! single-threaded Tokio runtime that owns the socket for the rest of the
//! session: it polls [`astrs_wire::ControlRequest::List`],
//! [`astrs_wire::ControlRequest::Info`] and
//! [`astrs_wire::ControlRequest::GetNodeMetrics`] on [`POLL_INTERVAL`],
//! fetches [`astrs_wire::ControlRequest::GetManifest`] exactly once per
//! dataflow id, and forwards every pushed [`astrs_wire::LogRecord`] from
//! its one `LogSubscribe` — all as internal update messages over a plain
//! [`std::sync::mpsc`] channel. [`CoordinatorSource::refresh`] only ever
//! drains that channel, so it can never block on a slow or wedged
//! coordinator.
//!
//! # Why polling, not a metrics subscription
//!
//! The daemon itself re-samples per-node metrics every two seconds
//! (blueprint §13); a push subscription for them would buy no freshness
//! over polling at the same cadence, and would need a new
//! [`astrs_wire::FrameKind`] payload shape alongside the existing
//! [`astrs_wire::TelemetryFrame`] to preserve
//! [`astrs_wire::NodeMetricsSample`]'s structured queue-depth map. Logs
//! are different: an operator wants every line, not a periodic sample, so
//! [`astrs_wire::ControlRequest::LogSubscribe`] stays a genuine push.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use astrs_time::HlcClock;
use astrs_transport::{ConnectionCounters, FramedStream, HandshakeParams, LocalIdentity, initiate};
use astrs_wire::{
    AuthToken, ControlReply, ControlRequest, DataflowId, DataflowSummary, FeatureFlags, FrameKind,
    FrameLimits, LogFrame, LogQuery, LogRecord, NodeId, NodeInfo, NodeMetricsSample, Role,
    SubscriptionId, WireDecode,
};
use tokio::net::TcpStream;

use crate::view::{
    ClusterSnapshot, ClusterView, ConnectionStatus, DataflowRow, NodeRow, TimelineCategory,
    TimelineEvent, ViewError, graph_info_from_manifest, parse_manifest,
};

/// How often the background task polls dataflow/node/metrics state.
///
/// Matches the daemon's own metrics sampling interval (§13): polling
/// faster would never observe fresher data.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How long the initial handshake may take before [`CoordinatorSource::connect`]
/// gives up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long any single request may take before the background connection
/// gives up and reports [`ConnectionStatus::Degraded`].
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a [`CoordinatorSource`] could not be established.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CoordinatorSourceError {
    /// The handshake failed, or the socket could not be opened at all.
    #[error("cannot reach the coordinator at {endpoint}: {reason}")]
    Connect {
        /// The address dialled.
        endpoint: String,
        /// Why it failed.
        reason: String,
    },
    /// The background polling thread could not be started.
    #[error("cannot start the coordinator connection thread: {0}")]
    Thread(#[source] std::io::Error),
}

/// One update the background task hands to the main-thread
/// [`CoordinatorSource`] over the channel.
enum Update {
    /// A fresh `List` result.
    Dataflows(Vec<DataflowSummary>),
    /// A fresh `Info` result for one dataflow.
    NodeInfo {
        /// Which dataflow.
        dataflow: DataflowId,
        /// Its nodes.
        nodes: Vec<NodeInfo>,
    },
    /// A fresh `GetNodeIoMetrics` result for one dataflow (§13's bandwidth
    /// half).
    Io {
        /// The dataflow the samples belong to.
        dataflow: DataflowId,
        /// One sample per node the coordinator holds a reading for.
        samples: Vec<astrs_wire::NodeIoSample>,
    },
    /// A fresh `GetNodeMetrics` result for one dataflow.
    Metrics {
        /// Which dataflow.
        dataflow: DataflowId,
        /// Its node samples.
        samples: Vec<NodeMetricsSample>,
    },
    /// A `GetManifest` result, fetched once per dataflow id.
    Manifest {
        /// Which dataflow.
        dataflow: DataflowId,
        /// Its expanded manifest YAML.
        yaml: String,
    },
    /// One pushed log record.
    Log(LogRecord),
    /// The connection ended; the background thread has exited.
    Disconnected(String),
}

/// A [`ClusterView`] backed by a live coordinator connection.
#[derive(Debug)]
pub struct CoordinatorSource {
    receiver: std_mpsc::Receiver<Update>,
    endpoint_display: String,
    hlc: HlcClock,
    snapshot: ClusterSnapshot,
}

impl CoordinatorSource {
    /// Dials `addr`, greets it as [`Role::Cli`] with `token`, and spawns
    /// the background polling/log-tailing thread.
    ///
    /// # Errors
    ///
    /// [`CoordinatorSourceError::Connect`] if the socket cannot be opened
    /// or the handshake is refused; [`CoordinatorSourceError::Thread`] if
    /// the background thread cannot be spawned.
    pub fn connect(addr: SocketAddr, token: AuthToken) -> Result<Self, CoordinatorSourceError> {
        let endpoint_display = addr.to_string();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(CoordinatorSourceError::Thread)?;

        let stream = runtime.block_on(dial(addr, token)).map_err(|reason| {
            CoordinatorSourceError::Connect {
                endpoint: endpoint_display.clone(),
                reason,
            }
        })?;

        let (sender, receiver) = std_mpsc::channel();
        std::thread::Builder::new()
            .name("astrs-tui-coordinator".to_owned())
            .spawn(move || runtime.block_on(drive(stream, &sender)))
            .map_err(CoordinatorSourceError::Thread)?;

        let mut snapshot = ClusterSnapshot::empty();
        snapshot.connection = ConnectionStatus::Live {
            endpoint: endpoint_display.clone(),
        };
        Ok(Self {
            receiver,
            endpoint_display,
            hlc: HlcClock::system(),
            snapshot,
        })
    }

    /// Applies one update, synthesizing a [`TimelineEvent`] for anything
    /// that looks like a lifecycle change (blueprint §13: spawns,
    /// restarts, status changes, violations).
    fn apply(&mut self, update: Update) {
        match update {
            Update::Dataflows(list) => self.apply_dataflows(list),
            Update::NodeInfo { dataflow, nodes } => self.apply_node_info(dataflow, nodes),
            Update::Metrics { dataflow, samples } => self.apply_metrics(dataflow, samples),
            Update::Io { dataflow, samples } => self.apply_io(dataflow, samples),
            Update::Manifest { dataflow, yaml } => self.apply_manifest(dataflow, yaml),
            Update::Log(record) => self.snapshot.push_log(record),
            Update::Disconnected(reason) => {
                self.snapshot.connection = ConnectionStatus::Degraded {
                    endpoint: self.endpoint_display.clone(),
                    reason,
                };
                return;
            }
        }
        self.snapshot.connection = ConnectionStatus::Live {
            endpoint: self.endpoint_display.clone(),
        };
    }

    fn apply_dataflows(&mut self, list: Vec<DataflowSummary>) {
        // Collected first, then emitted after every `self.snapshot`
        // borrow below has ended — `emit` also needs `&mut self`.
        let mut events: Vec<(TimelineCategory, DataflowId, String)> = Vec::new();
        for summary in list {
            let id = summary.id;
            if let Some(row) = self.snapshot.dataflow_mut(id) {
                if row.summary.status != summary.status {
                    events.push((
                        TimelineCategory::Status,
                        id,
                        format!(
                            "{}: {} \u{2192} {}",
                            summary.display_name(),
                            row.summary.status.as_str(),
                            summary.status.as_str()
                        ),
                    ));
                }
                row.summary = summary;
            } else {
                events.push((
                    TimelineCategory::Spawn,
                    id,
                    format!("dataflow {} appeared", summary.display_name()),
                ));
                self.snapshot.dataflows.push(DataflowRow::new(summary));
            }
        }
        for (category, id, message) in events {
            self.emit(category, Some(id), None, message);
        }
    }

    fn apply_node_info(&mut self, dataflow: DataflowId, nodes: Vec<NodeInfo>) {
        let Some(row) = self.snapshot.dataflow_mut(dataflow) else {
            return;
        };
        let mut updated = Vec::with_capacity(nodes.len());
        let mut events: Vec<(TimelineCategory, NodeId, String)> = Vec::new();
        for info in nodes {
            let previous = row.node(&info.node).cloned();
            match &previous {
                None => {
                    events.push((
                        TimelineCategory::Spawn,
                        info.node.clone(),
                        format!("{} spawned", info.node),
                    ));
                }
                Some(old) if old.info.restart_count < info.restart_count => {
                    events.push((
                        TimelineCategory::Restart,
                        info.node.clone(),
                        format!("{} restarted ({} total)", info.node, info.restart_count),
                    ));
                }
                Some(old) if old.info.state != info.state => {
                    events.push((
                        TimelineCategory::Status,
                        info.node.clone(),
                        format!(
                            "{}: {} \u{2192} {}",
                            info.node,
                            old.info.state.as_str(),
                            info.state.as_str()
                        ),
                    ));
                }
                Some(_) => {}
            }
            let (metrics, io, previous_io) = previous.map_or((None, None, None), |old| {
                (old.metrics, old.io, old.previous_io)
            });
            updated.push(NodeRow {
                info,
                metrics,
                io,
                previous_io,
            });
        }
        row.nodes = updated;
        for (category, node, message) in events {
            self.emit(category, Some(dataflow), Some(node), message);
        }
    }

    /// Records one bandwidth round, keeping the previous reading beside it.
    ///
    /// The pair is what a rate is made of — see [`crate::view::NodeRow`]. A
    /// sample whose timestamp did not advance is still stored, and
    /// [`astrs_wire::NodeIoSample::delta_since`] declines to divide by it.
    fn apply_io(&mut self, dataflow: DataflowId, samples: Vec<astrs_wire::NodeIoSample>) {
        let Some(row) = self.snapshot.dataflow_mut(dataflow) else {
            return;
        };
        for sample in samples {
            let Some(node_row) = row
                .nodes
                .iter_mut()
                .find(|row| row.info.node == sample.node)
            else {
                continue;
            };
            node_row.previous_io = node_row.io.replace(sample);
        }
    }

    fn apply_metrics(&mut self, dataflow: DataflowId, samples: Vec<NodeMetricsSample>) {
        let mut violations: Vec<(NodeId, u64)> = Vec::new();
        if let Some(row) = self.snapshot.dataflow_mut(dataflow) {
            for sample in samples {
                let Some(node_row) = row
                    .nodes
                    .iter_mut()
                    .find(|row| row.info.node == sample.node)
                else {
                    continue;
                };
                // Only a *previous* sample gives a baseline to diff
                // against — a first-ever reading may already carry a
                // nonzero drop count from before this view connected,
                // and that is not a new violation, just history this
                // view was not present for.
                if let Some(previous) = &node_row.metrics {
                    let dropped_delta = sample
                        .total_dropped()
                        .saturating_sub(previous.total_dropped());
                    if dropped_delta > 0 {
                        violations.push((sample.node.clone(), dropped_delta));
                    }
                }
                node_row.metrics = Some(sample);
            }
        }
        for (node, delta) in violations {
            let message = format!("{node}: {delta} message(s) dropped since the last sample");
            self.emit(
                TimelineCategory::Violation,
                Some(dataflow),
                Some(node),
                message,
            );
        }
    }

    fn apply_manifest(&mut self, dataflow: DataflowId, yaml: String) {
        let Some(manifest) = parse_manifest(&yaml) else {
            return;
        };
        let Some(info) = graph_info_from_manifest(&manifest) else {
            return;
        };
        if let Some(row) = self.snapshot.dataflow_mut(dataflow) {
            row.graph = Some(info);
        }
    }

    /// Stamps and pushes one synthesized timeline event, using this
    /// view's own [`HlcClock`] — there is no live daemon HLC to read here,
    /// so this at least keeps every synthesized event self-consistently
    /// ordered within one `astrs top` session.
    fn emit(
        &mut self,
        category: TimelineCategory,
        dataflow: Option<DataflowId>,
        node: Option<NodeId>,
        message: String,
    ) {
        let mut event = TimelineEvent::new(self.hlc.now(), category, message);
        event.dataflow = dataflow;
        event.node = node;
        self.snapshot.push_timeline(event);
    }
}

#[cfg(test)]
impl CoordinatorSource {
    /// A source with no live connection at all — `apply`'s diffing logic
    /// does not need one, so tests drive it directly rather than through
    /// a real socket.
    fn for_test() -> Self {
        let (_sender, receiver) = std_mpsc::channel();
        Self {
            receiver,
            endpoint_display: "test".to_owned(),
            hlc: HlcClock::system(),
            snapshot: ClusterSnapshot::empty(),
        }
    }
}

impl ClusterView for CoordinatorSource {
    fn refresh(&mut self) -> Result<(), ViewError> {
        loop {
            match self.receiver.try_recv() {
                Ok(update) => self.apply(update),
                Err(std_mpsc::TryRecvError::Empty) => return Ok(()),
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    let reason = "the background connection thread ended".to_owned();
                    self.snapshot.connection = ConnectionStatus::Degraded {
                        endpoint: self.endpoint_display.clone(),
                        reason: reason.clone(),
                    };
                    return Err(ViewError::Disconnected(reason));
                }
            }
        }
    }

    fn snapshot(&self) -> &ClusterSnapshot {
        &self.snapshot
    }
}

/// Dials and completes the handshake, on the caller's own (blocking)
/// call, before any background thread exists.
async fn dial(addr: SocketAddr, token: AuthToken) -> Result<FramedStream<TcpStream>, String> {
    let raw = TcpStream::connect(addr)
        .await
        .map_err(|error| error.to_string())?;
    let _ = raw.set_nodelay(true);
    let mut stream = FramedStream::new(raw, FrameLimits::network(), ConnectionCounters::shared());
    let params = HandshakeParams::new(LocalIdentity::new(Role::Cli), token)
        .with_features(FeatureFlags::EMPTY);
    initiate(&mut stream, &params, HANDSHAKE_TIMEOUT)
        .await
        .map_err(|error| error.to_string())?;
    Ok(stream)
}

/// The background thread's whole life: subscribe to logs once, then poll
/// forever until something fails.
async fn drive(mut stream: FramedStream<TcpStream>, sender: &std_mpsc::Sender<Update>) {
    let subscription = new_subscription_id();
    let subscribe = ControlRequest::LogSubscribe {
        dataflow: None,
        node: None,
        query: LogQuery::new(),
        subscription,
    };
    if let Err(reason) = request(&mut stream, &subscribe, sender).await {
        let _ = sender.send(Update::Disconnected(reason));
        return;
    }

    let mut manifests_fetched: BTreeSet<DataflowId> = BTreeSet::new();
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    loop {
        ticker.tick().await;
        if let Err(reason) = poll_once(&mut stream, &mut manifests_fetched, sender).await {
            let _ = sender.send(Update::Disconnected(reason));
            return;
        }
    }
}

/// One polling cycle: `List`, then `Info` + `GetNodeMetrics` +
/// `GetNodeIoMetrics` for every dataflow, plus `GetManifest` the first time a
/// dataflow id is seen.
async fn poll_once(
    stream: &mut FramedStream<TcpStream>,
    manifests_fetched: &mut BTreeSet<DataflowId>,
    sender: &std_mpsc::Sender<Update>,
) -> Result<(), String> {
    let reply = request(stream, &ControlRequest::List { all: true }, sender).await?;
    let ControlReply::DataflowList { dataflows, .. } = reply else {
        return Err(format!("unexpected reply to List: {reply}"));
    };
    let ids: Vec<DataflowId> = dataflows.iter().map(|summary| summary.id).collect();
    send(sender, Update::Dataflows(dataflows))?;

    for dataflow in ids {
        let reply = request(
            stream,
            &ControlRequest::Info {
                dataflow,
                include_nodes: true,
            },
            sender,
        )
        .await?;
        if let ControlReply::DataflowList { nodes, .. } = reply {
            send(sender, Update::NodeInfo { dataflow, nodes })?;
        }

        let reply = request(
            stream,
            &ControlRequest::GetNodeMetrics {
                dataflow,
                node: None,
            },
            sender,
        )
        .await?;
        if let ControlReply::NodeMetrics { samples } = reply {
            send(sender, Update::Metrics { dataflow, samples })?;
        }

        let reply = request(
            stream,
            &ControlRequest::GetNodeIoMetrics {
                dataflow,
                node: None,
            },
            sender,
        )
        .await?;
        if let ControlReply::NodeIoMetrics { samples } = reply {
            send(sender, Update::Io { dataflow, samples })?;
        }

        if manifests_fetched.insert(dataflow) {
            let reply = request(stream, &ControlRequest::GetManifest { dataflow }, sender).await?;
            if let ControlReply::Manifest { yaml, .. } = reply {
                send(sender, Update::Manifest { dataflow, yaml })?;
            }
        }
    }
    Ok(())
}

/// Sends one request, siphoning off any pushed [`FrameKind::Log`] frames
/// encountered while waiting for the matching
/// [`FrameKind::ControlReply`] — the two families interleave freely on
/// this connection because of the standing `LogSubscribe`
/// (blueprint §7.3: log/topic pushes ride the same framing as replies,
/// tagged by kind rather than a bespoke side channel).
async fn request(
    stream: &mut FramedStream<TcpStream>,
    message: &ControlRequest,
    sender: &std_mpsc::Sender<Update>,
) -> Result<ControlReply, String> {
    tokio::time::timeout(REQUEST_TIMEOUT, request_inner(stream, message, sender))
        .await
        .map_err(|_| "the coordinator did not answer in time".to_owned())?
}

async fn request_inner(
    stream: &mut FramedStream<TcpStream>,
    message: &ControlRequest,
    sender: &std_mpsc::Sender<Update>,
) -> Result<ControlReply, String> {
    stream
        .send_message(message)
        .await
        .map_err(|error| error.to_string())?;
    loop {
        let frame = stream
            .recv_frame()
            .await
            .map_err(|error| error.to_string())?;
        let Some(frame) = frame else {
            return Err("the coordinator closed the connection".to_owned());
        };
        match frame.kind() {
            FrameKind::Log => {
                if let Ok(log) = LogFrame::decode_exact(frame.payload()) {
                    let _ = sender.send(Update::Log(log.record));
                }
            }
            FrameKind::ControlReply => {
                return ControlReply::decode_exact(frame.payload())
                    .map_err(|error| error.to_string());
            }
            // Any other pushed family (a topic tap this connection never
            // opened, say) is not this view's concern; drop it and keep
            // waiting for the reply.
            _ => {}
        }
    }
}

/// Sends `update`, translating a closed receiver into the same `String`
/// error every other failure in this module reports.
fn send(sender: &std_mpsc::Sender<Update>, update: Update) -> Result<(), String> {
    sender
        .send(update)
        .map_err(|_| "the view was dropped".to_owned())
}

/// Mints a subscription id unlikely to collide with another client of the
/// same coordinator — including another `astrs top` session, or an
/// `astrs logs -f`, started at close to the same moment. Mirrors
/// `astrs-cli`'s own [`SubscriptionId`] minting rationale exactly (this
/// crate cannot depend on that one — `astrs-tui` sits beside it, not below
/// it, in the layer stack, §4.1): there is no server-side allocator to ask
/// (§7.3), so the id is derived here from three things that cannot all
/// coincide between two live processes: this process's id, the
/// nanoseconds since the epoch, and a per-process counter.
fn new_subscription_id() -> SubscriptionId {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
        });
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in [
        u64::from(std::process::id()),
        nanos,
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ] {
        for byte in part.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    SubscriptionId::new(hash.max(1))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::net::SocketAddr;

    use astrs_wire::AuthToken;

    use super::*;

    #[test]
    fn subscription_ids_minted_in_the_same_process_are_distinct() {
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..64 {
            assert!(seen.insert(new_subscription_id()));
        }
    }

    #[test]
    fn subscription_ids_are_never_the_reserved_none_value() {
        for _ in 0..64 {
            assert_ne!(new_subscription_id(), SubscriptionId::NONE);
        }
    }

    #[test]
    fn connecting_to_nothing_reports_a_connect_error_not_a_hang() {
        // Port 1 on loopback: privileged, never bound by this suite,
        // refused immediately rather than timing out.
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let error = CoordinatorSource::connect(addr, AuthToken::ZERO).unwrap_err();
        match error {
            CoordinatorSourceError::Connect { endpoint, .. } => {
                assert!(endpoint.contains("127.0.0.1"));
            }
            other => panic!("expected Connect, got {other}"),
        }
    }

    fn summary(id: DataflowId, status: astrs_wire::DataflowStatus) -> DataflowSummary {
        DataflowSummary {
            id,
            name: Some("perception".to_owned()),
            status,
            daemons: Vec::new(),
            node_count: 1,
            running_nodes: 1,
            started_at: None,
        }
    }

    fn node_info(
        dataflow: DataflowId,
        id: &str,
        state: astrs_wire::NodeRunState,
        restarts: u32,
    ) -> NodeInfo {
        NodeInfo {
            dataflow,
            node: NodeId::new(id).unwrap(),
            daemon: astrs_wire::DaemonId::generate(None),
            state,
            pid: Some(1),
            generation: 1,
            restart_count: restarts,
            inputs: std::collections::BTreeMap::new(),
            outputs: std::collections::BTreeMap::new(),
            started_at: None,
            exit_cause: None,
        }
    }

    #[test]
    fn a_new_dataflow_is_added_and_reported_as_a_spawn() {
        let mut source = CoordinatorSource::for_test();
        let id = DataflowId::from_u128(1);
        source.apply(Update::Dataflows(vec![summary(
            id,
            astrs_wire::DataflowStatus::Running,
        )]));

        assert_eq!(source.snapshot().dataflows.len(), 1);
        assert_eq!(source.snapshot().timeline.len(), 1);
        assert_eq!(
            source.snapshot().timeline[0].category,
            TimelineCategory::Spawn
        );
        assert!(matches!(
            source.snapshot().connection,
            ConnectionStatus::Live { .. }
        ));
    }

    #[test]
    fn a_dataflow_status_change_is_reported_and_applied() {
        let mut source = CoordinatorSource::for_test();
        let id = DataflowId::from_u128(1);
        source.apply(Update::Dataflows(vec![summary(
            id,
            astrs_wire::DataflowStatus::Starting,
        )]));
        source.apply(Update::Dataflows(vec![summary(
            id,
            astrs_wire::DataflowStatus::Running,
        )]));

        assert_eq!(
            source.snapshot().dataflows[0].summary.status,
            astrs_wire::DataflowStatus::Running
        );
        let statuses: Vec<_> = source
            .snapshot()
            .timeline
            .iter()
            .map(|event| event.category)
            .collect();
        assert_eq!(
            statuses,
            vec![TimelineCategory::Spawn, TimelineCategory::Status]
        );
    }

    #[test]
    fn a_new_node_is_reported_as_a_spawn_and_added_to_its_dataflow() {
        let mut source = CoordinatorSource::for_test();
        let id = DataflowId::from_u128(1);
        source.apply(Update::Dataflows(vec![summary(
            id,
            astrs_wire::DataflowStatus::Running,
        )]));
        source.apply(Update::NodeInfo {
            dataflow: id,
            nodes: vec![node_info(
                id,
                "camera",
                astrs_wire::NodeRunState::Running,
                0,
            )],
        });

        let row = source.snapshot().dataflow(id).unwrap();
        assert_eq!(row.nodes.len(), 1);
        assert!(
            source
                .snapshot()
                .timeline
                .iter()
                .any(|event| event.category == TimelineCategory::Spawn
                    && event.node == Some(NodeId::new("camera").unwrap()))
        );
    }

    #[test]
    fn a_restart_count_increase_is_reported_as_a_restart_not_a_status_change() {
        let mut source = CoordinatorSource::for_test();
        let id = DataflowId::from_u128(1);
        source.apply(Update::Dataflows(vec![summary(
            id,
            astrs_wire::DataflowStatus::Running,
        )]));
        source.apply(Update::NodeInfo {
            dataflow: id,
            nodes: vec![node_info(
                id,
                "camera",
                astrs_wire::NodeRunState::Running,
                0,
            )],
        });
        source.apply(Update::NodeInfo {
            dataflow: id,
            nodes: vec![node_info(
                id,
                "camera",
                astrs_wire::NodeRunState::Running,
                1,
            )],
        });

        let categories: Vec<_> = source
            .snapshot()
            .timeline
            .iter()
            .map(|event| event.category)
            .collect();
        assert_eq!(
            categories,
            vec![
                TimelineCategory::Spawn,
                TimelineCategory::Spawn,
                TimelineCategory::Restart
            ]
        );
    }

    #[test]
    fn a_node_state_change_with_no_restart_is_reported_as_a_status_change() {
        let mut source = CoordinatorSource::for_test();
        let id = DataflowId::from_u128(1);
        source.apply(Update::Dataflows(vec![summary(
            id,
            astrs_wire::DataflowStatus::Running,
        )]));
        source.apply(Update::NodeInfo {
            dataflow: id,
            nodes: vec![node_info(
                id,
                "camera",
                astrs_wire::NodeRunState::Spawning,
                0,
            )],
        });
        source.apply(Update::NodeInfo {
            dataflow: id,
            nodes: vec![node_info(
                id,
                "camera",
                astrs_wire::NodeRunState::Running,
                0,
            )],
        });

        let last = source.snapshot().timeline.last().unwrap();
        assert_eq!(last.category, TimelineCategory::Status);
        assert!(last.message.contains("spawning"));
        assert!(last.message.contains("running"));
    }

    #[test]
    fn metrics_are_preserved_across_a_later_node_info_update() {
        let mut source = CoordinatorSource::for_test();
        let id = DataflowId::from_u128(1);
        source.apply(Update::Dataflows(vec![summary(
            id,
            astrs_wire::DataflowStatus::Running,
        )]));
        source.apply(Update::NodeInfo {
            dataflow: id,
            nodes: vec![node_info(
                id,
                "camera",
                astrs_wire::NodeRunState::Running,
                0,
            )],
        });
        let mut sample = NodeMetricsSample::new(
            NodeId::new("camera").unwrap(),
            astrs_time::HlcTimestamp::EPOCH,
        );
        sample.cpu_percent = 5.0;
        source.apply(Update::Metrics {
            dataflow: id,
            samples: vec![sample],
        });
        assert!(
            source.snapshot().dataflow(id).unwrap().nodes[0]
                .metrics
                .is_some()
        );

        // A later `NodeInfo` refresh must not wipe out the metrics that
        // arrived from a separate `GetNodeMetrics` poll.
        source.apply(Update::NodeInfo {
            dataflow: id,
            nodes: vec![node_info(
                id,
                "camera",
                astrs_wire::NodeRunState::Running,
                1,
            )],
        });
        let row = source.snapshot().dataflow(id).unwrap();
        assert!(
            row.nodes[0].metrics.is_some(),
            "metrics must survive a NodeInfo refresh"
        );
        assert_eq!(row.nodes[0].metrics.as_ref().unwrap().cpu_percent, 5.0);
    }

    #[test]
    fn a_dropped_message_count_increase_is_reported_as_a_violation() {
        let mut source = CoordinatorSource::for_test();
        let id = DataflowId::from_u128(1);
        source.apply(Update::Dataflows(vec![summary(
            id,
            astrs_wire::DataflowStatus::Running,
        )]));
        source.apply(Update::NodeInfo {
            dataflow: id,
            nodes: vec![node_info(
                id,
                "camera",
                astrs_wire::NodeRunState::Running,
                0,
            )],
        });

        let mut first = NodeMetricsSample::new(
            NodeId::new("camera").unwrap(),
            astrs_time::HlcTimestamp::EPOCH,
        );
        first
            .dropped_total
            .insert(astrs_wire::DataId::new("frames").unwrap(), 2);
        source.apply(Update::Metrics {
            dataflow: id,
            samples: vec![first],
        });
        // No violation yet: this is the first sample, nothing to compare
        // against.
        assert!(
            !source
                .snapshot()
                .timeline
                .iter()
                .any(|event| event.category == TimelineCategory::Violation)
        );

        let mut second = NodeMetricsSample::new(
            NodeId::new("camera").unwrap(),
            astrs_time::HlcTimestamp::EPOCH,
        );
        second
            .dropped_total
            .insert(astrs_wire::DataId::new("frames").unwrap(), 9);
        source.apply(Update::Metrics {
            dataflow: id,
            samples: vec![second],
        });
        let violation = source
            .snapshot()
            .timeline
            .iter()
            .find(|event| event.category == TimelineCategory::Violation)
            .unwrap();
        assert!(violation.message.contains('7'), "{}", violation.message);
    }

    #[test]
    fn a_valid_manifest_populates_the_graph_and_an_invalid_one_does_not() {
        let mut source = CoordinatorSource::for_test();
        let id = DataflowId::from_u128(1);
        source.apply(Update::Dataflows(vec![summary(
            id,
            astrs_wire::DataflowStatus::Running,
        )]));
        source.apply(Update::Manifest {
            dataflow: id,
            yaml: "nodes:\n  - id: camera\n    path: ./camera\n".to_owned(),
        });
        assert!(source.snapshot().dataflow(id).unwrap().graph.is_some());

        let id2 = DataflowId::from_u128(2);
        source.apply(Update::Dataflows(vec![summary(
            id2,
            astrs_wire::DataflowStatus::Running,
        )]));
        source.apply(Update::Manifest {
            dataflow: id2,
            yaml: "not: [valid, manifest".to_owned(),
        });
        assert!(source.snapshot().dataflow(id2).unwrap().graph.is_none());
    }

    #[test]
    fn a_disconnect_update_marks_the_connection_degraded_and_stays_there() {
        let mut source = CoordinatorSource::for_test();
        source.apply(Update::Disconnected("socket closed".to_owned()));
        match &source.snapshot().connection {
            ConnectionStatus::Degraded { reason, .. } => assert_eq!(reason, "socket closed"),
            other => panic!("expected Degraded, got {other:?}"),
        }
    }

    #[test]
    fn refresh_reports_disconnected_once_the_channel_is_dropped() {
        let mut source = CoordinatorSource::for_test();
        // `for_test` already dropped its sender, so the very next
        // `refresh` must observe the channel as disconnected.
        let error = source.refresh().unwrap_err();
        assert!(matches!(error, ViewError::Disconnected(_)));
        assert!(matches!(
            source.snapshot().connection,
            ConnectionStatus::Degraded { .. }
        ));
    }

    #[test]
    fn every_error_variant_renders_a_non_empty_message() {
        let errors = [
            CoordinatorSourceError::Connect {
                endpoint: "127.0.0.1:7407".to_owned(),
                reason: "refused".to_owned(),
            },
            CoordinatorSourceError::Thread(std::io::Error::other("boom")),
        ];
        for error in errors {
            assert!(!error.to_string().is_empty());
        }
    }
}
