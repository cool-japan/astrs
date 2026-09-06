//! The owned, renderable state every [`crate::ClusterView`] refreshes and
//! every tab renderer reads: [`ClusterSnapshot`].
//!
//! Renderers never see a [`crate::ClusterView`] — see that trait's docs for
//! why — they see a `&ClusterSnapshot` plus a `&crate::app::UiState`. That
//! split is what makes every tab a pure function from data to text and
//! therefore testable with [`ratatui::backend::TestBackend`] and no
//! terminal, no coordinator and no clock.

use std::collections::{BTreeSet, VecDeque};

use astrs_graph::{DataflowGraph, EdgeKey};
use astrs_time::HlcTimestamp;
use astrs_wire::{
    DataflowId, DataflowSummary, LogRecord, NodeId, NodeInfo, NodeIoSample, NodeMetricsSample,
};

/// How many log lines a snapshot keeps before evicting the oldest.
///
/// A ring rather than an unbounded `Vec`: `astrs top` is meant to run for
/// the lifetime of a session, and a busy dataflow's log volume must not
/// turn a monitoring tool into the thing that exhausts memory on the
/// operator's own machine.
pub const MAX_LOG_LINES: usize = 500;

/// How many timeline events a snapshot keeps before evicting the oldest.
///
/// See [`MAX_LOG_LINES`] for the rationale; the same bound applies here.
pub const MAX_TIMELINE_EVENTS: usize = 500;

/// A dataflow's topology, parsed once from [`astrs_wire::ControlReply::Manifest`]
/// (`astrs top`'s Graph tab) and cached rather than re-fetched on every tick.
///
/// # Why the cross-machine set is precomputed
///
/// The Graph tab needs, for every edge, "is this same-host (planned `shm`)
/// or cross-host (planned `net`)?" [`astrs_graph::plan_placement`] answers
/// that as a `Vec<CrossMachineRoute>`; a linear membership test against
/// that `Vec` for every edge on every redraw is $O(E^2)$ for no reason, so
/// [`GraphInfo::new`] collapses it into a [`BTreeSet<EdgeKey>`] once, at
/// fetch time, giving [`GraphInfo::plane_of`] an $O(\log E)$ lookup instead.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphInfo {
    /// The parsed, validated topology.
    pub graph: DataflowGraph,
    /// Every edge [`astrs_graph::plan_placement`] classified as crossing a
    /// machine boundary — the honest "planned `net`" set; everything else
    /// is planned `shm` (or has no plane at all, for a virtual source).
    cross_machine: BTreeSet<EdgeKey>,
}

/// The plane badge the Graph tab prints beside one edge.
///
/// Blueprint §6.2/§6.4 describe the plane an edge actually negotiates at
/// runtime (`Uds → Shm` same host after the slow-start handshake, `Tcp`/
/// `Quic` cross host); nothing on the wire today lets a client read that
/// *live* state back (§13's metrics poll covers resource/queue samples,
/// not route state). What [`GraphInfo::plane_of`] reports instead is the
/// **planned** substrate implied by [`astrs_graph::plan_placement`] alone —
/// same machine or different — which is honest, always available offline
/// or online, and labelled as a plan rather than a live fact everywhere it
/// is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PlaneBadge {
    /// Producer and consumer resolve to the same machine: same-host, so the
    /// SHM plane is the plan (§6.2).
    Shm,
    /// Producer and consumer resolve to different machines: a cross-host
    /// route is the plan (§6.4). Whether that route ends up on QUIC or its
    /// TCP fallback is a live negotiation this crate cannot see, so the
    /// badge says only "net", never a specific transport.
    Net,
    /// The edge has no producer node at all — an `astrs/timer/*` or
    /// `astrs/logs/*` virtual source (§8.4), materialized locally on the
    /// consumer's own daemon and never routed anywhere.
    Virtual,
}

