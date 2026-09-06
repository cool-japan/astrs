//! The spawn specification: everything a daemon needs to start one node.
//!
//! Blueprint §8.3 lists the manifest's node fields; by the time a node reaches
//! a daemon the *manifest* concerns are gone — `git:`/`module:`/`build:` have
//! been resolved to an executable, `$VAR` expansion has happened, modules have
//! been flattened into `parent.child` ids. What crosses the wire is the
//! resolved form: [`NodeSpawnSpec`].
//!
//! Two fields deserve their own note:
//!
//! - **`generation`** (blueprint §3.5, §6.2). Every incarnation of a node gets
//!   a fresh generation counter. Shared-memory segments embed it
//!   (`{dataflow_id}/{node_id}/{generation}`), so a message from a previous
//!   incarnation is detectable by every reader rather than silently accepted.
//! - **`env`** is the *daemon-applied* environment, already filtered against
//!   the §16 denylist. The daemon applies its own variables last, so a
//!   manifest can never override the handshake blob.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{DataId, DataflowId, NodeId, NodeSource, NodeSpawnSpec, OutputSpec};
//!
//! let spec = NodeSpawnSpec::new(
//!     DataflowId::from_u128(1),
//!     NodeId::new("camera")?,
//!     1,
//!     NodeSource::Executable { path: "./target/release/camera".into() },
//! )
//! .with_output(OutputSpec::new(DataId::new("image")?));
//!
//! assert_eq!(spec.generation, 1);
//! assert_eq!(spec.outputs.len(), 1);
//! assert!(spec.inputs.is_empty());
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use core::fmt;
use std::collections::BTreeMap;

use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::common::duration::DurationMs;
use crate::common::log::LogLevel;
use crate::ids::{DataId, DataflowId, MachineName, NodeId, OperatorId, PortRef, TypeUrn};
use crate::metadata::Parameter;

/// The default input queue size (blueprint §24.2).
pub const DEFAULT_QUEUE_SIZE: u32 = 10;

/// The default shared-memory pool per output: 8 MiB (blueprint §24.2).
pub const DEFAULT_SHM_POOL_SIZE: u64 = 8 * 1024 * 1024;

/// What a daemon should actually execute for a node.
///
/// Manifest-level sources (`git:`, `module:`, `build:`) are resolved before
/// this crosses the wire — the daemon receives a decision, not a recipe.
///
/// # Examples
///
/// ```
/// use astrs_wire::NodeSource;
///
/// let source = NodeSource::Executable { path: "./bin/node".into() };
/// assert!(source.is_spawned());
/// assert!(!NodeSource::Dynamic.is_spawned());
/// ```
// No `Eq`: the `Runtime` variant carries `OperatorSpec`, whose `config` values
// are `Parameter`s that may hold `f64` — see `Parameter`'s own note on why the
// float keeps this family at `PartialEq` only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NodeSource {
    /// Run this executable. The path is already resolved against the
    /// dataflow's working directory.
    #[oxicode(variant = 0)]
    Executable {
        /// Path to the binary.
        path: String,
    },
    /// Host these operators inside an `astrs-runtime` process (§9.3).
    #[oxicode(variant = 1)]
    Runtime {
        /// The operators to register, in declaration order.
        operators: Vec<OperatorSpec>,
    },
    /// The node attaches itself (`path: dynamic`, §8.3); the daemon spawns
    /// nothing and waits for a [`crate::NodeRequest::Register`].
    #[oxicode(variant = 2)]
    Dynamic,
    /// A declarative ROS 2 bridge node (§10.5).
    ///
    /// The bridge configuration is carried as an opaque string because its
    /// schema belongs to `astrs-ros2`, which this crate must not depend on.
    #[oxicode(variant = 3)]
    Ros2Bridge {
        /// The serialized bridge configuration.
        config: String,
    },
    /// A recorder node (`record:` sugar, §14).
    #[oxicode(variant = 4)]
    Recorder {
        /// Destination `.arec` path.
        path: String,
    },
}

impl NodeSource {
    /// Whether the daemon spawns an OS process for this source.
    #[must_use]
    pub const fn is_spawned(&self) -> bool {
        !matches!(self, Self::Dynamic)
    }

    /// A stable, lower-case name for logs and metrics labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Executable { .. } => "executable",
            Self::Runtime { .. } => "runtime",
            Self::Dynamic => "dynamic",
            Self::Ros2Bridge { .. } => "ros2_bridge",
            Self::Recorder { .. } => "recorder",
        }
    }
}

