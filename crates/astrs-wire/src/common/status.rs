//! Lifecycle states, typed exit causes and the summaries built from them.
//!
//! Blueprint §12: *"the dataflow FSM aggregates per-node exit causes into a
//! `DataflowResult` with typed causes (not strings)"*. That parenthetical is
//! the whole design here. A string exit reason cannot be matched on, cannot be
//! counted in a metric, and cannot be tested for; [`NodeExitCause`] can.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{DataflowStatus, NodeExitCause};
//!
//! let clean = NodeExitCause::Success;
//! assert!(clean.is_success());
//!
//! let crashed = NodeExitCause::Signal { signal: 11, name: "SIGSEGV".into() };
//! assert!(!crashed.is_success());
//! assert!(crashed.is_failure());
//!
//! assert!(DataflowStatus::Finished.is_terminal());
//! assert!(DataflowStatus::Running.is_active());
//! ```

use core::fmt;
use std::collections::BTreeMap;

use astrs_time::HlcTimestamp;
use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::common::duration::DurationMs;
use crate::ids::{DaemonId, DataId, DataflowId, NodeId, TypeUrn};

/// The lifecycle state of a whole dataflow.
///
/// # Examples
///
/// ```
/// use astrs_wire::DataflowStatus;
///
/// assert!(DataflowStatus::Failed.is_terminal());
/// assert!(!DataflowStatus::Stopping.is_terminal());
/// assert_eq!(DataflowStatus::Building.as_str(), "building");
/// ```
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Encode,
    Decode,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DataflowStatus {
    /// Known to the coordinator but not started. The default.
    #[default]
    #[oxicode(variant = 0)]
    Pending,
    /// A build is running.
    #[oxicode(variant = 1)]
    Building,
    /// Built and ready to start.
    #[oxicode(variant = 2)]
    Ready,
    /// Nodes are being spawned.
    #[oxicode(variant = 3)]
    Starting,
    /// All nodes are spawned and running.
    #[oxicode(variant = 4)]
    Running,
    /// A stop is in progress.
    #[oxicode(variant = 5)]
    Stopping,
    /// Every node exited cleanly.
    #[oxicode(variant = 6)]
    Finished,
    /// At least one node failed, or the dataflow could not start.
    #[oxicode(variant = 7)]
    Failed,
}

impl DataflowStatus {
    /// Every status, in variant order.
    pub const ALL: &'static [Self] = &[
        Self::Pending,
        Self::Building,
        Self::Ready,
        Self::Starting,
        Self::Running,
        Self::Stopping,
        Self::Finished,
        Self::Failed,
    ];

    /// A stable, lower-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Building => "building",
            Self::Ready => "ready",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Finished => "finished",
            Self::Failed => "failed",
        }
    }

    /// Whether the dataflow has reached a final state.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Finished | Self::Failed)
    }

    /// Whether nodes are (or are about to be) running.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }
}

impl fmt::Display for DataflowStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The lifecycle state of one node.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Encode,
    Decode,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NodeRunState {
    /// Scheduled but not yet spawned. The default.
    #[default]
    #[oxicode(variant = 0)]
    Pending,
    /// The process has been spawned but has not registered yet.
    #[oxicode(variant = 1)]
    Spawning,
    /// Registered and receiving events.
    #[oxicode(variant = 2)]
    Running,
    /// Exited and awaiting a respawn under its restart policy.
    #[oxicode(variant = 3)]
    Restarting,
    /// Being asked to stop.
    #[oxicode(variant = 4)]
    Stopping,
    /// Exited cleanly.
    #[oxicode(variant = 5)]
    Finished,
    /// Exited abnormally, with no respawn pending.
    #[oxicode(variant = 6)]
    Failed,
}

impl NodeRunState {
    /// Every state, in variant order.
    pub const ALL: &'static [Self] = &[
        Self::Pending,
        Self::Spawning,
        Self::Running,
        Self::Restarting,
        Self::Stopping,
        Self::Finished,
        Self::Failed,
    ];

    /// A stable, lower-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Spawning => "spawning",
            Self::Running => "running",
            Self::Restarting => "restarting",
            Self::Stopping => "stopping",
            Self::Finished => "finished",
            Self::Failed => "failed",
        }
    }

    /// Whether the node has reached a final state.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Finished | Self::Failed)
    }
}

