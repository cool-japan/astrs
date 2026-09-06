//! The vocabulary of the coordinator ↔ daemon leg: registrations, build and
//! spawn outcomes, and the state catch-up log.
//!
//! # Why a catch-up log exists
//!
//! Blueprint §5.2 gives the coordinator *"state catch-up on reconnect"* and
//! §12 makes crash recovery a first-class concern. A daemon that loses its
//! coordinator link keeps running its nodes — that is the point of the split —
//! so when the link comes back the two disagree about the world. Replaying a
//! numbered log of state transitions is how they re-converge without stopping
//! anything: the daemon says "I am at sequence 41", the coordinator sends
//! everything after 41, and the daemon acknowledges the highest sequence it
//! applied ([`crate::DaemonEvent::StateCatchUpAck`]).

use core::fmt;
use std::collections::BTreeMap;

use astrs_time::HlcTimestamp;
use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::common::duration::DurationMs;
use crate::common::route::RouteSpec;
use crate::common::status::{DataflowStatus, NodeRunState};
use crate::ids::{
    BuildId, DaemonId, DataflowId, MachineName, NodeId, ParamKey, PortRef, SessionId,
    SubscriptionId,
};
use crate::messages::control::types::ParamScope;
use crate::metadata::Parameter;
use crate::version::AstrsVersion;

/// What a daemon tells the coordinator about itself when it registers.
///
/// # Examples
///
/// ```
/// use astrs_wire::{DaemonId, DaemonRegistration, SessionId};
///
/// let registration = DaemonRegistration::new(
///     DaemonId::generate(None),
///     "quic://10.0.0.4:7407",
///     SessionId::from_u128(1),
/// )
/// .with_label("zone", "front");
///
/// assert_eq!(registration.label("zone"), Some("front"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct DaemonRegistration {
    /// The daemon's identity.
    pub daemon: DaemonId,
    /// The machine it runs on, when the deployment names machines (§8.3
    /// `deploy.machine`).
    pub machine: Option<MachineName>,
    /// The address peers should dial to reach it.
    pub address: String,
    /// The daemon's AstRS release.
    pub version: AstrsVersion,
    /// Free-form labels for placement rules (`zone: front`, `gpu: yes`).
    pub labels: BTreeMap<String, String>,
    /// The session this registration belongs to, so a reconnecting daemon can
    /// be recognised as the same one.
    pub session: SessionId,
    /// How many nodes this daemon is already running — non-zero when a daemon
    /// reconnects to a coordinator that restarted under it.
    pub running_nodes: u32,
    /// The highest state-catch-up sequence this daemon has applied, or zero
    /// for a cold start.
    pub catch_up_seq: u64,
}

impl DaemonRegistration {
    /// A registration for a freshly started daemon.
    #[must_use]
    pub fn new(daemon: DaemonId, address: impl Into<String>, session: SessionId) -> Self {
        Self {
            daemon,
            machine: None,
            address: address.into(),
            version: AstrsVersion::current(),
            labels: BTreeMap::new(),
            session,
            running_nodes: 0,
            catch_up_seq: 0,
        }
    }

    /// Names the machine this daemon runs on.
    #[must_use]
    pub fn with_machine(mut self, machine: MachineName) -> Self {
        self.machine = Some(machine);
        self
    }

    /// Adds a placement label.
    #[must_use]
    pub fn with_label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    /// Records that this daemon is resuming rather than starting cold.
    #[must_use]
    pub const fn with_resume_state(mut self, running_nodes: u32, catch_up_seq: u64) -> Self {
        self.running_nodes = running_nodes;
        self.catch_up_seq = catch_up_seq;
        self
    }

    /// Reads a placement label.
    #[must_use]
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(String::as_str)
    }

    /// Whether this daemon is reconnecting with work already in progress.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{DaemonId, DaemonRegistration, SessionId};
    ///
    /// let cold = DaemonRegistration::new(DaemonId::generate(None), "uds", SessionId::NIL);
    /// assert!(!cold.is_resuming());
    /// assert!(cold.with_resume_state(3, 41).is_resuming());
    /// ```
    #[must_use]
    pub const fn is_resuming(&self) -> bool {
        self.running_nodes > 0 || self.catch_up_seq > 0
    }
}