/// One operator hosted inside a runtime node (§9.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct OperatorSpec {
    /// The operator's id within its host node.
    pub id: OperatorId,
    /// The registry name the `register_operator!` macro published it under.
    pub registry_name: String,
    /// Static configuration handed to the operator at construction.
    pub config: BTreeMap<String, Parameter>,
}

impl OperatorSpec {
    /// An operator with no configuration.
    #[must_use]
    pub fn new(id: OperatorId, registry_name: impl Into<String>) -> Self {
        Self {
            id,
            registry_name: registry_name.into(),
            config: BTreeMap::new(),
        }
    }
}

/// What to do with a message when an input queue is full.
///
/// Blueprint §11.2. Correlated messages are immune to eviction under either
/// policy — see [`crate::Metadata::is_correlated`].
///
/// # Examples
///
/// ```
/// use astrs_wire::QueuePolicy;
///
/// assert_eq!(QueuePolicy::default(), QueuePolicy::DropOldest);
/// assert!(QueuePolicy::Backpressure.buffers_beyond_capacity());
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
pub enum QueuePolicy {
    /// Evict the oldest queued message to make room. The default.
    #[default]
    #[oxicode(variant = 0)]
    DropOldest,
    /// Buffer up to ten times `queue_size`, then drop with an `ERROR` log and
    /// a metric.
    #[oxicode(variant = 1)]
    Backpressure,
}

impl QueuePolicy {
    /// Every policy, in variant order.
    pub const ALL: &'static [Self] = &[Self::DropOldest, Self::Backpressure];

    /// Whether the policy buffers past the nominal capacity before dropping.
    #[must_use]
    pub const fn buffers_beyond_capacity(self) -> bool {
        matches!(self, Self::Backpressure)
    }

    /// The over-capacity multiplier the policy allows (§11.2: ten times).
    #[must_use]
    pub const fn overflow_multiplier(self) -> u32 {
        match self {
            Self::DropOldest => 1,
            Self::Backpressure => 10,
        }
    }

    /// A stable, lower-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DropOldest => "drop_oldest",
            Self::Backpressure => "backpressure",
        }
    }
}

impl fmt::Display for QueuePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which lane an input is delivered on inside the merged event loop.
///
/// Blueprint §11.3: the control lane pre-empts the data lane, so a `Stop` or a
/// parameter update is not stuck behind a queue of camera frames.
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
pub enum PriorityLane {
    /// Ordinary data. The default.
    #[default]
    #[oxicode(variant = 0)]
    Data,
    /// Pre-empts the data lane within the merged loop.
    #[oxicode(variant = 1)]
    Control,
}

/// The service/action role a node's ports implement (§9.4).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Encode, Decode,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum NodePattern {
    /// Serves service requests.
    #[oxicode(variant = 0)]
    ServiceServer,
    /// Issues service requests.
    #[oxicode(variant = 1)]
    ServiceClient,
    /// Serves action goals.
    #[oxicode(variant = 2)]
    ActionServer,
    /// Issues action goals.
    #[oxicode(variant = 3)]
    ActionClient,
}

impl NodePattern {
    /// Every pattern, in variant order.
    pub const ALL: &'static [Self] = &[
        Self::ServiceServer,
        Self::ServiceClient,
        Self::ActionServer,
        Self::ActionClient,
    ];

    /// Whether this side serves requests rather than issuing them.
    #[must_use]
    pub const fn is_server(self) -> bool {
        matches!(self, Self::ServiceServer | Self::ActionServer)
    }

    /// The manifest spelling, e.g. `service-server`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ServiceServer => "service-server",
            Self::ServiceClient => "service-client",
            Self::ActionServer => "action-server",
            Self::ActionClient => "action-client",
        }
    }
}

impl fmt::Display for NodePattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// When a node should be respawned after it exits (§8.3, §12).
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
pub enum RestartPolicy {
    /// Never respawn. The default.
    #[default]
    #[oxicode(variant = 0)]
    Never,
    /// Respawn only after a non-zero exit or a signal.
    #[oxicode(variant = 1)]
    OnFailure,
    /// Respawn after any exit, successful or not.
    #[oxicode(variant = 2)]
    Always,
}