impl fmt::Display for NodeRunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a node stopped running — the typed cause of blueprint §12.
///
/// # Examples
///
/// ```
/// use astrs_wire::NodeExitCause;
///
/// assert!(NodeExitCause::Success.is_success());
/// assert!(NodeExitCause::ExitCode { code: 0 }.is_success());
/// assert!(NodeExitCause::ExitCode { code: 1 }.is_failure());
/// assert!(!NodeExitCause::Cancelled.is_failure());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NodeExitCause {
    /// The node returned successfully.
    #[oxicode(variant = 0)]
    Success,
    /// The process exited with this status code.
    #[oxicode(variant = 1)]
    ExitCode {
        /// The status code; zero is still a success.
        code: i32,
    },
    /// The process was terminated by a signal.
    #[oxicode(variant = 2)]
    Signal {
        /// The signal number.
        signal: i32,
        /// Its conventional name, e.g. `SIGSEGV`.
        name: String,
    },
    /// The node panicked.
    #[oxicode(variant = 3)]
    Panic {
        /// The panic message, as captured.
        message: String,
    },
    /// The process could not be spawned at all.
    #[oxicode(variant = 4)]
    SpawnFailed {
        /// Why the spawn failed.
        message: String,
    },
    /// The node was spawned but never registered in time (§12: dora's gap,
    /// closed).
    #[oxicode(variant = 5)]
    SpawnDeadlineExceeded {
        /// How long the daemon waited.
        after: DurationMs,
    },
    /// A registered node stopped answering liveness pings.
    #[oxicode(variant = 6)]
    HealthCheckTimeout {
        /// How long the daemon waited.
        after: DurationMs,
    },
    /// The daemon killed the node — typically the finish-straggler watchdog
    /// escalating past `finish_grace_secs`.
    #[oxicode(variant = 7)]
    Killed {
        /// Why the daemon escalated.
        reason: String,
    },
    /// The daemon shut down beneath the node.
    #[oxicode(variant = 8)]
    DaemonShutdown,
    /// The node kept failing and used up its restart budget.
    #[oxicode(variant = 9)]
    RestartBudgetExhausted {
        /// How many restarts were attempted.
        restarts: u32,
        /// The window they were counted over.
        window: DurationMs,
    },
    /// A stop was requested by an operator or by the dataflow FSM.
    #[oxicode(variant = 10)]
    Cancelled,
    /// The daemon hosting the node became unreachable (§12: peer partition).
    #[oxicode(variant = 11)]
    DaemonUnreachable {
        /// The daemon that dropped out.
        daemon: DaemonId,
    },
}

impl NodeExitCause {
    /// Whether this counts as a clean exit.
    ///
    /// [`NodeExitCause::ExitCode`] with a zero code is a success, which is
    /// why this is a method and not a `matches!` at every call site.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        match self {
            Self::Success => true,
            Self::ExitCode { code } => *code == 0,
            _ => false,
        }
    }

    /// Whether this counts as a failure.
    ///
    /// A deliberate cancellation is neither a success nor a failure: it is
    /// what the operator asked for, and counting it as a failure would make
    /// every `astrs stop` look like an incident.
    #[must_use]
    pub const fn is_failure(&self) -> bool {
        !self.is_success() && !matches!(self, Self::Cancelled | Self::DaemonShutdown)
    }

    /// A stable, lower-case name for metrics labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::ExitCode { .. } => "exit_code",
            Self::Signal { .. } => "signal",
            Self::Panic { .. } => "panic",
            Self::SpawnFailed { .. } => "spawn_failed",
            Self::SpawnDeadlineExceeded { .. } => "spawn_deadline_exceeded",
            Self::HealthCheckTimeout { .. } => "health_check_timeout",
            Self::Killed { .. } => "killed",
            Self::DaemonShutdown => "daemon_shutdown",
            Self::RestartBudgetExhausted { .. } => "restart_budget_exhausted",
            Self::Cancelled => "cancelled",
            Self::DaemonUnreachable { .. } => "daemon_unreachable",
        }
    }
}

