//! `CLI → coordinator`: [`ControlRequest`], the ~40 verbs of §7.3 and §24.1.
//!
//! One variant per CLI verb (§17), in the order §24.1 froze. The enum is the
//! coordinator's entire public API: everything `astrs` can ask a cluster to do
//! is one of these thirty-seven messages, and everything a cluster can answer is
//! a [`crate::ControlReply`].
//!
//! # Read verbs and mutating verbs
//!
//! Blueprint §16: *"the coordinator API distinguishes read verbs
//! (list/logs/topic) from mutating verbs (start/stop/param) — token scopes are
//! 0.2; the enum split lands now so it's not a breaking change later."*
//!
//! [`ControlRequest::scope`] is that split. It is an exhaustive `match`, so a
//! verb appended tomorrow does not compile until someone has classified it —
//! which is exactly the property that makes adding token scopes in 0.2 a
//! non-breaking change.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{ControlRequest, DataflowId, RequestScope, WireMessage};
//!
//! let read = ControlRequest::Info {
//!     dataflow: DataflowId::from_u128(1),
//!     include_nodes: true,
//! };
//! assert_eq!(read.scope(), RequestScope::Read);
//! assert_eq!(read.variant_name(), "Info");
//!
//! let mutate = ControlRequest::Stop {
//!     dataflow: DataflowId::from_u128(1),
//!     grace: None,
//! };
//! assert!(mutate.is_mutating());
//! ```

use core::fmt;

use astrs_time::HlcTimestamp;
use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::common::duration::DurationMs;
use crate::common::node::{InputSpec, NodeSpawnSpec};
use crate::frame::FrameKind;
use crate::handshake::messages::Hello;
use crate::ids::{BuildId, DataId, DataflowId, NodeId, ParamKey, PortRef, SubscriptionId};
use crate::messages::control::types::{
    DataflowSource, LogQuery, ParamScope, RequestScope, TopicQuery,
};
use crate::messages::impl_wire_message;
use crate::metadata::{Metadata, Parameter};