impl PlaneBadge {
    /// The short badge text the Graph tab prints, e.g. `[shm]`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Shm => "shm",
            Self::Net => "net",
            Self::Virtual => "timer",
        }
    }
}

impl GraphInfo {
    /// Builds the cached view of `graph`: parses nothing further, just
    /// precomputes the cross-machine edge set via
    /// [`astrs_graph::plan_placement`].
    #[must_use]
    pub fn new(graph: DataflowGraph) -> Self {
        let placement = astrs_graph::plan_placement(&graph);
        let cross_machine = placement
            .cross_machine_routes
            .into_iter()
            .map(|route| route.edge)
            .collect();
        Self {
            graph,
            cross_machine,
        }
    }

    /// The planned plane for `key`, or [`PlaneBadge::Virtual`] if the edge
    /// has no producer node. Returns `None` if `key` does not name an edge
    /// of [`GraphInfo::graph`].
    #[must_use]
    pub fn plane_of(&self, key: &EdgeKey) -> Option<PlaneBadge> {
        let edge = self.graph.edge(key)?;
        if edge.from.is_virtual() {
            return Some(PlaneBadge::Virtual);
        }
        Some(if self.cross_machine.contains(key) {
            PlaneBadge::Net
        } else {
            PlaneBadge::Shm
        })
    }
}

/// One node's runtime row on the Dataflows tab: its lifecycle state plus
/// its latest resource/queue sample, when the coordinator has one.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeRow {
    /// Identity, lifecycle state, restart count, ports (§12).
    pub info: NodeInfo,
    /// CPU/RSS/queue-depth, when [`astrs_wire::ControlRequest::GetNodeMetrics`]
    /// has answered for this node at least once (§13). A replay or a
    /// freshly connected session may have none yet.
    pub metrics: Option<NodeMetricsSample>,
    /// The newest bandwidth reading, from
    /// [`astrs_wire::ControlRequest::GetNodeIoMetrics`] (§6.2, §13).
    pub io: Option<NodeIoSample>,
    /// The reading before it.
    ///
    /// Kept because every total on the wire is cumulative and a *rate* needs
    /// two of them: `astrs top`'s throughput column is
    /// `(newer - older) / (newer.timestamp - older.timestamp)`, computed by
    /// [`astrs_wire::NodeIoSample::delta_since`]. Storing the pair here rather
    /// than a pre-divided rate keeps the arithmetic in one place and keeps the
    /// view honest when a poll is late or dropped — the denominator is the
    /// samples' own clock, never the polling cadence.
    pub previous_io: Option<NodeIoSample>,
}

impl NodeRow {
    /// A row for `info` with no samples attached yet — a node the coordinator
    /// has told us about but not yet measured.
    #[must_use]
    pub const fn new(info: NodeInfo) -> Self {
        Self {
            info,
            metrics: None,
            io: None,
            previous_io: None,
        }
    }

    /// The same row carrying a resource/queue sample.
    #[must_use]
    pub fn with_metrics(mut self, sample: NodeMetricsSample) -> Self {
        self.metrics = Some(sample);
        self
    }

    /// The same row carrying a bandwidth sample, keeping whatever reading was
    /// there as the baseline the next rate is measured against.
    #[must_use]
    pub fn with_io(mut self, sample: NodeIoSample) -> Self {
        self.previous_io = self.io.replace(sample);
        self
    }

    /// The node's current throughput, or `None` until two readings exist.
    #[must_use]
    pub fn throughput(&self) -> Option<astrs_wire::IoDelta> {
        let newer = self.io.as_ref()?;
        let older = self.previous_io.as_ref()?;
        newer.delta_since(older)
    }
}

/// One dataflow's row: its summary, its nodes, and its cached topology.
#[derive(Debug, Clone, PartialEq)]
pub struct DataflowRow {
    /// The list-level summary (status, node/running counts, daemons).
    pub summary: DataflowSummary,
    /// One row per node the coordinator has told us about, in [`NodeId`]
    /// order.
    pub nodes: Vec<NodeRow>,
    /// The parsed topology, once fetched — see [`GraphInfo`]'s docs for why
    /// this is cached rather than re-fetched every tick.
    pub graph: Option<GraphInfo>,
}