impl fmt::Display for NodeExitCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success => f.write_str("exited successfully"),
            Self::ExitCode { code } => write!(f, "exited with code {code}"),
            Self::Signal { signal, name } => write!(f, "killed by signal {signal} ({name})"),
            Self::Panic { message } => write!(f, "panicked: {message}"),
            Self::SpawnFailed { message } => write!(f, "spawn failed: {message}"),
            Self::SpawnDeadlineExceeded { after } => {
                write!(f, "did not register within {after}")
            }
            Self::HealthCheckTimeout { after } => write!(f, "health check timed out after {after}"),
            Self::Killed { reason } => write!(f, "killed by the daemon: {reason}"),
            Self::DaemonShutdown => f.write_str("daemon shut down"),
            Self::RestartBudgetExhausted { restarts, window } => {
                write!(f, "restarted {restarts} times within {window}")
            }
            Self::Cancelled => f.write_str("stop requested"),
            Self::DaemonUnreachable { daemon } => write!(f, "daemon {daemon} became unreachable"),
        }
    }
}

/// Why a node is being asked to stop — the payload of
/// [`crate::NodeEvent::Stop`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StopCause {
    /// An operator asked for it (`astrs stop`).
    #[oxicode(variant = 0)]
    Requested,
    /// The dataflow finished — every other node exited
    /// (`exit_when_nodes_finish`).
    #[oxicode(variant = 1)]
    DataflowFinished,
    /// Every input this node had is closed and no more will open.
    #[oxicode(variant = 2)]
    AllInputsClosed,
    /// The daemon is going away.
    #[oxicode(variant = 3)]
    DaemonShutdown,
    /// A peer node failed and the dataflow is tearing down.
    #[oxicode(variant = 4)]
    PeerFailed {
        /// The node that failed.
        node: NodeId,
    },
    /// The dataflow is being destroyed.
    #[oxicode(variant = 5)]
    Destroyed,
    /// An internal error forced the stop.
    #[oxicode(variant = 6)]
    Error {
        /// What went wrong.
        message: String,
    },
    /// A dynamic-topology `astrs node replace` (blueprint §8, §17)
    /// superseded this incarnation with a new generation — a tail append
    /// beyond the original seven variants.
    ///
    /// The outgoing incarnation is asked to stop cooperatively *after* the
    /// replacement has already been dispatched to spawn, which is what
    /// creates the brief dual-run window
    /// [`crate::CoordinatorEvent::ReplaceNode`]'s docs describe; this cause
    /// is what tells the outgoing process (and anything reading its logs)
    /// that its exit is an ordinary handover, not a crash.
    #[oxicode(variant = 7)]
    Replaced,
}

impl StopCause {
    /// Whether the stop is a normal end of work rather than a fault.
    #[must_use]
    pub const fn is_graceful(&self) -> bool {
        matches!(
            self,
            Self::Requested
                | Self::DataflowFinished
                | Self::AllInputsClosed
                | Self::Destroyed
                | Self::Replaced
        )
    }
}

impl fmt::Display for StopCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Requested => f.write_str("stop requested"),
            Self::DataflowFinished => f.write_str("dataflow finished"),
            Self::AllInputsClosed => f.write_str("all inputs closed"),
            Self::DaemonShutdown => f.write_str("daemon shutting down"),
            Self::PeerFailed { node } => write!(f, "peer node {node} failed"),
            Self::Destroyed => f.write_str("dataflow destroyed"),
            Self::Error { message } => write!(f, "error: {message}"),
            Self::Replaced => f.write_str("superseded by a replacement"),
        }
    }
}

/// The outcome of one dataflow run.
///
/// # Examples
///
/// ```
/// use astrs_wire::{DataflowId, DataflowResult, DataflowStatus, NodeExitCause, NodeId};
/// use astrs_time::HlcTimestamp;
///
/// let mut result = DataflowResult::new(DataflowId::from_u128(1), HlcTimestamp::new(1, 0));
/// result.record(NodeId::new("camera")?, NodeExitCause::Success);
/// result.record(NodeId::new("detector")?, NodeExitCause::ExitCode { code: 3 });
/// result.finish(HlcTimestamp::new(2, 0));
///
/// assert_eq!(result.status, DataflowStatus::Failed);
/// assert_eq!(result.failed_nodes().count(), 1);
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct DataflowResult {
    /// Which dataflow this describes.
    pub dataflow: DataflowId,
    /// The final status.
    pub status: DataflowStatus,
    /// Per-node exit causes, in node-id order.
    pub node_results: BTreeMap<NodeId, NodeExitCause>,
    /// When the run started.
    pub started_at: HlcTimestamp,
    /// When the run ended; `None` while it is still going.
    pub finished_at: Option<HlcTimestamp>,
    /// A human summary, for `astrs list` and error reports.
    pub message: String,
}