impl fmt::Display for DaemonRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "daemon {} at {}", self.daemon, self.address)?;
        if self.is_resuming() {
            write!(
                f,
                " (resuming {} node(s) from sequence {})",
                self.running_nodes, self.catch_up_seq
            )?;
        }
        Ok(())
    }
}

/// One node's build instructions (§17 `build`).
///
/// The coordinator resolves the manifest and hands each daemon the exact
/// commands to run, already split into argv — blueprint §16: *"No shell by
/// default: `build:`/`path:` exec directly (argv split by shlex rules)"*. A
/// daemon never re-parses a command line, so a manifest cannot smuggle a shell
/// metacharacter past the coordinator.
///
/// # Examples
///
/// ```
/// use astrs_wire::{BuildStep, NodeId};
///
/// let step = BuildStep::new(NodeId::new("camera")?, ["cargo", "build", "--release"])
///     .with_working_dir("nodes/camera");
/// assert_eq!(step.program(), Some("cargo"));
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct BuildStep {
    /// The node this step builds.
    pub node: NodeId,
    /// The command, already split into argv.
    pub command: Vec<String>,
    /// The directory to run it in.
    pub working_dir: Option<String>,
    /// Extra environment variables, applied over the daemon's scrubbed set
    /// (§16 env hygiene).
    pub env: BTreeMap<String, String>,
    /// Give up after this long.
    pub timeout: Option<DurationMs>,
}

impl BuildStep {
    /// A step running `command` for `node`.
    #[must_use]
    pub fn new<I, S>(node: NodeId, command: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            node,
            command: command.into_iter().map(Into::into).collect(),
            working_dir: None,
            env: BTreeMap::new(),
            timeout: None,
        }
    }

    /// Sets the directory the command runs in.
    #[must_use]
    pub fn with_working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Adds an environment variable.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Sets a deadline for the step.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: DurationMs) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// The program to execute, if the command is not empty.
    #[must_use]
    pub fn program(&self) -> Option<&str> {
        self.command.first().map(String::as_str)
    }

    /// The arguments after the program.
    #[must_use]
    pub fn args(&self) -> &[String] {
        self.command.get(1..).unwrap_or(&[])
    }

    /// Whether this step has anything to run.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.command.is_empty()
    }
}

impl fmt::Display for BuildStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.node, self.command.join(" "))
    }
}

/// How a build ended.
///
/// # Examples
///
/// ```
/// use astrs_wire::{BuildOutcome, DurationMs};
///
/// let ok = BuildOutcome::Succeeded {
///     artifacts: vec!["target/release/camera".to_owned()],
///     took: DurationMs::from_secs(12),
/// };
/// assert!(ok.is_success());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BuildOutcome {
    /// Every step succeeded.
    #[oxicode(variant = 0)]
    Succeeded {
        /// The artefact paths produced, for the coordinator's build cache
        /// index (§5.2 `astrs-store`).
        artifacts: Vec<String>,
        /// How long the build took.
        took: DurationMs,
    },
    /// A step failed.
    #[oxicode(variant = 1)]
    Failed {
        /// The node whose step failed, when the failure is attributable.
        node: Option<NodeId>,
        /// The process exit code, when the step was a process that ran.
        exit_code: Option<i32>,
        /// What went wrong.
        message: String,
        /// The tail of the build output, capped by the daemon.
        output: String,
    },
    /// The build was cancelled before it finished.
    #[oxicode(variant = 2)]
    Cancelled,
}

impl BuildOutcome {
    /// Whether the build succeeded.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Succeeded { .. })
    }

    /// A stable, lower-case name for logs and metrics labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Succeeded { .. } => "succeeded",
            Self::Failed { .. } => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for BuildOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Succeeded { artifacts, took } => {
                write!(f, "built {} artefact(s) in {took}", artifacts.len())
            }
            Self::Failed { node, message, .. } => match node {
                Some(node) => write!(f, "build of {node} failed: {message}"),
                None => write!(f, "build failed: {message}"),
            },
            Self::Cancelled => f.write_str("build cancelled"),
        }
    }
}

