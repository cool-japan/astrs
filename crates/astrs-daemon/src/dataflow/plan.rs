//! Manifest + graph → the [`astrs_wire::NodeSpawnSpec`]s the daemon runs.
//!
//! The translation layer between what a user *wrote* (an
//! [`astrs_manifest::Manifest`], validated and module-expanded) and what the
//! daemon *executes* (one [`NodeSpawnSpec`] per node, with every field
//! resolved: queue sizes, restart budgets, log settings, virtual sources,
//! working directories). One place, so the manifest's defaults are applied
//! once and every consumer downstream sees the same resolved answer.
//!
//! ```text
//!   Manifest ──validate──► DataflowGraph ──plan_dataflow──► Vec<NodeSpawnSpec>
//!       │                       │                                  │
//!       │ per-node defaults     │ edges, machine placement         │ what the
//!       │ (§8.3)                │ (§5.2)                           │ spawner runs
//! ```
//!
//! # What is resolved here
//!
//! | Manifest field | Resolved to |
//! |---|---|
//! | `path:` / `git:` / `operators:` / `ros2:` / `record:` | [`astrs_wire::NodeSource`] |
//! | `inputs:` (short and long form) | [`astrs_wire::InputSpec`] with queue size, policy, timeout |
//! | `outputs:` + `output_types:` | [`astrs_wire::OutputSpec`] with its type URN |
//! | `restart_policy:` + the four timings | [`astrs_wire::RestartConfig`] |
//! | `send_stdout_as:` / `min_log_level:` / rotation | [`astrs_wire::LogConfig`] |
//! | `env:` (graph-wide + node) | expanded against the scrubbed base, deny-filtered |
//! | `astrs/...` input sources | the reserved virtual producer port (§8.4) |
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::dataflow::plan_dataflow;
//! use astrs_manifest::Manifest;
//! use astrs_wire::DataflowId;
//! use std::collections::BTreeMap;
//!
//! let yaml = "\
//! nodes:
//!   - id: camera
//!     path: ./camera
//!     outputs: [image]
//!   - id: detect
//!     path: ./detect
//!     inputs:
//!       frames: camera/image
//! ";
//! let manifest = Manifest::from_yaml_str(yaml)?;
//! let plan = plan_dataflow(DataflowId::from_u128(1), &manifest, &BTreeMap::new())?;
//!
//! assert_eq!(plan.specs.len(), 2);
//! assert_eq!(plan.specs[1].inputs.len(), 1);
//! assert_eq!(plan.specs[1].inputs[0].source.to_string(), "camera/image");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::BTreeMap;

use astrs_manifest::{
    Manifest, Node as ManifestNode, PriorityLane as ManifestPriorityLane,
    QueuePolicy as ManifestQueuePolicy, RestartPolicy as ManifestRestartPolicy, VirtualSource,
};
use astrs_wire::{
    DataId, DataflowId, DurationMs, InputSpec, LogConfig, LogLevel, NodeId, NodeSource,
    NodeSpawnSpec, OperatorSpec, OutputSpec, PortRef, PriorityLane, QueuePolicy, RestartConfig,
    RestartPolicy, TypeUrn,
};

use crate::error::{DaemonError, DaemonResult};
use crate::state::routes::virtual_port_ref;

/// One planned dataflow: what to spawn, and the virtual sources it needs.
#[derive(Debug, Clone)]
pub struct DataflowPlan {
    /// The dataflow's identifier.
    pub dataflow: DataflowId,
    /// Its name, from the manifest.
    pub name: Option<String>,
    /// One specification per node, in manifest order.
    pub specs: Vec<NodeSpawnSpec>,
    /// The `astrs/...` subscriptions the daemon must serve (§8.4).
    pub virtual_inputs: Vec<PlannedVirtualInput>,
    /// The build lines to run before spawning, in node order.
    pub build_steps: Vec<BuildStep>,
    /// Whether the dataflow stops itself when every node finishes (§8.2).
    pub exit_when_nodes_finish: bool,
    /// The health-check period the manifest asks for (§24.2).
    pub health_check_interval: DurationMs,
}

impl DataflowPlan {
    /// The specification for one node.
    #[must_use]
    pub fn spec(&self, node: &NodeId) -> Option<&NodeSpawnSpec> {
        self.specs.iter().find(|spec| spec.node == *node)
    }