/// The CLI → coordinator message family (§24.1).
///
/// Variant indices 0–34 are frozen by §24.1; 35 and 36 are tail appends
/// beyond §24.1 (documented at [`ControlRequest::GetNodeMetrics`] and
/// [`ControlRequest::GetManifest`]), frozen by `tests/golden/protocol.snap`
/// once regenerated. New verbs are appended at 37 and above.
///
/// # Examples
///
/// ```
/// use astrs_wire::{ControlRequest, FrameFlags, FrameLimits, WireMessage};
///
/// let request = ControlRequest::ConnectedDaemons { include_unreachable: false };
/// let limits = FrameLimits::uds();
/// let bytes = request.to_frame(FrameFlags::EMPTY, &limits)?;
/// assert_eq!(ControlRequest::from_bytes(&bytes, &limits)?, request);
/// # Ok::<(), astrs_wire::WireError>(())
/// ```
// No `Eq`: `TopicPublish` carries [`Metadata`], whose parameters may hold an
// `f64`, and `SetParam` carries a [`Parameter`] directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ControlRequest {
    /// The handshake greeting (§7.2). Always the first frame on a connection.
    #[oxicode(variant = 0)]
    Hello(Hello),
    /// Build a dataflow's nodes without starting them (`astrs build`).
    #[oxicode(variant = 1)]
    Build {
        /// The manifest to build.
        manifest: String,
        /// The directory relative paths resolve against.
        working_dir: Option<String>,
        /// A name for the resulting build, for `astrs list`.
        name: Option<String>,
        /// Rebuild even when the build cache says the artefacts are current.
        force: bool,
    },
    /// Wait for a build to finish (`astrs build` without `--detach`).
    #[oxicode(variant = 2)]
    WaitForBuild {
        /// The build to wait for.
        build: BuildId,
        /// Give up after this long.
        timeout: Option<DurationMs>,
    },
    /// Start a dataflow (`astrs start`).
    #[oxicode(variant = 3)]
    Start {
        /// Where the dataflow comes from: an inline manifest or a build.
        source: DataflowSource,
        /// A name for the run, for `astrs list` and `astrs stop --name`.
        name: Option<String>,
        /// Return as soon as the spawn is requested rather than waiting for
        /// every node to register.
        detach: bool,
    },
    /// Wait for every node of a dataflow to be spawned and registered.
    #[oxicode(variant = 4)]
    WaitForSpawn {
        /// The dataflow to wait for.
        dataflow: DataflowId,
        /// Give up after this long.
        timeout: Option<DurationMs>,
    },
    /// Health-check the cluster, or one dataflow within it (`astrs status`).
    #[oxicode(variant = 5)]
    Check {
        /// The dataflow to check; `None` checks the coordinator and its
        /// daemons.
        dataflow: Option<DataflowId>,
    },
    /// Stop a dataflow by id (`astrs stop`).
    #[oxicode(variant = 6)]
    Stop {
        /// The dataflow to stop.
        dataflow: DataflowId,
        /// How long nodes get to finish before they are killed.
        grace: Option<DurationMs>,
    },
    /// Stop a dataflow by the name it was started under.
    #[oxicode(variant = 7)]
    StopByName {
        /// The name given at `Start`.
        name: String,
        /// How long nodes get to finish before they are killed.
        grace: Option<DurationMs>,
    },
    /// Restart a whole dataflow (`astrs restart`).
    #[oxicode(variant = 8)]
    Restart {
        /// The dataflow to restart.
        dataflow: DataflowId,
        /// Rebuild before restarting.
        rebuild: bool,
    },
    /// Restart a dataflow by the name it was started under.
    #[oxicode(variant = 9)]
    RestartByName {
        /// The name given at `Start`.
        name: String,
        /// Rebuild before restarting.
        rebuild: bool,
    },
    /// Fetch a bounded batch of log records (`astrs logs`).
    #[oxicode(variant = 10)]
    Logs {
        /// The dataflow whose logs are wanted.
        dataflow: DataflowId,
        /// One node's logs, or all of them.
        node: Option<NodeId>,
        /// The filter to apply at the source.
        query: LogQuery,
    },
    /// Subscribe to the live log stream (`astrs logs -f`).
    ///
    /// The coordinator then pushes [`crate::FrameKind::Log`] frames carrying
    /// [`crate::LogFrame`] until the subscription is cancelled with
    /// [`ControlRequest::TopicUnsubscribe`], which ends any subscription.
    #[oxicode(variant = 11)]
    LogSubscribe {
        /// The dataflow to follow; `None` follows the whole cluster.
        dataflow: Option<DataflowId>,
        /// One node, or all of them.
        node: Option<NodeId>,
        /// The filter to apply at the source.
        query: LogQuery,
        /// The id the client will see on every delivered frame.
        subscription: SubscriptionId,
    },
    /// List dataflows (`astrs list`).
    #[oxicode(variant = 12)]
    List {
        /// Include dataflows that have already finished.
        all: bool,
    },
    /// Describe one dataflow in detail (`astrs info`).
    #[oxicode(variant = 13)]
    Info {
        /// The dataflow to describe.
        dataflow: DataflowId,
        /// Include a row per node.
        include_nodes: bool,
    },
    /// Shut the cluster down (`astrs down`, `astrs destroy`).
    #[oxicode(variant = 14)]
    Destroy {
        /// Kill running dataflows rather than refusing while any is alive.
        force: bool,
    },
    /// Reclaim the resources of finished dataflows (`astrs clean`).
    #[oxicode(variant = 15)]
    Clean {
        /// The dataflow to clean; `None` cleans every finished one.
        dataflow: Option<DataflowId>,
        /// Also delete build artefacts.
        artifacts: bool,
        /// Also delete captured logs.
        logs: bool,
    },
    /// List the daemons currently registered (`astrs doctor`, `astrs list`).
    #[oxicode(variant = 16)]
    ConnectedDaemons {
        /// Include daemons the coordinator has lost contact with.
        include_unreachable: bool,
    },
    /// Describe one node (`astrs info <dataflow> <node>`).
    #[oxicode(variant = 17)]
    GetNodeInfo {
        /// The dataflow the node belongs to.
        dataflow: DataflowId,
        /// The node.
        node: NodeId,
    },
    /// Subscribe to a topic's messages (`astrs topic echo`).
    ///
    /// The coordinator then pushes [`crate::FrameKind::Data`] frames carrying
    /// [`crate::DataFrame`].
    #[oxicode(variant = 18)]
    TopicSubscribe {
        /// The dataflow to tap.
        dataflow: DataflowId,
        /// The producer port to tap.
        port: PortRef,
        /// How the tap should be shaped.
        query: TopicQuery,
        /// The id the client will see on every delivered frame.
        subscription: SubscriptionId,
    },
    /// End a subscription — a topic tap or a log tail alike.
    #[oxicode(variant = 19)]
    TopicUnsubscribe {
        /// The subscription to end.
        subscription: SubscriptionId,
    },
    /// Publish one message into a running dataflow (`astrs topic pub`).
    #[oxicode(variant = 20)]
    TopicPublish {
        /// The dataflow to publish into.
        dataflow: DataflowId,
        /// The port to publish as — a virtual source, or an existing output
        /// whose producer accepts injected messages.
        port: PortRef,
        /// The metadata to attach.
        metadata: Metadata,
        /// The payload bytes (an Arrow IPC stream, opaque here).
        payload: Vec<u8>,
    },
    /// List parameters in a scope (`astrs param list`).
    #[oxicode(variant = 21)]
    GetParams {
        /// The scope to list.
        scope: ParamScope,
        /// Keep only keys starting with this prefix.
        prefix: Option<String>,
        /// Include values inherited from parent scopes.
        inherited: bool,
    },
    /// Read one parameter (`astrs param get`).
    #[oxicode(variant = 22)]
    GetParam {
        /// The scope to read from.
        scope: ParamScope,
        /// The key.
        key: ParamKey,
        /// Fall back to parent scopes when the key is absent here.
        inherited: bool,
    },
    /// Write one parameter (`astrs param set`).
    #[oxicode(variant = 23)]
    SetParam {
        /// The scope to write to.
        scope: ParamScope,
        /// The key.
        key: ParamKey,
        /// The value.
        value: Parameter,
        /// Fail rather than overwrite an existing value.
        create_only: bool,
    },
    /// Delete one parameter (`astrs param delete`).
    #[oxicode(variant = 24)]
    DeleteParam {
        /// The scope to delete from.
        scope: ParamScope,
        /// The key.
        key: ParamKey,
    },
    /// Restart one node (`astrs node restart`).
    #[oxicode(variant = 25)]
    RestartNode {
        /// The dataflow the node belongs to.
        dataflow: DataflowId,
        /// The node to restart.
        node: NodeId,
    },
    /// Stop one node (`astrs node stop`).
    #[oxicode(variant = 26)]
    StopNode {
        /// The dataflow the node belongs to.
        dataflow: DataflowId,
        /// The node to stop.
        node: NodeId,
        /// How long the node gets to finish before it is killed.
        grace: Option<DurationMs>,
    },
    /// Add a node to a running dataflow (`astrs node add`, §5.2 dynamic
    /// topology).
    #[oxicode(variant = 27)]
    AddNode {
        /// The dataflow to extend.
        dataflow: DataflowId,
        /// The fully expanded spawn specification.
        node: Box<NodeSpawnSpec>,
        /// Spawn it immediately rather than leaving it pending.
        start: bool,
    },
    /// Remove a node from a running dataflow (`astrs node remove`).
    #[oxicode(variant = 28)]
    RemoveNode {
        /// The dataflow to shrink.
        dataflow: DataflowId,
        /// The node to remove.
        node: NodeId,
        /// How long the node gets to finish before it is killed.
        grace: Option<DurationMs>,
    },
    /// Replace a node in place, keeping its edges (`astrs node replace`).
    #[oxicode(variant = 29)]
    ReplaceNode {
        /// The dataflow to edit.
        dataflow: DataflowId,
        /// The replacement specification; its `node` names the node replaced.
        node: Box<NodeSpawnSpec>,
        /// Let the outgoing node drain its inputs before it is stopped.
        drain: bool,
    },
    /// Connect an existing output to an existing input (`astrs edge add`).
    #[oxicode(variant = 30)]
    AddEdge {
        /// The dataflow to edit.
        dataflow: DataflowId,
        /// The node whose input is being connected.
        consumer: NodeId,
        /// The input to create, including its source and queue policy.
        input: InputSpec,
    },
    /// Disconnect an input (`astrs edge remove`).
    #[oxicode(variant = 31)]
    RemoveEdge {
        /// The dataflow to edit.
        dataflow: DataflowId,
        /// The node whose input is being disconnected.
        consumer: NodeId,
        /// The input to remove.
        input: DataId,
    },
    /// Start recording a dataflow to an `.arec` file (`astrs record start`,
    /// §14).
    #[oxicode(variant = 32)]
    RecordStart {
        /// The dataflow to record.
        dataflow: DataflowId,
        /// Where to write the recording.
        path: String,
        /// The ports to record; empty records every port.
        ports: Vec<PortRef>,
        /// Overwrite an existing file rather than failing.
        overwrite: bool,
    },
    /// Stop recording (`astrs record stop`).
    #[oxicode(variant = 33)]
    RecordStop {
        /// The dataflow being recorded.
        dataflow: DataflowId,
    },
    /// Fetch collected trace spans (`astrs trace`, §13).
    #[oxicode(variant = 34)]
    GetTraces {
        /// The dataflow whose spans are wanted; `None` means all.
        dataflow: Option<DataflowId>,
        /// One node's spans, or all of them.
        node: Option<NodeId>,
        /// Drop spans that started before this instant.
        since: Option<HlcTimestamp>,
        /// Stop after this many spans.
        limit: Option<u32>,
    },
    /// Fetch the latest per-node resource and queue sample (`astrs top`,
    /// blueprint §13).
    ///
    /// A tail append beyond §24.1: the daemon has sampled per-node CPU/RSS/
    /// queue-depth every two seconds since W2 (§13), but nothing let a client
    /// read a sample back until the TUI needed one. Modelled as a poll
    /// rather than a subscription — the daemon's own sampling interval
    /// already bounds how fresh a push could ever be, so a client that
    /// wants a live view simply polls this on its own redraw tick, the same
    /// way `astrs top` polls [`ControlRequest::List`] and
    /// [`ControlRequest::Info`].
    #[oxicode(variant = 35)]
    GetNodeMetrics {
        /// The dataflow whose node metrics are wanted.
        dataflow: DataflowId,
        /// One node's metrics, or every node's.
        node: Option<NodeId>,
    },
    /// Fetch a running dataflow's expanded manifest (`astrs top`'s Graph
    /// tab, blueprint §5.2/§17 `graph`).
    ///
    /// A tail append beyond §24.1, for the same reason as
    /// [`ControlRequest::GetNodeMetrics`]: the coordinator's registry has
    /// held the *expanded* manifest (every `module:` reference already
    /// flattened, blueprint §8.5) since it decided what to spawn, but
    /// nothing let a client read it back until a live topology view needed
    /// one. Answered with the expanded manifest's YAML rather than the
    /// [`astrs-graph`](https://docs.rs/astrs-graph) types built from it —
    /// this crate sits below that domain crate in the layer stack (§4.1)
    /// and must not depend on it; a caller that wants the graph parses the
    /// YAML itself, exactly as `astrs graph` already does for a manifest
    /// file on disk.
    #[oxicode(variant = 36)]
    GetManifest {
        /// The dataflow whose manifest is wanted.
        dataflow: DataflowId,
    },
    /// Fetch the *bandwidth* half of a dataflow's per-node metrics
    /// (`astrs top`'s throughput column, blueprint §6.2/§13).
    ///
    /// A tail append beyond §24.1, and a second verb rather than a wider
    /// [`ControlRequest::GetNodeMetrics`] for the same reason
    /// [`crate::DaemonEvent::NodeIoMetrics`] is a second variant: the daemon
    /// leg that carries this data is frozen, so bytes travel as their own
    /// shape end to end ([`crate::NodeIoSample`]) rather than by widening a
    /// message somebody already depends on the layout of.
    #[oxicode(variant = 37)]
    GetNodeIoMetrics {
        /// The dataflow whose bandwidth samples are wanted.
        dataflow: DataflowId,
        /// One node's samples, or every node's.
        node: Option<NodeId>,
    },
}