/// How a spawn attempt ended.
///
/// # Examples
///
/// ```
/// use astrs_time::HlcTimestamp;
/// use astrs_wire::SpawnOutcome;
///
/// let spawned = SpawnOutcome::Spawned {
///     pid: Some(4_242),
///     started_at: HlcTimestamp::new(1_000, 0),
/// };
/// assert!(spawned.is_success());
/// assert_eq!(spawned.pid(), Some(4_242));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SpawnOutcome {
    /// The process started and registered.
    #[oxicode(variant = 0)]
    Spawned {
        /// The operating-system process id, absent for a node the daemon does
        /// not own a process for (a dynamic node that attached itself).
        pid: Option<u32>,
        /// When it started.
        started_at: HlcTimestamp,
    },
    /// The process could not be started.
    #[oxicode(variant = 1)]
    Failed {
        /// What went wrong.
        message: String,
        /// The operating-system error number, when there was one.
        errno: Option<i32>,
    },
    /// The node is `path: dynamic` and the daemon is waiting for it to attach
    /// (§8.3).
    #[oxicode(variant = 2)]
    AwaitingDynamic,
    /// The spawn was cancelled — the dataflow stopped before this node started.
    #[oxicode(variant = 3)]
    Cancelled,
}

impl SpawnOutcome {
    /// Whether a process is now running.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Spawned { .. })
    }

    /// The process id, if one was reported.
    #[must_use]
    pub const fn pid(&self) -> Option<u32> {
        match self {
            Self::Spawned { pid, .. } => *pid,
            _ => None,
        }
    }

    /// A stable, lower-case name for logs and metrics labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Spawned { .. } => "spawned",
            Self::Failed { .. } => "failed",
            Self::AwaitingDynamic => "awaiting_dynamic",
            Self::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for SpawnOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawned { pid, .. } => match pid {
                Some(pid) => write!(f, "spawned as pid {pid}"),
                None => f.write_str("spawned"),
            },
            Self::Failed { message, errno } => match errno {
                Some(errno) => write!(f, "spawn failed ({errno}): {message}"),
                None => write!(f, "spawn failed: {message}"),
            },
            Self::AwaitingDynamic => f.write_str("waiting for a dynamic node to attach"),
            Self::Cancelled => f.write_str("spawn cancelled"),
        }
    }
}

/// One cross-daemon edge, and how to reach the daemon holding its far end
/// (blueprint §4.2, §6.4).
///
/// A [`RouteSpec`] names the two *ports* an edge joins, which is everything a
/// same-host route needs. A route whose producer and consumer live on
/// different machines needs one more fact that no daemon can derive on its
/// own: **which daemon** hosts the other end, and **what address** to dial it
/// on. Only the coordinator knows that — it is the process every daemon
/// registered its peer address with (see [`DaemonRegistration::address`]) —
/// so it hands the pair down beside the spawn that creates the edge, in
/// [`crate::CoordinatorEvent::PeerRoutes`].
///
/// # Which side dials
///
/// The directive deliberately does *not* say. Both daemons receive the same
/// route, and each one derives its own role from state it already holds: the
/// daemon that hosts `route.key.producer`'s node is the producing side (it
/// sends [`crate::PeerEvent::RouteSetup`] and forwards payloads), the daemon
/// that hosts `route.key.consumer`'s node is the consuming side (it answers
/// the setup and delivers into a local mailbox). A flag on the wire could
/// disagree with the daemon's own node table; a derivation cannot.
///
/// # Examples
///
/// ```
/// use astrs_wire::{DaemonId, DataflowId, PeerRouteDirective, Plane, RouteKey, RouteSpec};
///
/// let peer = DaemonId::generate(None);
/// let directive = PeerRouteDirective::new(
///     RouteSpec::new(RouteKey::new(
///         DataflowId::from_u128(1),
///         "camera/image".parse()?,
///         "detector/frames".parse()?,
///     ))
///     .with_plane(Plane::Tcp),
///     peer.clone(),
///     "tcp:10.0.0.4:7409",
/// );
///
/// assert_eq!(directive.peer, peer);
/// assert_eq!(directive.dataflow(), DataflowId::from_u128(1));
/// assert_eq!(directive.producer_node().as_str(), "camera");
/// assert_eq!(directive.consumer_node().as_str(), "detector");
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct PeerRouteDirective {
    /// The edge that crosses the machine boundary, on the plane the
    /// coordinator chose for it (§6.4: `Tcp`, or `Quic` when both ends
    /// negotiated it).
    pub route: RouteSpec,
    /// The daemon hosting the end of `route` this daemon does **not** host.
    pub peer: DaemonId,
    /// The address to dial `peer` on — the value it registered with
    /// ([`DaemonRegistration::address`]), parsed by
    /// `astrs_transport::TransportAddr`.
    pub address: String,
}