    /// How many nodes the daemon will spawn (dynamic nodes excluded).
    #[must_use]
    pub fn spawn_count(&self) -> usize {
        self.specs
            .iter()
            .filter(|spec| spec.source.is_spawned())
            .count()
    }

    /// Whether anything needs building.
    #[must_use]
    pub fn needs_build(&self) -> bool {
        !self.build_steps.is_empty()
    }
}

/// One `astrs/...` subscription the daemon serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedVirtualInput {
    /// The subscribing node.
    pub node: NodeId,
    /// The input the ticks or records arrive on.
    pub input: DataId,
    /// The source, as written in the manifest.
    pub source: String,
    /// The parsed source.
    pub parsed: VirtualSource,
}

impl PlannedVirtualInput {
    /// Whether this is a timer subscription.
    #[must_use]
    pub const fn is_timer(&self) -> bool {
        matches!(
            self.parsed,
            VirtualSource::TimerMillis(_) | VirtualSource::TimerSecs(_) | VirtualSource::TimerHz(_)
        )
    }

    /// Whether this is a log subscription.
    #[must_use]
    pub const fn is_logs(&self) -> bool {
        matches!(self.parsed, VirtualSource::Logs { .. })
    }

    /// Whether this is the `astrs/status` lifecycle stream.
    #[must_use]
    pub const fn is_status(&self) -> bool {
        matches!(self.parsed, VirtualSource::Status)
    }
}

/// One node's `build:` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildStep {
    /// The node it belongs to.
    pub node: NodeId,
    /// The command line, unsplit.
    pub command: String,
    /// The directory to run it in, relative to the dataflow root.
    pub working_dir: Option<String>,
}

/// Plans `manifest` as `dataflow`, expanding `env:` against `base_env`.
///
/// `base_env` is the *scrubbed* environment (see [`crate::spawn::EnvPolicy`]),
/// never the daemon's raw one: expanding a manifest against an unscrubbed
/// environment would let it read exactly the variables §16 removed.
///
/// # Errors
///
/// - [`DaemonError::Manifest`] if the manifest is invalid, an input names a
///   malformed virtual source, or an id fails its grammar.
/// - [`DaemonError::BadEnv`] if a node's `env:` cannot be expanded.
pub fn plan_dataflow(
    dataflow: DataflowId,
    manifest: &Manifest,
    base_env: &BTreeMap<String, String>,
) -> DaemonResult<DataflowPlan> {
    manifest
        .validate()
        .map_err(|errors| DaemonError::Manifest(errors.to_string()))?;

    let mut specs = Vec::with_capacity(manifest.nodes.len());
    let mut virtual_inputs = Vec::new();
    let mut build_steps = Vec::new();

    for node in &manifest.nodes {
        let id = NodeId::new(node.id.clone())
            .map_err(|error| DaemonError::Manifest(format!("node id {}: {error}", node.id)))?;

        if let Some(command) = &node.build
            && !command.trim().is_empty()
        {
            build_steps.push(BuildStep {
                node: id.clone(),
                command: command.clone(),
                working_dir: node.working_dir.clone(),
            });
        }

        let mut spec = NodeSpawnSpec::new(dataflow, id.clone(), 0, source_for(node)?);
        spec.args = node.args.clone();
        spec.working_dir = node.working_dir.clone();
        spec.env = expand_env(node, manifest, base_env, &id)?;
        spec.restart = restart_config(node);
        spec.logging = log_config(node)?;
        spec.cpu_affinity = node
            .cpu_affinity
            .as_ref()
            .map(|cores| {
                cores
                    .iter()
                    .map(|core| u16::try_from(*core).unwrap_or(u16::MAX))
                    .collect()
            })
            .unwrap_or_default();
        spec.health_check_timeout = node
            .health_check_timeout
            .map(|secs| DurationMs::from_duration(duration_secs(secs)));
        spec.finish_grace = node
            .finish_grace_secs
            .map(|secs| DurationMs::from_duration(duration_secs(secs)));
        spec.pattern = node.pattern.map(pattern_for);

        for (name, input) in &node.inputs {
            spec.inputs.push(build_input_spec(
                &id,
                name,
                input,
                &node.input_types,
                &mut virtual_inputs,
            )?);
        }

        // `record:` sugar (§14) declares no `inputs:` of its own — a
        // node sourced this way is otherwise spawned with nothing wired
        // to it at all. `record_sugar_inputs` lowers it to the same
        // `_record_<i>` inputs `astrs-graph`'s independent graph-model
        // synthesis assigns them (see that method's docs for why the
        // separator differs), already skipping any index an explicit
        // `inputs:` entry claimed instead.
        for (name, input) in node.record_sugar_inputs() {
            spec.inputs.push(build_input_spec(
                &id,
                &name,
                &input,
                &node.input_types,
                &mut virtual_inputs,
            )?);
        }

        for name in &node.outputs {
            let output_id = DataId::new(name.clone()).map_err(|error| {
                DaemonError::Manifest(format!("node {id}: output {name}: {error}"))
            })?;
            let mut output = OutputSpec::new(output_id);
            output.type_urn = node
                .output_types
                .get(name)
                .and_then(|urn| TypeUrn::new(urn.to_string()).ok());
            output.shm_pool_size = node.shm_pool_size;
            spec.outputs.push(output);
        }

        specs.push(spec);
    }

    Ok(DataflowPlan {
        dataflow,
        name: manifest.name.clone(),
        specs,
        virtual_inputs,
        build_steps,
        exit_when_nodes_finish: manifest.exit_when_nodes_finish,
        health_check_interval: DurationMs::new(
            (manifest.health_check_interval * 1_000.0).max(1.0) as u64
        ),
    })
}