impl ControlRequest {
    /// Whether this request observes the cluster or changes it (§16).
    ///
    /// The `match` is exhaustive on purpose: a verb appended at the tail will
    /// not compile until it has been classified, which is what keeps the 0.2
    /// token-scope work from becoming a breaking change.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{ControlRequest, DataflowId, RequestScope};
    ///
    /// assert_eq!(ControlRequest::List { all: true }.scope(), RequestScope::Read);
    /// assert_eq!(
    ///     ControlRequest::RecordStop { dataflow: DataflowId::from_u128(1) }.scope(),
    ///     RequestScope::Mutate
    /// );
    /// ```
    #[must_use]
    pub const fn scope(&self) -> RequestScope {
        match self {
            // Observation: nothing an operator could notice changes.
            Self::Check { .. }
            | Self::Logs { .. }
            | Self::LogSubscribe { .. }
            | Self::List { .. }
            | Self::Info { .. }
            | Self::ConnectedDaemons { .. }
            | Self::GetNodeInfo { .. }
            | Self::GetNodeIoMetrics { .. }
            | Self::TopicSubscribe { .. }
            | Self::TopicUnsubscribe { .. }
            | Self::GetParams { .. }
            | Self::GetParam { .. }
            | Self::GetTraces { .. }
            | Self::GetNodeMetrics { .. }
            | Self::GetManifest { .. }
            | Self::WaitForBuild { .. }
            | Self::WaitForSpawn { .. } => RequestScope::Read,

            // The handshake is neither, and the safe classification of
            // "neither" is the one that grants nothing.
            Self::Hello(_) => RequestScope::Read,

            // Mutation: everything that starts, stops, edits or destroys.
            Self::Build { .. }
            | Self::Start { .. }
            | Self::Stop { .. }
            | Self::StopByName { .. }
            | Self::Restart { .. }
            | Self::RestartByName { .. }
            | Self::Destroy { .. }
            | Self::Clean { .. }
            | Self::TopicPublish { .. }
            | Self::SetParam { .. }
            | Self::DeleteParam { .. }
            | Self::RestartNode { .. }
            | Self::StopNode { .. }
            | Self::AddNode { .. }
            | Self::RemoveNode { .. }
            | Self::ReplaceNode { .. }
            | Self::AddEdge { .. }
            | Self::RemoveEdge { .. }
            | Self::RecordStart { .. }
            | Self::RecordStop { .. } => RequestScope::Mutate,
        }
    }