impl PeerRouteDirective {
    /// One directive.
    #[must_use]
    pub fn new(route: RouteSpec, peer: DaemonId, address: impl Into<String>) -> Self {
        Self {
            route,
            peer,
            address: address.into(),
        }
    }

    /// The dataflow the edge belongs to.
    #[must_use]
    pub const fn dataflow(&self) -> DataflowId {
        self.route.key.dataflow
    }

    /// The node producing on this edge.
    #[must_use]
    pub const fn producer_node(&self) -> &NodeId {
        self.route.key.producer.node()
    }

    /// The node consuming on this edge.
    #[must_use]
    pub const fn consumer_node(&self) -> &NodeId {
        self.route.key.consumer.node()
    }
}

impl fmt::Display for PeerRouteDirective {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} via {} at {}",
            self.route.key, self.peer, self.address
        )
    }
}

/// One numbered state transition in the catch-up log.
///
/// # Examples
///
/// ```
/// use astrs_time::HlcTimestamp;
/// use astrs_wire::{DataflowId, DataflowStatus, StateEntry, StateEntryKind};
///
/// let entry = StateEntry::new(
///     41,
///     HlcTimestamp::new(1_000, 0),
///     StateEntryKind::DataflowStatus {
///         dataflow: DataflowId::from_u128(1),
///         status: DataflowStatus::Running,
///         name: None,
///     },
/// );
/// assert_eq!(entry.seq, 41);
/// assert_eq!(entry.dataflow(), Some(DataflowId::from_u128(1)));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct StateEntry {
    /// The log position. Strictly increasing within one coordinator
    /// incarnation; a daemon applies entries in order and remembers the
    /// highest it has seen.
    pub seq: u64,
    /// When the transition happened, for ordering across a coordinator
    /// restart.
    pub timestamp: HlcTimestamp,
    /// What changed.
    pub kind: StateEntryKind,
}

impl StateEntry {
    /// One entry.
    #[must_use]
    pub const fn new(seq: u64, timestamp: HlcTimestamp, kind: StateEntryKind) -> Self {
        Self {
            seq,
            timestamp,
            kind,
        }
    }

    /// The dataflow this entry concerns, when it concerns one.
    #[must_use]
    pub fn dataflow(&self) -> Option<DataflowId> {
        self.kind.dataflow()
    }

    /// Compares two entries with `f64` bit patterns rather than IEEE equality.
    #[must_use]
    pub fn bitwise_eq(&self, other: &Self) -> bool {
        self.seq == other.seq
            && self.timestamp == other.timestamp
            && self.kind.bitwise_eq(&other.kind)
    }
}

impl fmt::Display for StateEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{} {}", self.seq, self.kind)
    }
}