impl DataflowResult {
    /// A result for a run that has just started.
    #[must_use]
    pub fn new(dataflow: DataflowId, started_at: HlcTimestamp) -> Self {
        Self {
            dataflow,
            status: DataflowStatus::Running,
            node_results: BTreeMap::new(),
            started_at,
            finished_at: None,
            message: String::new(),
        }
    }

    /// Records one node's exit cause.
    pub fn record(&mut self, node: NodeId, cause: NodeExitCause) {
        self.node_results.insert(node, cause);
    }

    /// Closes the run, deriving the final status from the recorded causes.
    ///
    /// The status is [`DataflowStatus::Failed`] if any node failed, and
    /// [`DataflowStatus::Finished`] otherwise. Cancelled nodes do not make a
    /// run a failure — see [`NodeExitCause::is_failure`].
    pub fn finish(&mut self, finished_at: HlcTimestamp) {
        self.finished_at = Some(finished_at);
        self.status = if self.has_failures() {
            DataflowStatus::Failed
        } else {
            DataflowStatus::Finished
        };
    }

    /// Whether any node failed.
    #[must_use]
    pub fn has_failures(&self) -> bool {
        self.node_results.values().any(NodeExitCause::is_failure)
    }

    /// The nodes that failed, with their causes.
    pub fn failed_nodes(&self) -> impl Iterator<Item = (&NodeId, &NodeExitCause)> {
        self.node_results
            .iter()
            .filter(|(_, cause)| cause.is_failure())
    }

    /// How long the run lasted, if it has finished.
    #[must_use]
    pub fn duration(&self) -> Option<DurationMs> {
        let finished = self.finished_at?;
        finished
            .physical_duration_since(&self.started_at)
            .map(DurationMs::from_duration)
    }
}

/// One row of `astrs list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct DataflowSummary {
    /// The dataflow's id.
    pub id: DataflowId,
    /// Its manifest `name:`, if it has one.
    pub name: Option<String>,
    /// Its current status.
    pub status: DataflowStatus,
    /// The daemons hosting its nodes.
    pub daemons: Vec<DaemonId>,
    /// How many nodes it declares.
    pub node_count: u32,
    /// How many of those are currently running.
    pub running_nodes: u32,
    /// When it started, if it has.
    pub started_at: Option<HlcTimestamp>,
}

impl DataflowSummary {
    /// A summary for a dataflow that is known but not started.
    #[must_use]
    pub fn pending(id: DataflowId, node_count: u32) -> Self {
        Self {
            id,
            name: None,
            status: DataflowStatus::Pending,
            daemons: Vec::new(),
            node_count,
            running_nodes: 0,
            started_at: None,
        }
    }

    /// The display name: the manifest name if set, else the id.
    #[must_use]
    pub fn display_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.id.to_string())
    }
}

/// The detail behind `astrs info <node>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct NodeInfo {
    /// The dataflow the node belongs to.
    pub dataflow: DataflowId,
    /// The node's id.
    pub node: NodeId,
    /// The daemon supervising it.
    pub daemon: DaemonId,
    /// Its current state.
    pub state: NodeRunState,
    /// The OS process id, when it has one.
    pub pid: Option<u32>,
    /// The current incarnation counter.
    pub generation: u64,
    /// How many times it has been restarted.
    pub restart_count: u32,
    /// Its input ports and their declared types.
    pub inputs: BTreeMap<DataId, Option<TypeUrn>>,
    /// Its output ports and their declared types.
    pub outputs: BTreeMap<DataId, Option<TypeUrn>>,
    /// When it started, if it has.
    pub started_at: Option<HlcTimestamp>,
    /// Why it stopped, if it has.
    pub exit_cause: Option<NodeExitCause>,
}