/// Resolves one input (hand-written, or synthesized from `record:` sugar
/// by [`astrs_manifest::Node::record_sugar_inputs`]) into an
/// [`InputSpec`], registering a virtual-source subscription along the
/// way when `input.source` names one.
///
/// Shared by both loops of [`plan_dataflow`] so a `record:`-sourced input
/// resolves through exactly the same virtual-source and queue-setting
/// logic an ordinary `inputs:` entry does — the two are indistinguishable
/// from this point on.
///
/// # Errors
///
/// [`DaemonError::Manifest`] if `name` is not a legal [`DataId`], if
/// `input.source` is a malformed virtual source, or if it is neither a
/// recognized virtual source nor a parsable [`PortRef`].
fn build_input_spec(
    id: &NodeId,
    name: &str,
    input: &astrs_manifest::Input,
    input_types: &BTreeMap<String, astrs_manifest::Urn>,
    virtual_inputs: &mut Vec<PlannedVirtualInput>,
) -> DaemonResult<InputSpec> {
    let input_id = DataId::new(name)
        .map_err(|error| DaemonError::Manifest(format!("node {id}: input {name}: {error}")))?;

    let mut is_status = false;
    let source = match astrs_manifest::recognize_virtual_source(&input.source) {
        Some(Ok(parsed)) => {
            is_status = matches!(parsed, VirtualSource::Status);
            virtual_inputs.push(PlannedVirtualInput {
                node: id.clone(),
                input: input_id.clone(),
                source: input.source.clone(),
                parsed,
            });
            virtual_port_ref(&input.source).map_err(|error| {
                DaemonError::Manifest(format!(
                    "node {id}: input {name}: {}: {error}",
                    input.source
                ))
            })?
        }
        Some(Err(error)) => {
            return Err(DaemonError::Manifest(format!(
                "node {id}: input {name}: {}: {error}",
                input.source
            )));
        }
        None => input.source.parse::<PortRef>().map_err(|error| {
            DaemonError::Manifest(format!(
                "node {id}: input {name}: {}: {error}",
                input.source
            ))
        })?,
    };

    let mut input_spec = InputSpec::new(input_id, source);
    input_spec.queue_size = input.queue_size.max(1);
    input_spec.queue_policy = queue_policy_for(input.queue_policy);
    input_spec.timeout = input
        .timeout
        .map(|secs| DurationMs::from_duration(duration_secs(secs)));
    input_spec.priority_lane = priority_lane_for(input.priority_lane, is_status);
    input_spec.deadline = input
        .deadline
        .map(|secs| DurationMs::from_duration(duration_secs(secs)));
    input_spec.type_urn = input_types
        .get(name)
        .and_then(|urn| TypeUrn::new(urn.to_string()).ok());
    Ok(input_spec)
}