/// What one [`StateEntry`] records.
///
/// The set is deliberately small: only state a reconnecting daemon cannot
/// reconstruct for itself. A daemon knows its own processes; it does not know
/// which dataflows the coordinator has since destroyed, which parameters an
/// operator changed, or which topic taps a CLI opened.
///
/// # Examples
///
/// ```
/// use astrs_wire::{DaemonId, StateEntryKind};
///
/// let presence = StateEntryKind::DaemonPresence {
///     daemon: DaemonId::generate(None),
///     connected: false,
/// };
/// assert_eq!(presence.kind_name(), "daemon_presence");
/// assert_eq!(presence.dataflow(), None);
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StateEntryKind {
    /// A dataflow entered a new status.
    #[oxicode(variant = 0)]
    DataflowStatus {
        /// The dataflow.
        dataflow: DataflowId,
        /// Its new status.
        status: DataflowStatus,
        /// The name it was started under, if any.
        name: Option<String>,
    },
    /// A node entered a new run state.
    #[oxicode(variant = 1)]
    NodeState {
        /// The dataflow the node belongs to.
        dataflow: DataflowId,
        /// The node.
        node: NodeId,
        /// The incarnation this state belongs to.
        generation: u64,
        /// Its new state.
        state: NodeRunState,
    },
    /// A parameter was written.
    #[oxicode(variant = 2)]
    ParamSet {
        /// The scope it was written in.
        scope: ParamScope,
        /// The key.
        key: ParamKey,
        /// The value.
        value: Parameter,
    },
    /// A parameter was deleted.
    #[oxicode(variant = 3)]
    ParamDeleted {
        /// The scope it was deleted from.
        scope: ParamScope,
        /// The key.
        key: ParamKey,
    },
    /// A route was established or removed.
    #[oxicode(variant = 4)]
    Route {
        /// The route.
        route: RouteSpec,
        /// `true` when established, `false` when removed.
        active: bool,
    },
    /// A fan-out subscription was opened or closed.
    #[oxicode(variant = 5)]
    Subscription {
        /// The subscription.
        subscription: SubscriptionId,
        /// The dataflow it taps.
        dataflow: DataflowId,
        /// The port it taps.
        port: PortRef,
        /// `true` when opened, `false` when closed.
        active: bool,
    },
    /// A peer daemon joined or left the cluster.
    #[oxicode(variant = 6)]
    DaemonPresence {
        /// The daemon.
        daemon: DaemonId,
        /// `true` when it joined, `false` when it left.
        connected: bool,
    },
    /// A build finished, so its artefacts can be reused.
    #[oxicode(variant = 7)]
    BuildFinished {
        /// The build.
        build: BuildId,
        /// The dataflow it was for.
        dataflow: DataflowId,
        /// Whether it succeeded.
        success: bool,
    },
}

impl StateEntryKind {
    /// The dataflow this entry concerns, when it concerns one.
    #[must_use]
    pub fn dataflow(&self) -> Option<DataflowId> {
        match self {
            Self::DataflowStatus { dataflow, .. }
            | Self::NodeState { dataflow, .. }
            | Self::Subscription { dataflow, .. }
            | Self::BuildFinished { dataflow, .. } => Some(*dataflow),
            Self::ParamSet { scope, .. } | Self::ParamDeleted { scope, .. } => scope.dataflow(),
            Self::Route { route, .. } => Some(route.key.dataflow),
            Self::DaemonPresence { .. } => None,
        }
    }

    /// A stable, lower-case name for logs and metrics labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::DataflowStatus { .. } => "dataflow_status",
            Self::NodeState { .. } => "node_state",
            Self::ParamSet { .. } => "param_set",
            Self::ParamDeleted { .. } => "param_deleted",
            Self::Route { .. } => "route",
            Self::Subscription { .. } => "subscription",
            Self::DaemonPresence { .. } => "daemon_presence",
            Self::BuildFinished { .. } => "build_finished",
        }
    }

    /// Compares two entries with `f64` bit patterns rather than IEEE equality.
    #[must_use]
    pub fn bitwise_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::ParamSet {
                    scope: left_scope,
                    key: left_key,
                    value: left_value,
                },
                Self::ParamSet {
                    scope: right_scope,
                    key: right_key,
                    value: right_value,
                },
            ) => {
                left_scope == right_scope
                    && left_key == right_key
                    && left_value.bitwise_eq(right_value)
            }
            _ => self == other,
        }
    }
}