/// One row of `astrs list --daemons` / `ConnectedDaemons`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct DaemonInfo {
    /// The daemon's identity.
    pub id: DaemonId,
    /// The AstRS build it runs.
    pub version: crate::version::AstrsVersion,
    /// The address it can be reached at, as a display string.
    pub address: String,
    /// When it connected to the coordinator.
    pub connected_at: HlcTimestamp,
    /// How many nodes it currently hosts.
    pub node_count: u32,
    /// Placement labels it advertises.
    pub labels: BTreeMap<String, String>,
    /// Whether the coordinator currently considers it reachable.
    pub reachable: bool,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::round_trip;
    use crate::ids::MachineName;

    fn node(name: &str) -> NodeId {
        NodeId::new(name).unwrap()
    }

    fn daemon() -> DaemonId {
        DaemonId::from_parts(Some("robot-1"), uuid::Uuid::from_u128(9)).unwrap()
    }

    fn causes() -> Vec<NodeExitCause> {
        vec![
            NodeExitCause::Success,
            NodeExitCause::ExitCode { code: 0 },
            NodeExitCause::ExitCode { code: -1 },
            NodeExitCause::Signal {
                signal: 9,
                name: "SIGKILL".into(),
            },
            NodeExitCause::Panic {
                message: "index out of bounds".into(),
            },
            NodeExitCause::SpawnFailed {
                message: "no such file".into(),
            },
            NodeExitCause::SpawnDeadlineExceeded {
                after: DurationMs::from_secs(10),
            },
            NodeExitCause::HealthCheckTimeout {
                after: DurationMs::from_secs(5),
            },
            NodeExitCause::Killed {
                reason: "finish grace expired".into(),
            },
            NodeExitCause::DaemonShutdown,
            NodeExitCause::RestartBudgetExhausted {
                restarts: 5,
                window: DurationMs::from_secs(60),
            },
            NodeExitCause::Cancelled,
            NodeExitCause::DaemonUnreachable { daemon: daemon() },
        ]
    }

    #[test]
    fn every_exit_cause_round_trips_and_names_itself() {
        let mut names = std::collections::BTreeSet::new();
        for cause in causes() {
            assert_eq!(round_trip(&cause).unwrap(), cause);
            names.insert(cause.kind_name());
            assert!(!cause.to_string().is_empty());
        }
        assert_eq!(names.len(), 12, "one name per variant");
    }

    #[test]
    fn success_and_failure_partition_correctly() {
        assert!(NodeExitCause::Success.is_success());
        assert!(NodeExitCause::ExitCode { code: 0 }.is_success());
        assert!(!NodeExitCause::ExitCode { code: 1 }.is_success());
        assert!(NodeExitCause::ExitCode { code: 1 }.is_failure());

        // Deliberate shutdowns are neither successes nor failures.
        for cause in [NodeExitCause::Cancelled, NodeExitCause::DaemonShutdown] {
            assert!(!cause.is_success(), "{cause}");
            assert!(!cause.is_failure(), "{cause}");
        }
    }

    #[test]
    fn every_other_cause_is_a_failure() {
        for cause in causes() {
            let deliberate = matches!(
                cause,
                NodeExitCause::Cancelled | NodeExitCause::DaemonShutdown
            );
            assert_eq!(cause.is_failure(), !cause.is_success() && !deliberate);
        }
    }

    #[test]
    fn dataflow_statuses_round_trip_and_classify() {
        for status in DataflowStatus::ALL.iter().copied() {
            assert_eq!(round_trip(&status).unwrap(), status);
            assert_eq!(status.to_string(), status.as_str());
            assert!(!(status.is_terminal() && status.is_active()));
        }
        assert!(DataflowStatus::Finished.is_terminal());
        assert!(DataflowStatus::Failed.is_terminal());
        assert!(DataflowStatus::Running.is_active());
        assert!(!DataflowStatus::Pending.is_active());
        assert_eq!(DataflowStatus::default(), DataflowStatus::Pending);
    }

    #[test]
    fn node_run_states_round_trip_and_classify() {
        for state in NodeRunState::ALL.iter().copied() {
            assert_eq!(round_trip(&state).unwrap(), state);
            assert_eq!(state.to_string(), state.as_str());
        }
        assert!(NodeRunState::Finished.is_terminal());
        assert!(NodeRunState::Failed.is_terminal());
        assert!(!NodeRunState::Restarting.is_terminal());
        assert_eq!(NodeRunState::default(), NodeRunState::Pending);
    }

    #[test]
    fn stop_causes_round_trip_and_classify() {
        let all = [
            StopCause::Requested,
            StopCause::DataflowFinished,
            StopCause::AllInputsClosed,
            StopCause::DaemonShutdown,
            StopCause::PeerFailed { node: node("a") },
            StopCause::Destroyed,
            StopCause::Error {
                message: "boom".into(),
            },
        ];
        for cause in all {
            assert_eq!(round_trip(&cause).unwrap(), cause);
            assert!(!cause.to_string().is_empty());
        }
        assert!(StopCause::Requested.is_graceful());
        assert!(!StopCause::DaemonShutdown.is_graceful());
        assert!(
            !StopCause::Error {
                message: String::new()
            }
            .is_graceful()
        );
    }

    #[test]
    fn a_clean_run_finishes_rather_than_failing() {
        let mut result = DataflowResult::new(DataflowId::from_u128(1), HlcTimestamp::new(1_000, 0));
        result.record(node("a"), NodeExitCause::Success);
        result.record(node("b"), NodeExitCause::ExitCode { code: 0 });
        result.record(node("c"), NodeExitCause::Cancelled);
        result.finish(HlcTimestamp::new(3_000_000_000, 0));

        assert_eq!(result.status, DataflowStatus::Finished);
        assert!(!result.has_failures());
        assert_eq!(result.failed_nodes().count(), 0);
        assert_eq!(result.duration(), Some(DurationMs::new(2_999)));
    }

    #[test]
    fn one_failure_fails_the_run() {
        let mut result = DataflowResult::new(DataflowId::from_u128(1), HlcTimestamp::new(0, 0));
        result.record(node("a"), NodeExitCause::Success);
        result.record(
            node("b"),
            NodeExitCause::Panic {
                message: "boom".into(),
            },
        );
        result.finish(HlcTimestamp::new(1, 0));

        assert_eq!(result.status, DataflowStatus::Failed);
        assert!(result.has_failures());
        let failed: Vec<_> = result
            .failed_nodes()
            .map(|(node, _)| node.clone())
            .collect();
        assert_eq!(failed, vec![node("b")]);
        assert_eq!(round_trip(&result).unwrap(), result);
    }

    #[test]
    fn an_unfinished_run_has_no_duration() {
        let result = DataflowResult::new(DataflowId::from_u128(1), HlcTimestamp::new(5, 0));
        assert_eq!(result.duration(), None);
        assert_eq!(result.status, DataflowStatus::Running);
    }

    #[test]
    fn summaries_round_trip_and_display() {
        let mut summary = DataflowSummary::pending(DataflowId::from_u128(0x2A), 3);
        assert_eq!(summary.display_name(), summary.id.to_string());
        assert_eq!(summary.running_nodes, 0);

        summary.name = Some("perception-demo".to_owned());
        summary.status = DataflowStatus::Running;
        summary.daemons.push(daemon());
        summary.running_nodes = 3;
        summary.started_at = Some(HlcTimestamp::new(1, 0));
        assert_eq!(summary.display_name(), "perception-demo");
        assert_eq!(round_trip(&summary).unwrap(), summary);
    }

    #[test]
    fn node_info_round_trips() {
        let info = NodeInfo {
            dataflow: DataflowId::from_u128(1),
            node: node("camera"),
            daemon: daemon(),
            state: NodeRunState::Running,
            pid: Some(4242),
            generation: 2,
            restart_count: 1,
            inputs: BTreeMap::from([(
                DataId::new("trigger").unwrap(),
                Some(TypeUrn::new("std/core/v1/Empty").unwrap()),
            )]),
            outputs: BTreeMap::from([(DataId::new("image").unwrap(), None)]),
            started_at: Some(HlcTimestamp::new(7, 0)),
            exit_cause: None,
        };
        assert_eq!(round_trip(&info).unwrap(), info);
    }

    #[test]
    fn daemon_info_round_trips() {
        let info = DaemonInfo {
            id: DaemonId::generate(Some(MachineName::new("lab-1").unwrap())),
            version: crate::version::AstrsVersion::from_parts(0, 1, 0),
            address: "127.0.0.1:7408".to_owned(),
            connected_at: HlcTimestamp::new(11, 0),
            node_count: 4,
            labels: BTreeMap::from([("gpu".to_owned(), "a100".to_owned())]),
            reachable: true,
        };
        assert_eq!(round_trip(&info).unwrap(), info);
    }
}