/// Maps a manifest `priority_lane` to the wire lane, preserving the
/// automatic promotion the daemon already gives `astrs/status` (§8.4): the
/// manifest field can *promote* an ordinary input onto the control lane, but
/// its own default — `data` — never demotes the status port, which always
/// needs to pre-empt data traffic whether or not the manifest names it
/// explicitly. There is deliberately no way to force the status port back
/// onto the data lane through this field.
const fn priority_lane_for(lane: ManifestPriorityLane, is_status: bool) -> PriorityLane {
    match lane {
        ManifestPriorityLane::Control => PriorityLane::Control,
        ManifestPriorityLane::Data if is_status => PriorityLane::Control,
        ManifestPriorityLane::Data => PriorityLane::Data,
    }
}

/// The [`NodeSource`] a manifest node describes.
///
/// # Errors
///
/// [`DaemonError::Manifest`] when a `ros2:` block will not serialize, which
/// for a block that already parsed cannot happen — but is surfaced rather
/// than swallowed, because the alternative (forwarding *something else*) is
/// what the bridge node used to receive.
fn source_for(node: &ManifestNode) -> DaemonResult<NodeSource> {
    if node.is_dynamic_path() {
        return Ok(NodeSource::Dynamic);
    }
    if let Some(operators) = &node.operators {
        return Ok(NodeSource::Runtime {
            operators: operators
                .iter()
                .filter_map(|operator| {
                    astrs_wire::OperatorId::new(operator.id.clone())
                        .ok()
                        .map(|id| OperatorSpec::new(id, operator.id.clone()))
                })
                .collect(),
        });
    }
    if let Some(ros2) = &node.ros2 {
        // The bridge configuration crosses as JSON, which is the shape
        // `astrs-coordinator`'s `graph_bridge::node_source_for` already
        // chose; one contract, so `astrs run` (which plans here) and
        // `astrs up` (which plans there) hand `astrs-ros2-bridge-node` the
        // same blob. This crate stays out of the schema's business by
        // treating the block as opaque `serde` data — it never names a
        // field — which is what §4.1's downward-only rule actually asks
        // for.
        let config = serde_json::to_string(ros2).map_err(|source| {
            DaemonError::Manifest(format!(
                "node {} has a ros2 bridge that failed to serialize: {source}",
                node.id
            ))
        })?;
        return Ok(NodeSource::Ros2Bridge { config });
    }
    if let Some(outputs) = &node.record
        && !outputs.is_empty()
    {
        return Ok(NodeSource::Recorder {
            path: format!("{}.arec", node.id),
        });
    }
    Ok(NodeSource::Executable {
        path: node.path.clone().unwrap_or_else(|| node.id.clone()),
    })
}

/// The restart budget a manifest node asks for (§12).
fn restart_config(node: &ManifestNode) -> RestartConfig {
    let policy = match node.effective_restart_policy() {
        ManifestRestartPolicy::Never => RestartPolicy::Never,
        ManifestRestartPolicy::OnFailure => RestartPolicy::OnFailure,
        ManifestRestartPolicy::Always => RestartPolicy::Always,
    };
    let defaults = crate::supervise::config_for(policy);
    RestartConfig {
        policy,
        max_restarts: node.max_restarts,
        restart_delay: node.restart_delay.map_or(defaults.restart_delay, |secs| {
            DurationMs::from_duration(duration_secs(secs))
        }),
        max_restart_delay: node
            .max_restart_delay
            .map_or(defaults.max_restart_delay, |secs| {
                DurationMs::from_duration(duration_secs(secs))
            }),
        restart_window: node.restart_window.map_or(defaults.restart_window, |secs| {
            DurationMs::from_duration(duration_secs(secs))
        }),
    }
}

/// The logging configuration a manifest node asks for (§8.3, §13).
fn log_config(node: &ManifestNode) -> DaemonResult<LogConfig> {
    let send_stdout_as = match &node.send_stdout_as {
        Some(name) => Some(DataId::new(name.clone()).map_err(|error| {
            DaemonError::Manifest(format!("node {}: send_stdout_as: {error}", node.id))
        })?),
        None => None,
    };
    Ok(LogConfig {
        send_stdout_as,
        min_log_level: node.min_log_level.map_or(LogLevel::Info, log_level_for),
        max_log_size: node.max_log_size,
        max_rotated_files: Some(node.max_rotated_files),
    })
}