impl DataflowRow {
    /// A row with no nodes and no topology yet — the shape a freshly seen
    /// dataflow id starts in before its detail arrives.
    #[must_use]
    pub fn new(summary: DataflowSummary) -> Self {
        Self {
            summary,
            nodes: Vec::new(),
            graph: None,
        }
    }

    /// Looks up one node's row by id.
    #[must_use]
    pub fn node(&self, id: &NodeId) -> Option<&NodeRow> {
        self.nodes.iter().find(|row| &row.info.node == id)
    }
}

/// How a [`crate::ClusterView`] is currently reaching its data.
///
/// Rendered as the status line every tab shares (blueprint: the TUI must
/// stay responsive and informative through a coordinator dropout, not just
/// freeze on stale data).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConnectionStatus {
    /// The first connection attempt has not resolved yet.
    Connecting,
    /// Connected and refreshing normally.
    Live {
        /// The endpoint dialled, as typed or resolved (matches
        /// `astrs-cli`'s own [`Endpoint::display`](https://docs.rs/astrs-cli)
        /// convention).
        endpoint: String,
    },
    /// Connected once, but the last [`crate::ClusterView::refresh`] failed —
    /// the snapshot shown is the last one that succeeded, not necessarily
    /// current.
    Degraded {
        /// The endpoint dialled.
        endpoint: String,
        /// Why the last refresh failed, for the status line.
        reason: String,
    },
    /// Rendering a recording rather than a live cluster (`astrs top
    /// --replay`).
    Replay {
        /// The `.arec` file being played.
        path: String,
        /// How many entries have been delivered so far.
        position: usize,
        /// The total entry count, when the source knows it up front.
        total: Option<usize>,
    },
}

/// One row of the Timeline tab: an HLC-ordered lifecycle event.
///
/// A flattened struct rather than a nested enum-per-kind on purpose: every
/// tab, golden test and category filter wants "when, who, what kind, what
/// text" — nothing here needs to pattern-match a payload back out, so there
/// is no payload to keep re-deriving as the event vocabulary grows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineEvent {
    /// When the underlying change happened, on the producer's HLC where one
    /// is known, or the viewer's own HLC when the event is synthesized from
    /// a diff (see `view::coordinator` for exactly which is which).
    pub timestamp: HlcTimestamp,
    /// The dataflow this event is about, when it is about one.
    pub dataflow: Option<DataflowId>,
    /// The node this event is about, when it is about one.
    pub node: Option<NodeId>,
    /// The event's category, for the badge/color the Timeline tab renders.
    pub category: TimelineCategory,
    /// The human-readable line.
    pub message: String,
}

impl TimelineEvent {
    /// Builds an event.
    #[must_use]
    pub fn new(
        timestamp: HlcTimestamp,
        category: TimelineCategory,
        message: impl Into<String>,
    ) -> Self {
        Self {
            timestamp,
            dataflow: None,
            node: None,
            category,
            message: message.into(),
        }
    }

    /// Attaches a dataflow id.
    #[must_use]
    pub fn with_dataflow(mut self, dataflow: DataflowId) -> Self {
        self.dataflow = Some(dataflow);
        self
    }

    /// Attaches a node id.
    #[must_use]
    pub fn with_node(mut self, node: NodeId) -> Self {
        self.node = Some(node);
        self
    }
}

/// What kind of change a [`TimelineEvent`] reports — drives the badge/color
/// the Timeline tab renders it with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum TimelineCategory {
    /// A node was spawned, or a dataflow started.
    Spawn,
    /// A node was restarted under its restart policy (§12).
    Restart,
    /// A dataflow or node's lifecycle state changed in some other way
    /// (finished, stopping, and the like).
    Status,
    /// A queue drop, a deadline miss, or another condition an operator
    /// should notice (§11.2, §11.3).
    Violation,
    /// A message was replayed from a recording (`view::replay`).
    Replay,
}