    /// Whether this request changes cluster state.
    #[must_use]
    pub const fn is_mutating(&self) -> bool {
        self.scope().is_mutating()
    }

    /// Whether this request only observes.
    #[must_use]
    pub const fn is_read_only(&self) -> bool {
        !self.is_mutating()
    }

    /// Whether this request is the handshake greeting.
    ///
    /// A connection's first frame must be one, and no later frame may be.
    #[must_use]
    pub const fn is_handshake(&self) -> bool {
        matches!(self, Self::Hello(_))
    }

    /// The greeting this request carries, if it is a handshake.
    #[must_use]
    pub const fn hello(&self) -> Option<&Hello> {
        match self {
            Self::Hello(hello) => Some(hello),
            _ => None,
        }
    }

    /// The dataflow this request addresses, when it addresses one by id.
    ///
    /// Requests that name a dataflow by *name* rather than id return `None` —
    /// resolving a name needs the coordinator's registry, which this crate
    /// does not have.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{ControlRequest, DataflowId};
    ///
    /// let id = DataflowId::from_u128(2);
    /// assert_eq!(
    ///     ControlRequest::Stop { dataflow: id, grace: None }.dataflow(),
    ///     Some(id)
    /// );
    /// assert_eq!(
    ///     ControlRequest::StopByName { name: "demo".into(), grace: None }.dataflow(),
    ///     None
    /// );
    /// ```
    #[must_use]
    pub fn dataflow(&self) -> Option<DataflowId> {
        match self {
            Self::WaitForSpawn { dataflow, .. }
            | Self::Stop { dataflow, .. }
            | Self::Restart { dataflow, .. }
            | Self::Logs { dataflow, .. }
            | Self::Info { dataflow, .. }
            | Self::GetNodeInfo { dataflow, .. }
            | Self::GetNodeMetrics { dataflow, .. }
            | Self::GetNodeIoMetrics { dataflow, .. }
            | Self::GetManifest { dataflow }
            | Self::TopicSubscribe { dataflow, .. }
            | Self::TopicPublish { dataflow, .. }
            | Self::RestartNode { dataflow, .. }
            | Self::StopNode { dataflow, .. }
            | Self::AddNode { dataflow, .. }
            | Self::RemoveNode { dataflow, .. }
            | Self::ReplaceNode { dataflow, .. }
            | Self::AddEdge { dataflow, .. }
            | Self::RemoveEdge { dataflow, .. }
            | Self::RecordStart { dataflow, .. }
            | Self::RecordStop { dataflow } => Some(*dataflow),

            Self::Check { dataflow }
            | Self::Clean { dataflow, .. }
            | Self::LogSubscribe { dataflow, .. }
            | Self::GetTraces { dataflow, .. } => *dataflow,

            Self::GetParams { scope, .. }
            | Self::GetParam { scope, .. }
            | Self::SetParam { scope, .. }
            | Self::DeleteParam { scope, .. } => scope.dataflow(),

            Self::Hello(_)
            | Self::Build { .. }
            | Self::WaitForBuild { .. }
            | Self::Start { .. }
            | Self::StopByName { .. }
            | Self::RestartByName { .. }
            | Self::List { .. }
            | Self::Destroy { .. }
            | Self::ConnectedDaemons { .. }
            | Self::TopicUnsubscribe { .. } => None,
        }
    }