impl RestartPolicy {
    /// Every policy, in variant order.
    pub const ALL: &'static [Self] = &[Self::Never, Self::OnFailure, Self::Always];

    /// Whether a node that exited with `success` should be respawned.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::RestartPolicy;
    ///
    /// assert!(!RestartPolicy::Never.should_restart(false));
    /// assert!(RestartPolicy::OnFailure.should_restart(false));
    /// assert!(!RestartPolicy::OnFailure.should_restart(true));
    /// assert!(RestartPolicy::Always.should_restart(true));
    /// ```
    #[must_use]
    pub const fn should_restart(self, success: bool) -> bool {
        match self {
            Self::Never => false,
            Self::OnFailure => !success,
            Self::Always => true,
        }
    }

    /// A stable, lower-case name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::OnFailure => "on_failure",
            Self::Always => "always",
        }
    }
}

impl fmt::Display for RestartPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The full restart budget for one node (§12).
///
/// # Examples
///
/// ```
/// use astrs_wire::{DurationMs, RestartConfig, RestartPolicy};
///
/// let config = RestartConfig {
///     policy: RestartPolicy::OnFailure,
///     max_restarts: Some(3),
///     restart_delay: DurationMs::new(100),
///     max_restart_delay: DurationMs::from_secs(30),
///     restart_window: DurationMs::from_secs(60),
/// };
///
/// // Exponential backoff, capped.
/// assert_eq!(config.backoff_for(0), DurationMs::new(100));
/// assert_eq!(config.backoff_for(3), DurationMs::new(800));
/// assert_eq!(config.backoff_for(64), DurationMs::from_secs(30));
/// assert!(config.budget_exhausted(3));
/// assert!(!config.budget_exhausted(2));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct RestartConfig {
    /// When to respawn.
    pub policy: RestartPolicy,
    /// How many respawns are permitted inside `restart_window`; `None` for
    /// unlimited.
    pub max_restarts: Option<u32>,
    /// The base delay before the first respawn.
    pub restart_delay: DurationMs,
    /// The cap on the exponentially-growing delay.
    pub max_restart_delay: DurationMs,
    /// The sliding window over which `max_restarts` is counted.
    pub restart_window: DurationMs,
}

impl RestartConfig {
    /// The default: never restart.
    #[must_use]
    pub const fn never() -> Self {
        Self {
            policy: RestartPolicy::Never,
            max_restarts: None,
            restart_delay: DurationMs::new(100),
            max_restart_delay: DurationMs::new(30_000),
            restart_window: DurationMs::new(60_000),
        }
    }

    /// The delay before the respawn that follows `restarts_so_far` restarts.
    ///
    /// `restart_delay × 2^n`, capped at `max_restart_delay` (§12). Overflow
    /// saturates at the cap rather than wrapping to a spin.
    #[must_use]
    pub fn backoff_for(&self, restarts_so_far: u32) -> DurationMs {
        let factor = 1u64.checked_shl(restarts_so_far).unwrap_or(u64::MAX);
        self.restart_delay
            .checked_mul(factor)
            .unwrap_or(self.max_restart_delay)
            .min(self.max_restart_delay)
    }

    /// Whether `restarts_in_window` has used up the budget.
    #[must_use]
    pub const fn budget_exhausted(&self, restarts_in_window: u32) -> bool {
        match self.max_restarts {
            Some(max) => restarts_in_window >= max,
            None => false,
        }
    }
}

impl Default for RestartConfig {
    fn default() -> Self {
        Self::never()
    }
}

/// Per-node logging configuration (§8.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct LogConfig {
    /// Republish the node's stdout as this output port, if set.
    pub send_stdout_as: Option<DataId>,
    /// Drop records below this level before they leave the node.
    pub min_log_level: LogLevel,
    /// Rotate the node's log file at this size, in bytes.
    pub max_log_size: Option<u64>,
    /// Keep at most this many rotated files.
    pub max_rotated_files: Option<u32>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            send_stdout_as: None,
            min_log_level: LogLevel::Info,
            max_log_size: None,
            max_rotated_files: None,
        }
    }
}

/// Where a node runs and under what labels (§8.3 `deploy:`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct DeploySpec {
    /// The machine the node is pinned to; `None` lets the placement planner
    /// choose.
    pub machine: Option<MachineName>,
    /// Placement constraints matched against daemon labels.
    pub labels: BTreeMap<String, String>,
    /// A working directory overriding the dataflow's.
    pub working_dir: Option<String>,
}