impl TimelineCategory {
    /// Every category, in a fixed, testable order.
    pub const ALL: &'static [Self] = &[
        Self::Spawn,
        Self::Restart,
        Self::Status,
        Self::Violation,
        Self::Replay,
    ];

    /// A short, stable label for the Timeline tab's badge column.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Restart => "restart",
            Self::Status => "status",
            Self::Violation => "violation",
            Self::Replay => "replay",
        }
    }
}

/// The complete, owned state a [`crate::ClusterView`] hands to every
/// renderer.
///
/// # Why owned, not borrowed from behind a lock
///
/// A live [`crate::ClusterView`] implementation typically holds its
/// freshest data behind a background task and a mutex or a channel; a
/// renderer that borrowed straight through that guard would tie the guard's
/// lifetime to every `Frame` in the render closure, which `ratatui`'s
/// `Terminal::draw` does not give a name to. Owning the snapshot sidesteps
/// the problem entirely: [`crate::ClusterView::refresh`] drains whatever is
/// newly available into this struct, and [`crate::ClusterView::snapshot`]
/// hands back a plain reference to data that already belongs to the view.
///
/// # Examples
///
/// ```
/// use astrs_time::HlcTimestamp;
/// use astrs_tui::ClusterSnapshot;
/// use astrs_wire::{LogLevel, LogRecord};
///
/// let mut snapshot = ClusterSnapshot::empty();
/// assert!(snapshot.dataflows.is_empty());
///
/// snapshot.push_log(LogRecord::new(
///     HlcTimestamp::new(1, 0),
///     LogLevel::Info,
///     "camera up",
/// ));
/// assert_eq!(snapshot.logs.len(), 1);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterSnapshot {
    /// Every dataflow known so far, in the order the last `List`/replay
    /// header reported them.
    pub dataflows: Vec<DataflowRow>,
    /// Recent log records, oldest first, capped at [`MAX_LOG_LINES`].
    pub logs: VecDeque<LogRecord>,
    /// Recent lifecycle events, sorted ascending by
    /// [`TimelineEvent::timestamp`] and capped at [`MAX_TIMELINE_EVENTS`].
    pub timeline: Vec<TimelineEvent>,
    /// How the data is currently being obtained.
    pub connection: ConnectionStatus,
}

impl ClusterSnapshot {
    /// An empty snapshot, not yet connected to anything.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            dataflows: Vec::new(),
            logs: VecDeque::new(),
            timeline: Vec::new(),
            connection: ConnectionStatus::Connecting,
        }
    }

    /// Looks up one dataflow's row by id.
    #[must_use]
    pub fn dataflow(&self, id: DataflowId) -> Option<&DataflowRow> {
        self.dataflows.iter().find(|row| row.summary.id == id)
    }

    /// Looks up one dataflow's row mutably by id.
    pub fn dataflow_mut(&mut self, id: DataflowId) -> Option<&mut DataflowRow> {
        self.dataflows.iter_mut().find(|row| row.summary.id == id)
    }

    /// Appends a log record, evicting the oldest once [`MAX_LOG_LINES`] is
    /// exceeded.
    pub fn push_log(&mut self, record: LogRecord) {
        self.logs.push_back(record);
        while self.logs.len() > MAX_LOG_LINES {
            self.logs.pop_front();
        }
    }

    /// Inserts a timeline event at its sorted position, evicting the
    /// oldest once [`MAX_TIMELINE_EVENTS`] is exceeded.
    ///
    /// Insertion rather than an unconditional push: events are HLC-ordered
    /// (blueprint §13), but their two sources — a live diff against the
    /// previous poll, and a recording's own entry order — do not always
    /// arrive already sorted relative to each other (a fast node's second
    /// restart can be observed in the same tick as a slow node's first
    /// spawn, in either order). A bounded `Vec` with a binary-search
    /// insert keeps the tab's ordering guarantee exact at a cost that is
    /// trivial at [`MAX_TIMELINE_EVENTS`]'s scale.
    pub fn push_timeline(&mut self, event: TimelineEvent) {
        let index = self
            .timeline
            .partition_point(|existing| existing.timestamp <= event.timestamp);
        self.timeline.insert(index, event);
        if self.timeline.len() > MAX_TIMELINE_EVENTS {
            self.timeline.remove(0);
        }
    }
}