    /// The subscription this request opens or closes, if any.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{ControlRequest, SubscriptionId};
    ///
    /// let request = ControlRequest::TopicUnsubscribe { subscription: SubscriptionId::new(4) };
    /// assert_eq!(request.subscription(), Some(SubscriptionId::new(4)));
    /// ```
    #[must_use]
    pub const fn subscription(&self) -> Option<SubscriptionId> {
        match self {
            Self::LogSubscribe { subscription, .. }
            | Self::TopicSubscribe { subscription, .. }
            | Self::TopicUnsubscribe { subscription } => Some(*subscription),
            _ => None,
        }
    }

    /// Whether this request opens a fan-out subscription, so the caller should
    /// expect [`crate::FrameKind::Data`] or [`crate::FrameKind::Log`] frames to
    /// follow.
    #[must_use]
    pub const fn opens_subscription(&self) -> bool {
        matches!(
            self,
            Self::LogSubscribe { .. } | Self::TopicSubscribe { .. }
        )
    }

    /// Whether this request blocks until something completes, and therefore
    /// deserves a longer client-side deadline than a plain query.
    #[must_use]
    pub const fn is_blocking(&self) -> bool {
        matches!(self, Self::WaitForBuild { .. } | Self::WaitForSpawn { .. })
    }

    /// The caller-supplied deadline, if the request carries one.
    #[must_use]
    pub const fn timeout(&self) -> Option<DurationMs> {
        match self {
            Self::WaitForBuild { timeout, .. } | Self::WaitForSpawn { timeout, .. } => *timeout,
            _ => None,
        }
    }

    /// The payload bytes this request carries, for bandwidth accounting.
    ///
    /// Only [`ControlRequest::TopicPublish`] carries bulk payload; a manifest
    /// counts as text, not payload.
    #[must_use]
    pub fn payload_len(&self) -> usize {
        match self {
            Self::TopicPublish { payload, .. } => payload.len(),
            _ => 0,
        }
    }

    /// Compares two requests with `f64` bit patterns rather than IEEE
    /// equality — see [`crate::PeerEvent::bitwise_eq`].
    #[must_use]
    pub fn bitwise_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::TopicPublish {
                    dataflow: left_dataflow,
                    port: left_port,
                    metadata: left_metadata,
                    payload: left_payload,
                },
                Self::TopicPublish {
                    dataflow: right_dataflow,
                    port: right_port,
                    metadata: right_metadata,
                    payload: right_payload,
                },
            ) => {
                left_dataflow == right_dataflow
                    && left_port == right_port
                    && left_metadata.bitwise_eq(right_metadata)
                    && left_payload == right_payload
            }
            (
                Self::SetParam {
                    scope: left_scope,
                    key: left_key,
                    value: left_value,
                    create_only: left_create,
                },
                Self::SetParam {
                    scope: right_scope,
                    key: right_key,
                    value: right_value,
                    create_only: right_create,
                },
            ) => {
                left_scope == right_scope
                    && left_key == right_key
                    && left_value.bitwise_eq(right_value)
                    && left_create == right_create
            }
            _ => self == other,
        }
    }
}