/// One input port of a node, fully resolved (§8.3 long form).
///
/// # Examples
///
/// ```
/// use astrs_wire::{DataId, InputSpec, PortRef, QueuePolicy};
///
/// let input = InputSpec::new(DataId::new("frames")?, PortRef::from_parts("camera", "image")?)
///     .with_queue(4, QueuePolicy::Backpressure);
/// assert_eq!(input.queue_size, 4);
/// assert_eq!(input.effective_capacity(), 40);
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct InputSpec {
    /// The port name as this node sees it.
    pub id: DataId,
    /// The producing port.
    pub source: PortRef,
    /// The nominal queue depth.
    pub queue_size: u32,
    /// What to do when the queue is full.
    pub queue_policy: QueuePolicy,
    /// Declare the input closed if nothing arrives for this long.
    pub timeout: Option<DurationMs>,
    /// The lane this input is delivered on.
    pub priority_lane: PriorityLane,
    /// The declared type of the port, if any.
    pub type_urn: Option<TypeUrn>,
    /// The input-to-output latency budget monitored for this port (§11.3).
    pub deadline: Option<DurationMs>,
}

impl InputSpec {
    /// An input with the default queue settings.
    #[must_use]
    pub fn new(id: DataId, source: PortRef) -> Self {
        Self {
            id,
            source,
            queue_size: DEFAULT_QUEUE_SIZE,
            queue_policy: QueuePolicy::DropOldest,
            timeout: None,
            priority_lane: PriorityLane::Data,
            type_urn: None,
            deadline: None,
        }
    }

    /// Sets the queue depth and policy.
    #[must_use]
    pub const fn with_queue(mut self, size: u32, policy: QueuePolicy) -> Self {
        self.queue_size = size;
        self.queue_policy = policy;
        self
    }

    /// Sets the declared port type.
    #[must_use]
    pub fn with_type(mut self, urn: TypeUrn) -> Self {
        self.type_urn = Some(urn);
        self
    }

    /// The real buffering capacity, accounting for the policy's overflow
    /// allowance (§11.2).
    #[must_use]
    pub const fn effective_capacity(&self) -> u32 {
        self.queue_size
            .saturating_mul(self.queue_policy.overflow_multiplier())
    }
}

/// One output port of a node, fully resolved (§8.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Encode, Decode)]
pub struct OutputSpec {
    /// The port name.
    pub id: DataId,
    /// The declared type of the port, if any.
    pub type_urn: Option<TypeUrn>,
    /// The shared-memory pool size for this output, in bytes.
    pub shm_pool_size: Option<u64>,
}

impl OutputSpec {
    /// An output with no declared type and the default pool size.
    #[must_use]
    pub const fn new(id: DataId) -> Self {
        Self {
            id,
            type_urn: None,
            shm_pool_size: None,
        }
    }

    /// Sets the declared port type.
    #[must_use]
    pub fn with_type(mut self, urn: TypeUrn) -> Self {
        self.type_urn = Some(urn);
        self
    }

    /// The pool size to use, falling back to [`DEFAULT_SHM_POOL_SIZE`].
    #[must_use]
    pub const fn pool_size(&self) -> u64 {
        match self.shm_pool_size {
            Some(size) => size,
            None => DEFAULT_SHM_POOL_SIZE,
        }
    }
}