impl Default for ClusterSnapshot {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_graph::{NodeId as GraphNodeId, PortName};
    use astrs_manifest::{Input, Manifest, Node};
    use astrs_wire::LogLevel;

    use super::*;

    fn two_node_graph() -> DataflowGraph {
        let mut camera = Node::with_path("camera", "./camera");
        camera.outputs = vec!["frames".to_owned()];
        let mut detector = Node::with_path("detector", "./detector");
        detector
            .inputs
            .insert("frames".to_owned(), Input::from_source("camera/frames"));
        let manifest = Manifest {
            nodes: vec![camera, detector],
            ..Manifest::default()
        };
        DataflowGraph::from_manifest(&manifest).unwrap().0
    }

    #[test]
    fn graph_info_labels_a_same_host_edge_as_shm() {
        let info = GraphInfo::new(two_node_graph());
        let key = EdgeKey::new(GraphNodeId::new("detector"), PortName::new("frames"));
        assert_eq!(info.plane_of(&key), Some(PlaneBadge::Shm));
        assert_eq!(PlaneBadge::Shm.label(), "shm");
    }

    #[test]
    fn graph_info_labels_a_virtual_source_edge_distinctly() {
        let mut node = Node::with_path("n", "./n");
        node.inputs
            .insert("tick".to_owned(), Input::from_source("astrs/timer/hz/50"));
        let manifest = Manifest {
            nodes: vec![node],
            ..Manifest::default()
        };
        let (graph, _) = DataflowGraph::from_manifest(&manifest).unwrap();
        let info = GraphInfo::new(graph);
        let key = EdgeKey::new(GraphNodeId::new("n"), PortName::new("tick"));
        assert_eq!(info.plane_of(&key), Some(PlaneBadge::Virtual));
    }

    #[test]
    fn graph_info_reports_none_for_an_unknown_edge() {
        let info = GraphInfo::new(two_node_graph());
        let key = EdgeKey::new(GraphNodeId::new("nope"), PortName::new("nope"));
        assert_eq!(info.plane_of(&key), None);
    }

    #[test]
    fn a_cross_machine_edge_is_labelled_net() {
        use astrs_manifest::Deploy;

        let mut camera = Node::with_path("camera", "./camera");
        camera.outputs = vec!["frames".to_owned()];
        let mut detector = Node::with_path("detector", "./detector");
        detector.deploy = Some(Deploy {
            machine: Some("robot-1".to_owned()),
            ..Deploy::default()
        });
        detector
            .inputs
            .insert("frames".to_owned(), Input::from_source("camera/frames"));
        let manifest = Manifest {
            nodes: vec![camera, detector],
            ..Manifest::default()
        };
        let (graph, _) = DataflowGraph::from_manifest(&manifest).unwrap();
        let info = GraphInfo::new(graph);
        let key = EdgeKey::new(GraphNodeId::new("detector"), PortName::new("frames"));
        assert_eq!(info.plane_of(&key), Some(PlaneBadge::Net));
        assert_eq!(PlaneBadge::Net.label(), "net");
    }

    #[test]
    fn dataflow_row_finds_a_node_by_id() {
        let summary = DataflowSummary::pending(DataflowId::from_u128(1), 1);
        let mut row = DataflowRow::new(summary);
        row.nodes.push(NodeRow::new(sample_node_info()));
        assert!(row.node(&NodeId::new("camera").unwrap()).is_some());
        assert!(row.node(&NodeId::new("nope").unwrap()).is_none());
    }