/// The wire log level a manifest level maps to.
const fn log_level_for(level: astrs_manifest::LogLevel) -> LogLevel {
    match level {
        astrs_manifest::LogLevel::Error => LogLevel::Error,
        astrs_manifest::LogLevel::Warn => LogLevel::Warn,
        astrs_manifest::LogLevel::Info => LogLevel::Info,
        astrs_manifest::LogLevel::Debug => LogLevel::Debug,
        astrs_manifest::LogLevel::Trace => LogLevel::Trace,
    }
}

/// The wire queue policy a manifest policy maps to.
const fn queue_policy_for(policy: ManifestQueuePolicy) -> QueuePolicy {
    match policy {
        ManifestQueuePolicy::DropOldest => QueuePolicy::DropOldest,
        ManifestQueuePolicy::Backpressure => QueuePolicy::Backpressure,
    }
}

/// The wire pattern a manifest pattern maps to.
const fn pattern_for(pattern: astrs_manifest::Pattern) -> astrs_wire::NodePattern {
    match pattern {
        astrs_manifest::Pattern::ServiceServer => astrs_wire::NodePattern::ServiceServer,
        astrs_manifest::Pattern::ServiceClient => astrs_wire::NodePattern::ServiceClient,
        astrs_manifest::Pattern::ActionServer => astrs_wire::NodePattern::ActionServer,
        astrs_manifest::Pattern::ActionClient => astrs_wire::NodePattern::ActionClient,
    }
}

/// The [`std::time::Duration`] a manifest's fractional-seconds field denotes.
///
/// A negative or non-finite value — which the manifest parser already refuses
/// — becomes zero rather than an error, because by this point the value has
/// been validated and there is nothing left to report it to.
fn duration_secs(secs: astrs_manifest::DurationSecs) -> std::time::Duration {
    std::time::Duration::try_from_secs_f64(secs.as_secs_f64()).unwrap_or(std::time::Duration::ZERO)
}