impl fmt::Display for ControlRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hello(hello) => write!(f, "{hello}"),
            Self::Build { name, force, .. } => {
                let name = name.as_deref().unwrap_or("<unnamed>");
                write!(f, "build {name}{}", if *force { " (forced)" } else { "" })
            }
            Self::WaitForBuild { build, .. } => write!(f, "wait for build {build}"),
            Self::Start { source, name, .. } => {
                let name = name.as_deref().unwrap_or("<unnamed>");
                write!(f, "start {name} from {source}")
            }
            Self::WaitForSpawn { dataflow, .. } => write!(f, "wait for spawn of {dataflow}"),
            Self::Check { dataflow } => match dataflow {
                Some(dataflow) => write!(f, "check {dataflow}"),
                None => f.write_str("check cluster"),
            },
            Self::Stop { dataflow, .. } => write!(f, "stop {dataflow}"),
            Self::StopByName { name, .. } => write!(f, "stop {name}"),
            Self::Restart { dataflow, .. } => write!(f, "restart {dataflow}"),
            Self::RestartByName { name, .. } => write!(f, "restart {name}"),
            Self::Logs { dataflow, node, .. } => match node {
                Some(node) => write!(f, "logs of {dataflow}/{node}"),
                None => write!(f, "logs of {dataflow}"),
            },
            Self::LogSubscribe { subscription, .. } => {
                write!(f, "follow logs as subscription {subscription}")
            }
            Self::List { all } => {
                write!(f, "list {} dataflows", if *all { "all" } else { "active" })
            }
            Self::Info { dataflow, .. } => write!(f, "info for {dataflow}"),
            Self::Destroy { force } => {
                write!(
                    f,
                    "destroy cluster{}",
                    if *force { " (forced)" } else { "" }
                )
            }
            Self::Clean { dataflow, .. } => match dataflow {
                Some(dataflow) => write!(f, "clean {dataflow}"),
                None => f.write_str("clean finished dataflows"),
            },
            Self::ConnectedDaemons { .. } => f.write_str("list daemons"),
            Self::GetNodeInfo { dataflow, node } => write!(f, "info for {dataflow}/{node}"),
            Self::TopicSubscribe {
                port, subscription, ..
            } => write!(f, "tap {port} as subscription {subscription}"),
            Self::TopicUnsubscribe { subscription } => {
                write!(f, "end subscription {subscription}")
            }
            Self::TopicPublish { port, payload, .. } => {
                write!(f, "publish {} bytes to {port}", payload.len())
            }
            Self::GetParams { scope, .. } => write!(f, "list parameters of {scope}"),
            Self::GetParam { scope, key, .. } => write!(f, "get {key} of {scope}"),
            Self::SetParam { scope, key, .. } => write!(f, "set {key} of {scope}"),
            Self::DeleteParam { scope, key } => write!(f, "delete {key} of {scope}"),
            Self::RestartNode { dataflow, node } => write!(f, "restart {dataflow}/{node}"),
            Self::StopNode { dataflow, node, .. } => write!(f, "stop {dataflow}/{node}"),
            Self::AddNode { dataflow, node, .. } => {
                write!(f, "add {} to {dataflow}", node.node)
            }
            Self::RemoveNode { dataflow, node, .. } => write!(f, "remove {node} from {dataflow}"),
            Self::ReplaceNode { dataflow, node, .. } => {
                write!(f, "replace {} in {dataflow}", node.node)
            }
            Self::AddEdge {
                dataflow,
                consumer,
                input,
            } => write!(
                f,
                "connect {} to {dataflow}/{consumer}/{}",
                input.source, input.id
            ),
            Self::RemoveEdge {
                dataflow,
                consumer,
                input,
            } => write!(f, "disconnect {dataflow}/{consumer}/{input}"),
            Self::RecordStart { dataflow, path, .. } => write!(f, "record {dataflow} to {path}"),
            Self::RecordStop { dataflow } => write!(f, "stop recording {dataflow}"),
            Self::GetTraces { dataflow, .. } => match dataflow {
                Some(dataflow) => write!(f, "traces of {dataflow}"),
                None => f.write_str("traces of every dataflow"),
            },
            Self::GetNodeMetrics { dataflow, node } => match node {
                Some(node) => write!(f, "metrics for {dataflow}/{node}"),
                None => write!(f, "metrics for {dataflow}"),
            },
            Self::GetManifest { dataflow } => write!(f, "manifest of {dataflow}"),
            Self::GetNodeIoMetrics { dataflow, node } => match node {
                Some(node) => write!(f, "bandwidth for {dataflow}/{node}"),
                None => write!(f, "bandwidth for {dataflow}"),
            },
        }
    }
}