    fn sample_node_info() -> NodeInfo {
        NodeInfo {
            dataflow: DataflowId::from_u128(1),
            node: NodeId::new("camera").unwrap(),
            daemon: astrs_wire::DaemonId::generate(None),
            state: astrs_wire::NodeRunState::Running,
            pid: Some(1),
            generation: 1,
            restart_count: 0,
            inputs: std::collections::BTreeMap::new(),
            outputs: std::collections::BTreeMap::new(),
            started_at: None,
            exit_cause: None,
        }
    }

    #[test]
    fn snapshot_log_ring_evicts_the_oldest_past_the_cap() {
        let mut snapshot = ClusterSnapshot::empty();
        for index in 0..(MAX_LOG_LINES + 10) {
            snapshot.push_log(LogRecord::new(
                HlcTimestamp::new(index as u64, 0),
                LogLevel::Info,
                format!("line {index}"),
            ));
        }
        assert_eq!(snapshot.logs.len(), MAX_LOG_LINES);
        assert_eq!(snapshot.logs.front().unwrap().message, "line 10");
        assert_eq!(
            snapshot.logs.back().unwrap().message,
            format!("line {}", MAX_LOG_LINES + 9)
        );
    }

    #[test]
    fn snapshot_timeline_stays_sorted_even_when_pushed_out_of_order() {
        let mut snapshot = ClusterSnapshot::empty();
        snapshot.push_timeline(TimelineEvent::new(
            HlcTimestamp::new(10, 0),
            TimelineCategory::Spawn,
            "later",
        ));
        snapshot.push_timeline(TimelineEvent::new(
            HlcTimestamp::new(5, 0),
            TimelineCategory::Spawn,
            "earlier",
        ));
        assert_eq!(snapshot.timeline.len(), 2);
        assert_eq!(snapshot.timeline[0].message, "earlier");
        assert_eq!(snapshot.timeline[1].message, "later");
    }

    #[test]
    fn snapshot_timeline_evicts_the_oldest_past_the_cap() {
        let mut snapshot = ClusterSnapshot::empty();
        for index in 0..(MAX_TIMELINE_EVENTS + 5) {
            snapshot.push_timeline(TimelineEvent::new(
                HlcTimestamp::new(index as u64, 0),
                TimelineCategory::Status,
                format!("event {index}"),
            ));
        }
        assert_eq!(snapshot.timeline.len(), MAX_TIMELINE_EVENTS);
        assert_eq!(snapshot.timeline[0].message, "event 5");
    }

    #[test]
    fn snapshot_finds_and_updates_a_dataflow_by_id() {
        let mut snapshot = ClusterSnapshot::empty();
        let id = DataflowId::from_u128(1);
        snapshot
            .dataflows
            .push(DataflowRow::new(DataflowSummary::pending(id, 0)));
        assert!(snapshot.dataflow(id).is_some());
        assert!(snapshot.dataflow(DataflowId::from_u128(2)).is_none());
        snapshot.dataflow_mut(id).unwrap().summary.name = Some("demo".to_owned());
        assert_eq!(
            snapshot.dataflow(id).unwrap().summary.name.as_deref(),
            Some("demo")
        );
    }

    #[test]
    fn timeline_categories_have_stable_labels() {
        let mut labels = std::collections::BTreeSet::new();
        for category in TimelineCategory::ALL {
            assert!(labels.insert(category.label()));
        }
        assert_eq!(labels.len(), TimelineCategory::ALL.len());
    }

    #[test]
    fn a_default_snapshot_is_empty_and_connecting() {
        let snapshot = ClusterSnapshot::default();
        assert!(snapshot.dataflows.is_empty());
        assert!(snapshot.logs.is_empty());
        assert!(snapshot.timeline.is_empty());
        assert_eq!(snapshot.connection, ConnectionStatus::Connecting);
    }
}