/// Everything a daemon needs to spawn and supervise one node.
///
/// # Examples
///
/// ```
/// use astrs_wire::{DataflowId, NodeId, NodeSource, NodeSpawnSpec};
///
/// let spec = NodeSpawnSpec::new(
///     DataflowId::from_u128(1),
///     NodeId::new("planner")?,
///     3,
///     NodeSource::Dynamic,
/// );
/// assert!(!spec.source.is_spawned());
/// assert_eq!(spec.segment_prefix(), format!("{}/planner/3", DataflowId::from_u128(1)));
/// # Ok::<(), astrs_wire::IdError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct NodeSpawnSpec {
    /// The dataflow this node belongs to.
    pub dataflow: DataflowId,
    /// The node's id.
    pub node: NodeId,
    /// The incarnation counter — bumped on every restart (§3.5).
    pub generation: u64,
    /// What to execute.
    pub source: NodeSource,
    /// Command-line arguments, already split by shlex rules (§16: no shell).
    pub args: Vec<String>,
    /// Environment variables the daemon applies, already filtered against the
    /// §16 denylist.
    pub env: BTreeMap<String, String>,
    /// The working directory to spawn in.
    pub working_dir: Option<String>,
    /// The node's input ports.
    pub inputs: Vec<InputSpec>,
    /// The node's output ports.
    pub outputs: Vec<OutputSpec>,
    /// Restart budget and policy.
    pub restart: RestartConfig,
    /// Logging configuration.
    pub logging: LogConfig,
    /// Placement.
    pub deploy: DeploySpec,
    /// CPU cores this node is pinned to; empty means unpinned (§11.3).
    pub cpu_affinity: Vec<u16>,
    /// How long the node may take to register before the spawn deadline fires
    /// (§12: dora's gap, closed).
    pub spawn_deadline: Option<DurationMs>,
    /// How long a registered node may go without a liveness reply.
    pub health_check_timeout: Option<DurationMs>,
    /// How long to wait after `SIGTERM` before escalating to `SIGKILL`.
    pub finish_grace: Option<DurationMs>,
    /// The service/action role this node's ports implement.
    pub pattern: Option<NodePattern>,
}

impl NodeSpawnSpec {
    /// A spawn spec with default supervision settings and no ports.
    #[must_use]
    pub fn new(dataflow: DataflowId, node: NodeId, generation: u64, source: NodeSource) -> Self {
        Self {
            dataflow,
            node,
            generation,
            source,
            args: Vec::new(),
            env: BTreeMap::new(),
            working_dir: None,
            inputs: Vec::new(),
            outputs: Vec::new(),
            restart: RestartConfig::never(),
            logging: LogConfig::default(),
            deploy: DeploySpec::default(),
            cpu_affinity: Vec::new(),
            spawn_deadline: None,
            health_check_timeout: None,
            finish_grace: None,
            pattern: None,
        }
    }

    /// Adds an input port.
    #[must_use]
    pub fn with_input(mut self, input: InputSpec) -> Self {
        self.inputs.push(input);
        self
    }

    /// Adds an output port.
    #[must_use]
    pub fn with_output(mut self, output: OutputSpec) -> Self {
        self.outputs.push(output);
        self
    }

    /// Sets the restart configuration.
    #[must_use]
    pub const fn with_restart(mut self, restart: RestartConfig) -> Self {
        self.restart = restart;
        self
    }

    /// Looks up an input port by id.
    #[must_use]
    pub fn input(&self, id: &DataId) -> Option<&InputSpec> {
        self.inputs.iter().find(|input| &input.id == id)
    }

    /// Looks up an output port by id.
    #[must_use]
    pub fn output(&self, id: &DataId) -> Option<&OutputSpec> {
        self.outputs.iter().find(|output| &output.id == id)
    }

    /// The shared-memory segment name prefix for this incarnation:
    /// `{dataflow_id}/{node_id}/{generation}` (blueprint §6.2).
    ///
    /// Building it here — beside the generation field it depends on — keeps
    /// the daemon and the node API from inventing two spellings of the same
    /// name.
    #[must_use]
    pub fn segment_prefix(&self) -> String {
        format!("{}/{}/{}", self.dataflow, self.node, self.generation)
    }