impl_wire_message!(
    ControlRequest,
    FrameKind::Control,
    [
        "Hello",
        "Build",
        "WaitForBuild",
        "Start",
        "WaitForSpawn",
        "Check",
        "Stop",
        "StopByName",
        "Restart",
        "RestartByName",
        "Logs",
        "LogSubscribe",
        "List",
        "Info",
        "Destroy",
        "Clean",
        "ConnectedDaemons",
        "GetNodeInfo",
        "TopicSubscribe",
        "TopicUnsubscribe",
        "TopicPublish",
        "GetParams",
        "GetParam",
        "SetParam",
        "DeleteParam",
        "RestartNode",
        "StopNode",
        "AddNode",
        "RemoveNode",
        "ReplaceNode",
        "AddEdge",
        "RemoveEdge",
        "RecordStart",
        "RecordStop",
        "GetTraces",
        "GetNodeMetrics",
        "GetManifest",
        "GetNodeIoMetrics",
    ],
    fn variant_index(&self) -> u16 {
        match self {
            Self::Hello(_) => 0,
            Self::Build { .. } => 1,
            Self::WaitForBuild { .. } => 2,
            Self::Start { .. } => 3,
            Self::WaitForSpawn { .. } => 4,
            Self::Check { .. } => 5,
            Self::Stop { .. } => 6,
            Self::StopByName { .. } => 7,
            Self::Restart { .. } => 8,
            Self::RestartByName { .. } => 9,
            Self::Logs { .. } => 10,
            Self::LogSubscribe { .. } => 11,
            Self::List { .. } => 12,
            Self::Info { .. } => 13,
            Self::Destroy { .. } => 14,
            Self::Clean { .. } => 15,
            Self::ConnectedDaemons { .. } => 16,
            Self::GetNodeInfo { .. } => 17,
            Self::TopicSubscribe { .. } => 18,
            Self::TopicUnsubscribe { .. } => 19,
            Self::TopicPublish { .. } => 20,
            Self::GetParams { .. } => 21,
            Self::GetParam { .. } => 22,
            Self::SetParam { .. } => 23,
            Self::DeleteParam { .. } => 24,
            Self::RestartNode { .. } => 25,
            Self::StopNode { .. } => 26,
            Self::AddNode { .. } => 27,
            Self::RemoveNode { .. } => 28,
            Self::ReplaceNode { .. } => 29,
            Self::AddEdge { .. } => 30,
            Self::RemoveEdge { .. } => 31,
            Self::RecordStart { .. } => 32,
            Self::RecordStop { .. } => 33,
            Self::GetTraces { .. } => 34,
            Self::GetNodeMetrics { .. } => 35,
            Self::GetManifest { .. } => 36,
            Self::GetNodeIoMetrics { .. } => 37,
        }
    }
);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::auth::AuthToken;
    use crate::codec::{WireDecode, WireEncode, round_trip};
    use crate::common::node::NodeSource;
    use crate::frame::{FrameFlags, FrameLimits};
    use crate::handshake::role::Role;
    use crate::messages::WireMessage;
    use crate::messages::samples::control_requests;

    #[test]
    fn the_family_has_the_thirty_five_frozen_verbs_and_its_tail_appends() {
        assert_eq!(ControlRequest::VARIANT_NAMES.len(), 38);
        // Spot-check the ends of the §24.1 list, plus the three tail appends.
        assert_eq!(ControlRequest::VARIANT_NAMES[0], "Hello");
        assert_eq!(ControlRequest::VARIANT_NAMES[34], "GetTraces");
        assert_eq!(ControlRequest::VARIANT_NAMES[35], "GetNodeMetrics");
        assert_eq!(ControlRequest::VARIANT_NAMES[36], "GetManifest");
        assert_eq!(ControlRequest::VARIANT_NAMES[37], "GetNodeIoMetrics");
    }

    #[test]
    fn every_variant_reports_and_encodes_its_frozen_index() {
        let samples = control_requests().unwrap();
        assert_eq!(
            samples.len(),
            ControlRequest::VARIANT_NAMES.len(),
            "the sample table must cover every verb"
        );
        for (index, sample) in samples.iter().enumerate() {
            let index = u16::try_from(index).unwrap();
            assert_eq!(sample.variant_index(), index, "{sample:?}");
            let bytes = sample.encode_to_vec().unwrap();
            assert_eq!(u16::from(bytes[0]), index, "{sample:?}");
            assert_eq!(
                sample.variant_name(),
                ControlRequest::VARIANT_NAMES[usize::from(index)]
            );
        }
    }

    #[test]
    fn every_variant_round_trips_through_a_frame() {
        let limits = FrameLimits::uds();
        for sample in control_requests().unwrap() {
            let bytes = sample.to_frame(FrameFlags::CRC, &limits).unwrap();
            let decoded = ControlRequest::from_bytes(&bytes, &limits).unwrap();
            assert!(decoded.bitwise_eq(&sample), "{sample:?}");
        }
    }

    #[test]
    fn trailing_bytes_after_a_request_are_refused() {
        for sample in control_requests().unwrap() {
            let mut bytes = sample.encode_to_vec().unwrap();
            bytes.push(0xFF);
            assert!(matches!(
                ControlRequest::decode_exact(&bytes),
                Err(crate::error::WireError::TrailingBytes { .. })
            ));
        }
    }

    #[test]
    fn every_verb_is_classified_and_the_split_matches_the_blueprint() {
        for sample in control_requests().unwrap() {
            // An exhaustive match backs this; the assertion documents intent.
            let scope = sample.scope();
            assert_eq!(scope.is_mutating(), sample.is_mutating());
            assert_ne!(sample.is_mutating(), sample.is_read_only());
        }

        // §16 names these verbs explicitly.
        assert!(ControlRequest::List { all: false }.is_read_only());
        assert!(
            ControlRequest::Logs {
                dataflow: DataflowId::from_u128(1),
                node: None,
                query: LogQuery::new(),
            }
            .is_read_only()
        );
        assert!(
            ControlRequest::TopicSubscribe {
                dataflow: DataflowId::from_u128(1),
                port: "camera/image".parse().unwrap(),
                query: TopicQuery::new(),
                subscription: SubscriptionId::FIRST,
            }
            .is_read_only()
        );
        assert!(
            ControlRequest::Start {
                source: DataflowSource::from_manifest("nodes: []"),
                name: None,
                detach: false,
            }
            .is_mutating()
        );
        assert!(
            ControlRequest::Stop {
                dataflow: DataflowId::from_u128(1),
                grace: None,
            }
            .is_mutating()
        );
        assert!(
            ControlRequest::SetParam {
                scope: ParamScope::Global,
                key: ParamKey::new("gain").unwrap(),
                value: Parameter::Integer(1),
                create_only: false,
            }
            .is_mutating()
        );
    }

    #[test]
    fn the_read_and_mutate_sets_partition_the_family() {
        let mut read = 0usize;
        let mut mutate = 0usize;
        for sample in control_requests().unwrap() {
            if sample.is_mutating() {
                mutate += 1;
            } else {
                read += 1;
            }
        }
        assert_eq!(read + mutate, ControlRequest::VARIANT_NAMES.len());
        assert!(read > 0 && mutate > 0);
    }

    #[test]
    fn get_manifest_is_read_only_and_addresses_its_dataflow() {
        let request = ControlRequest::GetManifest {
            dataflow: DataflowId::from_u128(1),
        };
        assert!(request.is_read_only());
        assert_eq!(request.dataflow(), Some(DataflowId::from_u128(1)));
        assert!(request.to_string().contains("manifest"));
    }

    #[test]
    fn get_node_metrics_is_a_read_only_poll_not_a_subscription() {
        let request = ControlRequest::GetNodeMetrics {
            dataflow: DataflowId::from_u128(1),
            node: None,
        };
        assert!(request.is_read_only());
        assert_eq!(request.dataflow(), Some(DataflowId::from_u128(1)));
        assert_eq!(request.subscription(), None);
        assert!(!request.opens_subscription());
        assert!(request.to_string().contains("metrics"));
    }

    #[test]
    fn a_publish_is_mutating_even_though_it_reads_like_a_topic_verb() {
        // §16 groups "topic" with the read verbs, but publishing injects data
        // into a running graph, which is a change by any measure.
        let publish = ControlRequest::TopicPublish {
            dataflow: DataflowId::from_u128(1),
            port: "camera/image".parse().unwrap(),
            metadata: Metadata::new(HlcTimestamp::new(1, 0)),
            payload: vec![1, 2, 3],
        };
        assert!(publish.is_mutating());
        assert_eq!(publish.payload_len(), 3);
    }

    #[test]
    fn the_handshake_is_recognised_and_grants_nothing() {
        let hello = Hello::new(Role::Cli, AuthToken::ZERO);
        let request = ControlRequest::Hello(hello.clone());
        assert!(request.is_handshake());
        assert_eq!(request.hello(), Some(&hello));
        assert!(request.is_read_only());
        assert!(!ControlRequest::List { all: true }.is_handshake());
        assert_eq!(ControlRequest::List { all: true }.hello(), None);
    }

    #[test]
    fn dataflow_addressing_is_reported_where_it_exists() {
        let id = DataflowId::from_u128(0x1234);
        assert_eq!(
            ControlRequest::Stop {
                dataflow: id,
                grace: None
            }
            .dataflow(),
            Some(id)
        );
        assert_eq!(
            ControlRequest::Check { dataflow: Some(id) }.dataflow(),
            Some(id)
        );
        assert_eq!(ControlRequest::Check { dataflow: None }.dataflow(), None);
        assert_eq!(
            ControlRequest::GetParam {
                scope: ParamScope::dataflow_scope(id),
                key: ParamKey::new("gain").unwrap(),
                inherited: true,
            }
            .dataflow(),
            Some(id)
        );
        assert_eq!(
            ControlRequest::StopByName {
                name: "demo".to_owned(),
                grace: None
            }
            .dataflow(),
            None,
            "resolving a name needs the coordinator's registry"
        );
        assert_eq!(ControlRequest::Destroy { force: true }.dataflow(), None);
    }

    #[test]
    fn subscription_accessors_cover_the_three_subscription_verbs() {
        let subscription = SubscriptionId::new(11);
        let subscribe = ControlRequest::TopicSubscribe {
            dataflow: DataflowId::from_u128(1),
            port: "camera/image".parse().unwrap(),
            query: TopicQuery::new(),
            subscription,
        };
        assert_eq!(subscribe.subscription(), Some(subscription));
        assert!(subscribe.opens_subscription());

        let unsubscribe = ControlRequest::TopicUnsubscribe { subscription };
        assert_eq!(unsubscribe.subscription(), Some(subscription));
        assert!(!unsubscribe.opens_subscription());

        assert_eq!(ControlRequest::List { all: false }.subscription(), None);
    }

    #[test]
    fn blocking_verbs_expose_their_deadlines() {
        let build = ControlRequest::WaitForBuild {
            build: BuildId::from_u128(1),
            timeout: Some(DurationMs::from_secs(30)),
        };
        assert!(build.is_blocking());
        assert_eq!(build.timeout(), Some(DurationMs::from_secs(30)));

        let list = ControlRequest::List { all: false };
        assert!(!list.is_blocking());
        assert_eq!(list.timeout(), None);
    }

    #[test]
    fn a_spawn_spec_survives_the_wire_intact() {
        let spec = NodeSpawnSpec::new(
            DataflowId::from_u128(1),
            NodeId::new("camera").unwrap(),
            3,
            NodeSource::Executable {
                path: "./target/release/camera".to_owned(),
            },
        );
        let request = ControlRequest::AddNode {
            dataflow: DataflowId::from_u128(1),
            node: Box::new(spec.clone()),
            start: true,
        };
        let decoded = round_trip(&request).unwrap();
        match decoded {
            ControlRequest::AddNode { node, start, .. } => {
                assert_eq!(*node, spec);
                assert!(start);
            }
            other => panic!("expected AddNode, got {other:?}"),
        }
    }

    #[test]
    fn nan_parameters_survive_the_wire() {
        let request = ControlRequest::SetParam {
            scope: ParamScope::Global,
            key: ParamKey::new("gain").unwrap(),
            value: Parameter::Float(f64::NAN),
            create_only: false,
        };
        let decoded = round_trip(&request).unwrap();
        assert_ne!(decoded, request);
        assert!(decoded.bitwise_eq(&request));
    }

    #[test]
    fn display_names_every_verb_without_panicking() {
        for sample in control_requests().unwrap() {
            assert!(
                !sample.to_string().is_empty(),
                "{} rendered empty",
                sample.variant_name()
            );
        }
    }

    #[test]
    fn serde_round_trips_the_family_for_diagnostics() {
        for sample in control_requests().unwrap() {
            let json = serde_json::to_string(&sample).unwrap();
            let back: ControlRequest = serde_json::from_str(&json).unwrap();
            assert!(back.bitwise_eq(&sample));
        }
    }
}