/// Expands one node's `env:` against the scrubbed base.
fn expand_env(
    node: &ManifestNode,
    manifest: &Manifest,
    base_env: &BTreeMap<String, String>,
    id: &NodeId,
) -> DaemonResult<BTreeMap<String, String>> {
    let merged = node.effective_env(&manifest.env);
    astrs_manifest::expand_map(&merged, |name| base_env.get(name).cloned()).map_err(|error| {
        DaemonError::BadEnv {
            node: id.clone(),
            reason: error.to_string(),
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn plan(yaml: &str) -> DataflowPlan {
        let manifest = Manifest::from_yaml_str(yaml).expect("valid yaml");
        plan_dataflow(DataflowId::from_u128(1), &manifest, &BTreeMap::new()).expect("valid plan")
    }

    fn node(name: &str) -> NodeId {
        NodeId::new(name).unwrap()
    }

    const PIPELINE: &str = "\
name: pipeline
nodes:
  - id: camera
    path: ./camera
    args: [--device, /dev/video0]
    outputs: [image]
    output_types:
      image: std/media/v1/Image
  - id: detect
    path: ./detect
    inputs:
      frames:
        source: camera/image
        queue_size: 4
        queue_policy: backpressure
    outputs: [detections]
";

    /// The bridge node (§10.5) is spawned by name for every `ros2:` node,
    /// and reads its whole configuration out of this field. Forwarding
    /// anything but the serialized block — the node id, say — leaves the
    /// bridge with nothing to bridge, so this pins the contract that
    /// `astrs-coordinator`'s `graph_bridge::node_source_for` also honours.
    #[test]
    fn a_ros2_block_crosses_as_the_serialized_block_not_the_node_id() {
        let plan = plan(
            "\
nodes:
  - id: lidar-in
    ros2:
      compat: humble
      topic: /scan
      message_type: sensor_msgs/msg/LaserScan
      direction: to_astrs
      qos: { reliable: true, keep_last: 10 }
    outputs: [scan]
",
        );
        let NodeSource::Ros2Bridge { config } = &plan.specs[0].source else {
            panic!("a `ros2:` node must plan as a bridge source");
        };
        let decoded: astrs_manifest::Ros2Config =
            serde_json::from_str(config).expect("the blob must be the serialized block");
        assert_eq!(decoded.topic.as_deref(), Some("/scan"));
        assert_eq!(
            decoded.message_type.as_deref(),
            Some("sensor_msgs/msg/LaserScan")
        );
        assert_eq!(decoded.compat, astrs_manifest::RosCompat::Humble);
        assert_eq!(decoded.qos.and_then(|qos| qos.keep_last), Some(10));
        assert_ne!(config, "lidar-in", "the node id is not a configuration");
    }

    #[test]
    fn every_node_gets_a_specification() {
        let plan = plan(PIPELINE);
        assert_eq!(plan.specs.len(), 2);
        assert_eq!(plan.name.as_deref(), Some("pipeline"));
        assert_eq!(plan.dataflow, DataflowId::from_u128(1));
        assert_eq!(plan.spawn_count(), 2);
        assert!(!plan.needs_build());
        assert!(
            !plan.exit_when_nodes_finish,
            "the manifest default is long-running (§8.2)"
        );
    }

    #[test]
    fn the_source_and_arguments_come_from_the_manifest() {
        let plan = plan(PIPELINE);
        let camera = plan.spec(&node("camera")).expect("planned");
        assert_eq!(
            camera.source,
            NodeSource::Executable {
                path: "./camera".into()
            }
        );
        assert_eq!(camera.args, ["--device", "/dev/video0"]);
    }

    #[test]
    fn queue_configuration_is_resolved_per_input() {
        let plan = plan(PIPELINE);
        let detect = plan.spec(&node("detect")).expect("planned");
        assert_eq!(detect.inputs.len(), 1);
        let frames = &detect.inputs[0];
        assert_eq!(frames.id.as_str(), "frames");
        assert_eq!(frames.source.to_string(), "camera/image");
        assert_eq!(frames.queue_size, 4);
        assert_eq!(frames.queue_policy, QueuePolicy::Backpressure);
    }

    // ------------------------------------------------- priority_lane / deadline (§11.3)

    #[test]
    fn an_ordinary_input_with_no_priority_lane_stays_on_the_data_lane() {
        let plan = plan(PIPELINE);
        let detect = plan.spec(&node("detect")).expect("planned");
        assert_eq!(detect.inputs[0].priority_lane, PriorityLane::Data);
        assert!(detect.inputs[0].deadline.is_none());
    }

    #[test]
    fn priority_lane_and_deadline_are_resolved_per_input() {
        let plan = plan(
            "\
nodes:
  - id: camera
    path: ./camera
    outputs: [image]
  - id: detect
    path: ./detect
    inputs:
      frames:
        source: camera/image
        priority_lane: control
        deadline: 20ms
    outputs: [detections]
",
        );
        let detect = plan.spec(&node("detect")).expect("planned");
        let frames = &detect.inputs[0];
        assert_eq!(frames.priority_lane, PriorityLane::Control);
        assert_eq!(frames.deadline, Some(DurationMs::new(20)));
    }

    #[test]
    fn the_status_port_is_promoted_to_the_control_lane_even_when_unset() {
        // `status: astrs/status` names no `priority_lane:` at all — the
        // manifest field's own default (`data`) must not silently demote
        // the one input that always needs to pre-empt data traffic.
        let plan = plan(
            "\
nodes:
  - id: planner
    path: ./planner
    inputs:
      status: astrs/status
",
        );
        let planner = plan.spec(&node("planner")).expect("planned");
        assert_eq!(planner.inputs[0].id.as_str(), "status");
        assert_eq!(planner.inputs[0].priority_lane, PriorityLane::Control);
    }

    #[test]
    fn an_explicit_data_lane_on_the_status_port_still_resolves_to_control() {
        // The manifest field has no way to force the status port back onto
        // the data lane (see `priority_lane_for`'s docs) — an *explicit*
        // `data` is indistinguishable from an omitted field, and both still
        // resolve to the automatic `control` promotion.
        let plan = plan(
            "\
nodes:
  - id: planner
    path: ./planner
    inputs:
      status:
        source: astrs/status
        priority_lane: data
",
        );
        let planner = plan.spec(&node("planner")).expect("planned");
        assert_eq!(planner.inputs[0].priority_lane, PriorityLane::Control);
    }

    #[test]
    fn a_short_form_input_takes_the_default_queue() {
        let plan = plan(
            "\
nodes:
  - id: a
    path: ./a
    outputs: [out]
  - id: b
    path: ./b
    inputs:
      in: a/out
",
        );
        let b = plan.spec(&node("b")).expect("planned");
        assert_eq!(b.inputs[0].queue_size, astrs_wire::DEFAULT_QUEUE_SIZE);
        assert_eq!(b.inputs[0].queue_policy, QueuePolicy::DropOldest);
    }

    #[test]
    fn type_urns_are_carried_onto_the_ports() {
        let plan = plan(PIPELINE);
        let camera = plan.spec(&node("camera")).expect("planned");
        assert_eq!(
            camera.outputs[0].type_urn.as_ref().map(TypeUrn::as_str),
            Some("std/media/v1/Image")
        );
    }

    #[test]
    fn virtual_inputs_are_collected_and_mapped_to_the_reserved_producer() {
        let plan = plan(
            "\
nodes:
  - id: planner
    path: ./planner
    inputs:
      tick: astrs/timer/hz/50
      logs: astrs/logs/warn
      status: astrs/status
",
        );
        assert_eq!(plan.virtual_inputs.len(), 3);
        assert_eq!(
            plan.virtual_inputs.iter().filter(|v| v.is_timer()).count(),
            1
        );
        assert_eq!(
            plan.virtual_inputs.iter().filter(|v| v.is_logs()).count(),
            1
        );
        assert_eq!(
            plan.virtual_inputs.iter().filter(|v| v.is_status()).count(),
            1
        );

        let planner = plan.spec(&node("planner")).expect("planned");
        let sources: Vec<String> = planner
            .inputs
            .iter()
            .map(|input| input.source.to_string())
            .collect();
        assert!(
            sources.contains(&"astrs/timer.hz.50".to_string()),
            "{sources:?}"
        );
        assert!(sources.contains(&"astrs/status".to_string()), "{sources:?}");
    }

    #[test]
    fn a_dynamic_node_is_planned_but_not_spawned() {
        let plan = plan(
            "\
nodes:
  - id: attached
    path: dynamic
    outputs: [out]
",
        );
        assert_eq!(plan.specs.len(), 1);
        assert_eq!(plan.specs[0].source, NodeSource::Dynamic);
        assert_eq!(plan.spawn_count(), 0);
    }

    #[test]
    fn restart_policies_and_timings_are_resolved() {
        let plan = plan(
            "\
nodes:
  - id: flaky
    path: ./flaky
    restart_policy: on_failure
    max_restarts: 3
    restart_delay: 0.5
    max_restart_delay: 10
    restart_window: 30
",
        );
        let flaky = plan.spec(&node("flaky")).expect("planned");
        assert_eq!(flaky.restart.policy, RestartPolicy::OnFailure);
        assert_eq!(flaky.restart.max_restarts, Some(3));
        assert_eq!(flaky.restart.restart_delay, DurationMs::new(500));
        assert_eq!(flaky.restart.max_restart_delay, DurationMs::from_secs(10));
        assert_eq!(flaky.restart.restart_window, DurationMs::from_secs(30));
    }

    #[test]
    fn a_node_with_no_restart_policy_never_restarts() {
        let plan = plan("nodes:\n  - id: a\n    path: ./a\n");
        assert_eq!(
            plan.spec(&node("a")).expect("planned").restart.policy,
            RestartPolicy::Never
        );
    }

    #[test]
    fn log_settings_are_resolved() {
        let plan = plan(
            "\
nodes:
  - id: chatty
    path: ./chatty
    outputs: [stdout]
    send_stdout_as: stdout
    min_log_level: debug
    max_log_size: 1048576
    max_rotated_files: 3
",
        );
        let chatty = plan.spec(&node("chatty")).expect("planned");
        assert_eq!(
            chatty.logging.send_stdout_as.as_ref().map(DataId::as_str),
            Some("stdout")
        );
        assert_eq!(chatty.logging.min_log_level, LogLevel::Debug);
        assert_eq!(chatty.logging.max_log_size, Some(1_048_576));
        assert_eq!(chatty.logging.max_rotated_files, Some(3));
    }

    #[test]
    fn build_lines_are_collected_in_node_order() {
        let plan = plan(
            "\
nodes:
  - id: a
    path: ./a
    build: cargo build --release -p a
  - id: b
    path: ./b
",
        );
        assert!(plan.needs_build());
        assert_eq!(plan.build_steps.len(), 1);
        assert_eq!(plan.build_steps[0].node, node("a"));
        assert_eq!(plan.build_steps[0].command, "cargo build --release -p a");
    }

    #[test]
    fn environment_is_expanded_against_the_supplied_base() {
        let manifest = Manifest::from_yaml_str(
            "\
env:
  GRAPH_WIDE: shared
nodes:
  - id: a
    path: ./a
    env:
      DEVICE: $ROOT/dev
      INDEX: 3
",
        )
        .unwrap();
        let base = BTreeMap::from([("ROOT".to_string(), "/opt".to_string())]);
        let plan = plan_dataflow(DataflowId::from_u128(1), &manifest, &base).unwrap();
        let a = plan.spec(&node("a")).expect("planned");
        assert_eq!(a.env.get("DEVICE").map(String::as_str), Some("/opt/dev"));
        assert_eq!(a.env.get("INDEX").map(String::as_str), Some("3"));
        assert_eq!(a.env.get("GRAPH_WIDE").map(String::as_str), Some("shared"));
    }

    #[test]
    fn an_unresolvable_reference_is_a_bad_env_error() {
        let manifest = Manifest::from_yaml_str(
            "nodes:\n  - id: a\n    path: ./a\n    env:\n      LEAK: $SECRET\n",
        )
        .unwrap();
        let error =
            plan_dataflow(DataflowId::from_u128(1), &manifest, &BTreeMap::new()).unwrap_err();
        assert!(matches!(error, DaemonError::BadEnv { .. }), "{error}");
    }

    #[test]
    fn an_invalid_manifest_is_refused_before_anything_is_planned() {
        let manifest = Manifest::from_yaml_str(
            "nodes:\n  - id: a\n    path: ./a\n    inputs:\n      in: nonexistent/out\n",
        )
        .unwrap();
        let error =
            plan_dataflow(DataflowId::from_u128(1), &manifest, &BTreeMap::new()).unwrap_err();
        assert!(matches!(error, DaemonError::Manifest(_)), "{error}");
    }

    #[test]
    fn a_malformed_virtual_source_is_refused() {
        let manifest = Manifest::from_yaml_str(
            "nodes:\n  - id: a\n    path: ./a\n    inputs:\n      tick: astrs/timer/hz/0\n",
        )
        .unwrap();
        assert!(plan_dataflow(DataflowId::from_u128(1), &manifest, &BTreeMap::new()).is_err());
    }

    #[test]
    fn the_health_check_interval_comes_from_the_manifest() {
        let explicit = plan("health_check_interval: 2.5\nnodes:\n  - id: a\n    path: ./a\n");
        assert_eq!(explicit.health_check_interval, DurationMs::new(2_500));

        let default = plan("nodes:\n  - id: a\n    path: ./a\n");
        assert_eq!(default.health_check_interval, DurationMs::from_secs(5));
    }

    #[test]
    fn exit_when_nodes_finish_is_carried_through() {
        let opted_in = plan("exit_when_nodes_finish: true\nnodes:\n  - id: a\n    path: ./a\n");
        assert!(opted_in.exit_when_nodes_finish);

        let default = plan("nodes:\n  - id: a\n    path: ./a\n");
        assert!(
            !default.exit_when_nodes_finish,
            "long-running unless the manifest says otherwise"
        );
    }

    #[test]
    fn per_node_deadlines_are_carried_through() {
        let plan = plan(
            "\
nodes:
  - id: a
    path: ./a
    health_check_timeout: 3
    finish_grace_secs: 7
",
        );
        let a = plan.spec(&node("a")).expect("planned");
        assert_eq!(a.health_check_timeout, Some(DurationMs::from_secs(3)));
        assert_eq!(a.finish_grace, Some(DurationMs::from_secs(7)));
    }

    #[test]
    fn cpu_affinity_is_carried_through() {
        let plan = plan("nodes:\n  - id: a\n    path: ./a\n    cpu_affinity: [0, 2]\n");
        assert_eq!(plan.spec(&node("a")).expect("planned").cpu_affinity, [0, 2]);
    }

    #[test]
    fn an_unknown_node_has_no_specification() {
        let plan = plan(PIPELINE);
        assert!(plan.spec(&node("nobody")).is_none());
    }
}