    /// Whether this is a restart rather than a first start.
    ///
    /// Generation `1` is the first incarnation; anything higher is a respawn,
    /// which is what `Node::is_restart()` reports to user code (§9.1).
    #[must_use]
    pub const fn is_restart(&self) -> bool {
        self.generation > 1
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::round_trip;

    fn spec() -> NodeSpawnSpec {
        NodeSpawnSpec::new(
            DataflowId::from_u128(0x11),
            NodeId::new("detector").unwrap(),
            2,
            NodeSource::Executable {
                path: "./bin/detector".into(),
            },
        )
        .with_input(
            InputSpec::new(
                DataId::new("frames").unwrap(),
                PortRef::from_parts("camera", "image").unwrap(),
            )
            .with_queue(4, QueuePolicy::Backpressure)
            .with_type(TypeUrn::new("std/media/v1/Image").unwrap()),
        )
        .with_output(
            OutputSpec::new(DataId::new("boxes").unwrap())
                .with_type(TypeUrn::new("std/vision/v1/Detections").unwrap()),
        )
        .with_restart(RestartConfig {
            policy: RestartPolicy::OnFailure,
            max_restarts: Some(5),
            restart_delay: DurationMs::new(100),
            max_restart_delay: DurationMs::from_secs(30),
            restart_window: DurationMs::from_secs(60),
        })
    }

    #[test]
    fn spawn_specs_round_trip() {
        let spec = spec();
        assert_eq!(round_trip(&spec).unwrap(), spec);
    }

    #[test]
    fn a_minimal_spawn_spec_round_trips() {
        let spec = NodeSpawnSpec::new(
            DataflowId::NIL,
            NodeId::new("n").unwrap(),
            1,
            NodeSource::Dynamic,
        );
        assert_eq!(round_trip(&spec).unwrap(), spec);
    }

    #[test]
    fn every_node_source_round_trips_and_names_itself() {
        let sources = [
            NodeSource::Executable { path: "p".into() },
            NodeSource::Runtime {
                operators: vec![OperatorSpec::new(
                    OperatorId::new("op").unwrap(),
                    "crate::Op",
                )],
            },
            NodeSource::Dynamic,
            NodeSource::Ros2Bridge {
                config: "{}".into(),
            },
            NodeSource::Recorder {
                path: "out.arec".into(),
            },
        ];
        let mut names = std::collections::BTreeSet::new();
        for source in sources {
            assert!(names.insert(source.kind_name()));
            assert_eq!(round_trip(&source).unwrap(), source);
        }
        assert_eq!(names.len(), 5);
    }

    #[test]
    fn only_dynamic_sources_are_unspawned() {
        assert!(!NodeSource::Dynamic.is_spawned());
        assert!(NodeSource::Executable { path: "p".into() }.is_spawned());
        assert!(NodeSource::Recorder { path: "p".into() }.is_spawned());
    }

    #[test]
    fn port_lookup_finds_declared_ports() {
        let spec = spec();
        assert!(spec.input(&DataId::new("frames").unwrap()).is_some());
        assert!(spec.input(&DataId::new("absent").unwrap()).is_none());
        assert!(spec.output(&DataId::new("boxes").unwrap()).is_some());
        assert!(spec.output(&DataId::new("absent").unwrap()).is_none());
    }

    #[test]
    fn segment_prefix_matches_the_documented_shape() {
        let spec = spec();
        assert_eq!(
            spec.segment_prefix(),
            format!("{}/detector/2", DataflowId::from_u128(0x11))
        );
        assert!(spec.is_restart());

        let first = NodeSpawnSpec::new(
            DataflowId::NIL,
            NodeId::new("n").unwrap(),
            1,
            NodeSource::Dynamic,
        );
        assert!(!first.is_restart());
    }

    #[test]
    fn queue_policies_describe_their_capacity() {
        assert_eq!(QueuePolicy::DropOldest.overflow_multiplier(), 1);
        assert_eq!(QueuePolicy::Backpressure.overflow_multiplier(), 10);
        assert!(!QueuePolicy::DropOldest.buffers_beyond_capacity());
        assert!(QueuePolicy::Backpressure.buffers_beyond_capacity());
        assert_eq!(QueuePolicy::DropOldest.to_string(), "drop_oldest");
        assert_eq!(QueuePolicy::Backpressure.to_string(), "backpressure");
    }

    #[test]
    fn effective_capacity_reflects_the_policy() {
        let source = PortRef::from_parts("a", "b").unwrap();
        let id = DataId::new("in").unwrap();
        let dropping = InputSpec::new(id.clone(), source.clone());
        assert_eq!(dropping.queue_size, DEFAULT_QUEUE_SIZE);
        assert_eq!(dropping.effective_capacity(), DEFAULT_QUEUE_SIZE);

        let buffering = InputSpec::new(id, source).with_queue(7, QueuePolicy::Backpressure);
        assert_eq!(buffering.effective_capacity(), 70);
    }

    #[test]
    fn effective_capacity_saturates() {
        let input = InputSpec::new(
            DataId::new("in").unwrap(),
            PortRef::from_parts("a", "b").unwrap(),
        )
        .with_queue(u32::MAX, QueuePolicy::Backpressure);
        assert_eq!(input.effective_capacity(), u32::MAX);
    }

    #[test]
    fn output_pool_size_falls_back_to_the_default() {
        let output = OutputSpec::new(DataId::new("o").unwrap());
        assert_eq!(output.pool_size(), DEFAULT_SHM_POOL_SIZE);

        let sized = OutputSpec {
            shm_pool_size: Some(4096),
            ..OutputSpec::new(DataId::new("o").unwrap())
        };
        assert_eq!(sized.pool_size(), 4096);
    }

    #[test]
    fn restart_policies_decide_correctly() {
        for &policy in RestartPolicy::ALL {
            assert_eq!(round_trip(&policy).unwrap(), policy);
        }
        assert!(!RestartPolicy::Never.should_restart(true));
        assert!(!RestartPolicy::Never.should_restart(false));
        assert!(!RestartPolicy::OnFailure.should_restart(true));
        assert!(RestartPolicy::OnFailure.should_restart(false));
        assert!(RestartPolicy::Always.should_restart(true));
        assert!(RestartPolicy::Always.should_restart(false));
        assert_eq!(RestartPolicy::default(), RestartPolicy::Never);
        assert_eq!(RestartPolicy::OnFailure.to_string(), "on_failure");
    }

    #[test]
    fn backoff_doubles_then_caps() {
        let config = RestartConfig {
            policy: RestartPolicy::Always,
            max_restarts: Some(2),
            restart_delay: DurationMs::new(50),
            max_restart_delay: DurationMs::new(1_000),
            restart_window: DurationMs::from_secs(60),
        };
        assert_eq!(config.backoff_for(0), DurationMs::new(50));
        assert_eq!(config.backoff_for(1), DurationMs::new(100));
        assert_eq!(config.backoff_for(2), DurationMs::new(200));
        assert_eq!(config.backoff_for(4), DurationMs::new(800));
        assert_eq!(config.backoff_for(5), DurationMs::new(1_000));
        // Far past the shift width — must cap, not wrap to something tiny.
        for restarts in [31u32, 32, 63, 64, 100, u32::MAX] {
            assert_eq!(config.backoff_for(restarts), DurationMs::new(1_000));
        }
    }

    #[test]
    fn budget_exhaustion_respects_unlimited() {
        let mut config = RestartConfig::never();
        config.max_restarts = None;
        assert!(!config.budget_exhausted(u32::MAX));

        config.max_restarts = Some(0);
        assert!(config.budget_exhausted(0));

        config.max_restarts = Some(3);
        assert!(!config.budget_exhausted(2));
        assert!(config.budget_exhausted(3));
        assert!(config.budget_exhausted(4));
    }

    #[test]
    fn defaults_are_conservative() {
        assert_eq!(RestartConfig::default(), RestartConfig::never());
        assert_eq!(RestartConfig::never().policy, RestartPolicy::Never);
        assert_eq!(LogConfig::default().min_log_level, LogLevel::Info);
        assert!(LogConfig::default().send_stdout_as.is_none());
        assert!(DeploySpec::default().machine.is_none());
        assert_eq!(PriorityLane::default(), PriorityLane::Data);
    }

    #[test]
    fn patterns_name_themselves_and_split_by_role() {
        for pattern in NodePattern::ALL.iter().copied() {
            assert_eq!(round_trip(&pattern).unwrap(), pattern);
            assert_eq!(pattern.to_string(), pattern.as_str());
            assert!(pattern.as_str().contains('-'));
        }
        assert!(NodePattern::ServiceServer.is_server());
        assert!(NodePattern::ActionServer.is_server());
        assert!(!NodePattern::ServiceClient.is_server());
        assert!(!NodePattern::ActionClient.is_server());
    }

    #[test]
    fn deploy_and_log_config_round_trip() {
        let deploy = DeploySpec {
            machine: Some(MachineName::new("robot-1").unwrap()),
            labels: BTreeMap::from([("gpu".to_owned(), "true".to_owned())]),
            working_dir: Some("/tmp/x".to_owned()),
        };
        assert_eq!(round_trip(&deploy).unwrap(), deploy);

        let logging = LogConfig {
            send_stdout_as: Some(DataId::new("stdout").unwrap()),
            min_log_level: LogLevel::Debug,
            max_log_size: Some(1 << 20),
            max_rotated_files: Some(3),
        };
        assert_eq!(round_trip(&logging).unwrap(), logging);
    }

    #[test]
    fn operator_specs_round_trip_with_configuration() {
        let mut operator = OperatorSpec::new(OperatorId::new("nms").unwrap(), "vision::Nms");
        operator
            .config
            .insert("threshold".to_owned(), Parameter::Float(0.5));
        assert_eq!(round_trip(&operator).unwrap(), operator);
    }
}