impl fmt::Display for StateEntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DataflowStatus {
                dataflow, status, ..
            } => write!(f, "dataflow {dataflow} is {status}"),
            Self::NodeState {
                dataflow,
                node,
                generation,
                state,
            } => write!(f, "node {dataflow}/{node}#{generation} is {state}"),
            Self::ParamSet { scope, key, value } => write!(f, "{scope}: {key} = {value}"),
            Self::ParamDeleted { scope, key } => write!(f, "{scope}: {key} deleted"),
            Self::Route { route, active } => write!(
                f,
                "route {} {}",
                route.key,
                if *active { "established" } else { "removed" }
            ),
            Self::Subscription {
                subscription,
                port,
                active,
                ..
            } => write!(
                f,
                "subscription {subscription} on {port} {}",
                if *active { "opened" } else { "closed" }
            ),
            Self::DaemonPresence { daemon, connected } => write!(
                f,
                "daemon {daemon} {}",
                if *connected { "joined" } else { "left" }
            ),
            Self::BuildFinished { build, success, .. } => write!(
                f,
                "build {build} {}",
                if *success { "succeeded" } else { "failed" }
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireEncode, round_trip};
    use crate::common::route::RouteKey;

    fn dataflow() -> DataflowId {
        DataflowId::from_u128(0x0D)
    }

    #[test]
    fn a_registration_round_trips_and_reports_resumption() {
        let registration = DaemonRegistration::new(
            DaemonId::generate(Some(MachineName::new("robot-01").unwrap())),
            "quic://10.0.0.4:7407",
            SessionId::from_u128(2),
        )
        .with_machine(MachineName::new("robot-01").unwrap())
        .with_label("zone", "front");

        assert!(!registration.is_resuming());
        assert_eq!(registration.label("zone"), Some("front"));
        assert_eq!(registration.label("missing"), None);
        assert_eq!(round_trip(&registration).unwrap(), registration);

        let resuming = registration.with_resume_state(3, 41);
        assert!(resuming.is_resuming());
        assert!(resuming.to_string().contains("resuming"));
    }

    #[test]
    fn build_steps_split_program_and_arguments() {
        let step = BuildStep::new(
            NodeId::new("camera").unwrap(),
            ["cargo", "build", "--release"],
        )
        .with_working_dir("nodes/camera")
        .with_env("RUSTFLAGS", "-C target-cpu=native")
        .with_timeout(DurationMs::from_secs(600));

        assert_eq!(step.program(), Some("cargo"));
        assert_eq!(step.args(), ["build".to_owned(), "--release".to_owned()]);
        assert!(!step.is_empty());
        assert_eq!(round_trip(&step).unwrap(), step);
        assert!(step.to_string().contains("cargo build --release"));

        let empty = BuildStep::new(NodeId::new("camera").unwrap(), Vec::<String>::new());
        assert!(empty.is_empty());
        assert_eq!(empty.program(), None);
        assert!(empty.args().is_empty());
    }

    #[test]
    fn build_outcome_indices_are_frozen() {
        let outcomes = [
            BuildOutcome::Succeeded {
                artifacts: vec!["target/release/camera".to_owned()],
                took: DurationMs::from_secs(12),
            },
            BuildOutcome::Failed {
                node: Some(NodeId::new("camera").unwrap()),
                exit_code: Some(101),
                message: "compile error".to_owned(),
                output: "error[E0433]".to_owned(),
            },
            BuildOutcome::Cancelled,
        ];
        for (index, outcome) in outcomes.into_iter().enumerate() {
            assert_eq!(usize::from(outcome.encode_to_vec().unwrap()[0]), index);
            assert_eq!(round_trip(&outcome).unwrap(), outcome);
            assert!(!outcome.kind_name().is_empty());
            assert!(!outcome.to_string().is_empty());
            assert_eq!(outcome.is_success(), index == 0);
        }
    }

    #[test]
    fn spawn_outcome_indices_are_frozen() {
        let outcomes = [
            SpawnOutcome::Spawned {
                pid: Some(4_242),
                started_at: HlcTimestamp::new(1_000, 0),
            },
            SpawnOutcome::Failed {
                message: "no such file".to_owned(),
                errno: Some(2),
            },
            SpawnOutcome::AwaitingDynamic,
            SpawnOutcome::Cancelled,
        ];
        for (index, outcome) in outcomes.into_iter().enumerate() {
            assert_eq!(usize::from(outcome.encode_to_vec().unwrap()[0]), index);
            assert_eq!(round_trip(&outcome).unwrap(), outcome);
            assert_eq!(outcome.is_success(), index == 0);
            assert!(!outcome.to_string().is_empty());
        }
        assert_eq!(
            SpawnOutcome::Spawned {
                pid: Some(7),
                started_at: HlcTimestamp::EPOCH
            }
            .pid(),
            Some(7)
        );
        assert_eq!(SpawnOutcome::Cancelled.pid(), None);
    }

    fn state_entry_kinds() -> Vec<StateEntryKind> {
        vec![
            StateEntryKind::DataflowStatus {
                dataflow: dataflow(),
                status: DataflowStatus::Running,
                name: Some("demo".to_owned()),
            },
            StateEntryKind::NodeState {
                dataflow: dataflow(),
                node: NodeId::new("camera").unwrap(),
                generation: 2,
                state: NodeRunState::Running,
            },
            StateEntryKind::ParamSet {
                scope: ParamScope::dataflow_scope(dataflow()),
                key: ParamKey::new("gain").unwrap(),
                value: Parameter::Integer(3),
            },
            StateEntryKind::ParamDeleted {
                scope: ParamScope::Global,
                key: ParamKey::new("gain").unwrap(),
            },
            StateEntryKind::Route {
                route: RouteSpec::new(RouteKey::new(
                    dataflow(),
                    "camera/image".parse().unwrap(),
                    "detector/frames".parse().unwrap(),
                )),
                active: true,
            },
            StateEntryKind::Subscription {
                subscription: SubscriptionId::FIRST,
                dataflow: dataflow(),
                port: "camera/image".parse().unwrap(),
                active: true,
            },
            StateEntryKind::DaemonPresence {
                daemon: DaemonId::new(None, uuid_for_test()),
                connected: true,
            },
            StateEntryKind::BuildFinished {
                build: BuildId::from_u128(4),
                dataflow: dataflow(),
                success: true,
            },
        ]
    }

    fn uuid_for_test() -> uuid::Uuid {
        uuid::Uuid::from_u128(0x1234)
    }

    #[test]
    fn state_entry_kind_indices_are_frozen() {
        let kinds = state_entry_kinds();
        assert_eq!(kinds.len(), 8);
        for (index, kind) in kinds.into_iter().enumerate() {
            assert_eq!(usize::from(kind.encode_to_vec().unwrap()[0]), index);
            assert_eq!(round_trip(&kind).unwrap(), kind);
            assert!(!kind.kind_name().is_empty());
            assert!(!kind.to_string().is_empty());
        }
    }

    #[test]
    fn state_entries_report_the_dataflow_they_touch() {
        for kind in state_entry_kinds() {
            let entry = StateEntry::new(7, HlcTimestamp::new(1, 0), kind.clone());
            assert_eq!(entry.dataflow(), kind.dataflow());
            match kind {
                StateEntryKind::DaemonPresence { .. }
                | StateEntryKind::ParamDeleted {
                    scope: ParamScope::Global,
                    ..
                } => assert_eq!(entry.dataflow(), None),
                _ => assert_eq!(entry.dataflow(), Some(dataflow())),
            }
            assert!(entry.to_string().starts_with("#7 "));
        }
    }

    #[test]
    fn nan_parameters_in_the_catch_up_log_survive_the_wire() {
        let entry = StateEntry::new(
            1,
            HlcTimestamp::new(1, 0),
            StateEntryKind::ParamSet {
                scope: ParamScope::Global,
                key: ParamKey::new("gain").unwrap(),
                value: Parameter::Float(f64::NAN),
            },
        );
        let decoded = round_trip(&entry).unwrap();
        assert_ne!(decoded, entry);
        assert!(decoded.bitwise_eq(&entry));
    }
}
