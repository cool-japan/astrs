//! Bridging `astrs-manifest`/`astrs-graph`'s model onto `astrs-wire`'s spawn
//! specification.
//!
//! `astrs-graph` deliberately keeps its own [`astrs_graph::NodeId`] /
//! [`astrs_graph::PortName`] newtypes, unvalidated against the wire's
//! charset-restricted [`astrs_wire::NodeId`] / [`astrs_wire::DataId`] (see
//! that crate's `ids` module docs) — a manifest's node ids and port names
//! are already charset-checked by [`astrs_manifest::Manifest::validate`],
//! so the conversions here are expected to succeed for any manifest that
//! passed validation, and return a typed error rather than panicking on the
//! (only reachable via a bug elsewhere) case where they do not.
//!
//! This module is where blueprint §8.3's manifest node fields become §24.1's
//! [`astrs_wire::NodeSpawnSpec`] — the fully resolved form a daemon actually
//! spawns from. See [`build_node_spawn_spec`] for the composition, and its
//! helpers for the individual field mappings.

use std::collections::BTreeMap;

use astrs_graph::{DataflowGraph, Edge, EdgeKey, EdgeSource, GraphNode};
use astrs_manifest::{EnvValue, Manifest, Node};
use astrs_wire::{
    DataId, DeploySpec, DurationMs, InputSpec, LogConfig, MachineName, NodeId as WireNodeId,
    NodePattern, NodeSource, NodeSpawnSpec, OperatorId, OperatorSpec, OutputSpec, PortRef,
    PriorityLane as WirePriorityLane, QueuePolicy as WireQueuePolicy, RestartConfig,
    RestartPolicy as WireRestartPolicy, TypeUrn,
};

use crate::error::{CoordinatorError, Result};

/// Converts a graph node id to its wire form.
///
/// # Errors
///
/// [`CoordinatorError::Id`] if the id somehow fails the wire's charset check
/// — unreachable for a node id that came from a validated manifest.
pub fn node_id_to_wire(id: &astrs_graph::NodeId) -> Result<WireNodeId> {
    Ok(WireNodeId::new(id.as_str())?)
}

/// The inverse of [`node_id_to_wire`] — always succeeds, since the graph's
/// own [`astrs_graph::NodeId`] is an unvalidated newtype (see this
/// module's top-level docs).
#[must_use]
pub fn wire_node_id_to_graph(id: &WireNodeId) -> astrs_graph::NodeId {
    astrs_graph::NodeId::new(id.as_str())
}

/// Converts a graph port name to a wire data id.
///
/// # Errors
///
/// As [`node_id_to_wire`].
pub fn port_name_to_data_id(name: &astrs_graph::PortName) -> Result<DataId> {
    Ok(DataId::new(name.as_str())?)
}

/// Converts a validated manifest type URN to its wire form.
///
/// The wire's [`TypeUrn`] grammar (`is_ascii_graphic`) is a strict superset
/// of the manifest's own `<ns>/v<n>/<Type>[params]` grammar, so this only
/// fails for a URN longer than [`astrs_wire::MAX_TYPE_URN_LEN`].
///
/// # Errors
///
/// [`CoordinatorError::Id`] if the URN is too long for the wire type.
pub fn urn_to_type_urn(urn: &astrs_manifest::Urn) -> Result<TypeUrn> {
    Ok(TypeUrn::new(urn.as_str())?)
}

/// Converts a manifest queue policy to its wire equivalent.
#[must_use]
pub const fn queue_policy_to_wire(policy: astrs_manifest::QueuePolicy) -> WireQueuePolicy {
    match policy {
        astrs_manifest::QueuePolicy::DropOldest => WireQueuePolicy::DropOldest,
        astrs_manifest::QueuePolicy::Backpressure => WireQueuePolicy::Backpressure,
    }
}

/// Maps a manifest `priority_lane` to the wire lane, preserving the
/// automatic promotion the daemon already gives `astrs/status` (§8.4): the
/// manifest field can *promote* an ordinary input onto the control lane, but
/// its own default — `data` — never demotes the status port, which always
/// needs to pre-empt data traffic whether or not the manifest names it
/// explicitly. There is deliberately no way to force the status port back
/// onto the data lane through this field — the identical rule
/// `astrs_daemon`'s own local planner applies (see that crate's
/// `dataflow::plan::priority_lane_for`, which this mirrors for the
/// cluster-dispatch path).
#[must_use]
pub const fn priority_lane_to_wire(
    lane: astrs_manifest::PriorityLane,
    is_status: bool,
) -> WirePriorityLane {
    match lane {
        astrs_manifest::PriorityLane::Control => WirePriorityLane::Control,
        astrs_manifest::PriorityLane::Data if is_status => WirePriorityLane::Control,
        astrs_manifest::PriorityLane::Data => WirePriorityLane::Data,
    }
}

/// Converts a manifest restart policy to its wire equivalent.
#[must_use]
pub const fn restart_policy_to_wire(policy: astrs_manifest::RestartPolicy) -> WireRestartPolicy {
    match policy {
        astrs_manifest::RestartPolicy::Never => WireRestartPolicy::Never,
        astrs_manifest::RestartPolicy::OnFailure => WireRestartPolicy::OnFailure,
        astrs_manifest::RestartPolicy::Always => WireRestartPolicy::Always,
    }
}

/// Converts a manifest log level to its wire equivalent.
#[must_use]
pub const fn log_level_to_wire(level: astrs_manifest::LogLevel) -> astrs_wire::LogLevel {
    match level {
        astrs_manifest::LogLevel::Trace => astrs_wire::LogLevel::Trace,
        astrs_manifest::LogLevel::Debug => astrs_wire::LogLevel::Debug,
        astrs_manifest::LogLevel::Info => astrs_wire::LogLevel::Info,
        astrs_manifest::LogLevel::Warn => astrs_wire::LogLevel::Warn,
        astrs_manifest::LogLevel::Error => astrs_wire::LogLevel::Error,
    }
}

/// Converts a manifest service/action pattern to its wire equivalent.
#[must_use]
pub const fn pattern_to_wire(pattern: astrs_manifest::Pattern) -> NodePattern {
    match pattern {
        astrs_manifest::Pattern::ServiceServer => NodePattern::ServiceServer,
        astrs_manifest::Pattern::ServiceClient => NodePattern::ServiceClient,
        astrs_manifest::Pattern::ActionServer => NodePattern::ActionServer,
        astrs_manifest::Pattern::ActionClient => NodePattern::ActionClient,
    }
}

/// Renders a manifest duration as a wire [`DurationMs`], clamping a
/// pathological negative value (never produced by
/// [`astrs_manifest::DurationSecs::parse`], but not structurally
/// unrepresentable either) to zero rather than panicking on the
/// `as u64` cast.
#[must_use]
pub fn duration_ms(secs: astrs_manifest::DurationSecs) -> DurationMs {
    let millis = (secs.as_secs_f64().max(0.0) * 1_000.0).round();
    DurationMs::new(if millis.is_finite() {
        millis as u64
    } else {
        u64::MAX
    })
}

/// Flattens a manifest environment map into the plain string map
/// [`NodeSpawnSpec::env`] carries.
///
/// Deliberately performs **no** `$VAR` expansion: the manifest's `$VAR`
/// references name variables in the *target machine's* environment
/// (blueprint §16), which only the daemon that will actually spawn the
/// process can resolve — expanding against the coordinator's own
/// environment here would silently substitute the wrong machine's values,
/// or leak the coordinator's environment into a remote daemon's spawn.
#[must_use]
pub fn env_to_string_map(env: &BTreeMap<String, EnvValue>) -> BTreeMap<String, String> {
    env.iter()
        .map(|(key, value)| (key.clone(), value.to_raw_string()))
        .collect()
}

/// Builds the [`NodeSource`] a node's manifest fields resolve to.
///
/// [`astrs_manifest::Manifest::validate`] already enforces that a node
/// names exactly one source kind, so this checks them in a fixed,
/// documented order rather than needing to re-derive that exclusivity.
///
/// # Errors
///
/// - [`CoordinatorError::invalid`] if the node names none of the
///   recognised source kinds, or still carries an unexpanded `module:`
///   reference ([`astrs_manifest::expand::expand`] must run before this).
/// - [`CoordinatorError::invalid`] if a `ros2:` bridge configuration fails
///   to serialize (unreachable for a well-formed [`Ros2Config`](astrs_manifest::Ros2Config)).
pub fn node_source_for(node: &Node) -> Result<NodeSource> {
    if node.record.is_some() {
        return Ok(NodeSource::Recorder {
            path: format!("{}.arec", node.id),
        });
    }
    if let Some(operators) = &node.operators {
        let converted = operators
            .iter()
            .map(operator_config_to_wire)
            .collect::<Result<Vec<_>>>()?;
        return Ok(NodeSource::Runtime {
            operators: converted,
        });
    }
    if let Some(ros2) = &node.ros2 {
        let config = serde_json::to_string(ros2).map_err(|source| {
            CoordinatorError::invalid(format!(
                "node {} has a ros2 bridge that failed to serialize: {source}",
                node.id
            ))
        })?;
        return Ok(NodeSource::Ros2Bridge { config });
    }
    if node.is_dynamic_path() {
        return Ok(NodeSource::Dynamic);
    }
    if let Some(path) = &node.path {
        return Ok(NodeSource::Executable { path: path.clone() });
    }
    if let Some(hub) = &node.hub {
        let resolved = crate::hub_index::resolve_from_env(hub)
            .map_err(|error| CoordinatorError::invalid(format!("node {}: {error}", node.id)))?;
        return Ok(NodeSource::Executable {
            path: hub_executable_path(&node.id, resolved.subdir.as_deref()),
        });
    }
    if node.module.is_some() {
        return Err(CoordinatorError::invalid(format!(
            "node {} still carries an unexpanded module reference; \
             Manifest::expand must run before a dataflow starts",
            node.id
        )));
    }
    Err(CoordinatorError::invalid(format!(
        "node {} names no source (path/git/hub/module/operators/ros2/record)",
        node.id
    )))
}

/// The relative executable path a resolved `hub:` source spawns from, when
/// its package declares no `path:` of its own (impossible at the manifest
/// level — see [`crate::hub_index`]'s module docs on why `subdir` names a
/// package's directory, not its binary).
///
/// Defaults to the node's own id, exactly the convention
/// `astrs_daemon::dataflow::plan::source_for` already applies to *any*
/// node with no `path:` at all (`node.path.clone().unwrap_or(node.id)`) —
/// this crate's own git-source branch above has no such default (a `git:`
/// node with no `path:` is a pre-existing gap this function does not
/// widen), but a `hub:` source has no `path:` field to be missing in the
/// first place, so leaving it unresolvable would make the bare
/// `hub: pkg@rev` form — the one the whole feature exists for — unusable.
/// A `subdir`-hosting monorepo package joins it as the containing
/// directory, mirroring how a hand-written `git:` + `path:` pair would
/// spell `<subdir>/<binary>`.
fn hub_executable_path(node_id: &str, subdir: Option<&str>) -> String {
    match subdir {
        Some(subdir) => format!("{subdir}/{node_id}"),
        None => node_id.to_owned(),
    }
}

/// Converts one manifest operator entry to its wire form.
///
/// # Errors
///
/// [`CoordinatorError::Id`] if `id`/`operator` fail the wire's charset
/// checks.
fn operator_config_to_wire(config: &astrs_manifest::OperatorConfig) -> Result<OperatorSpec> {
    let id = OperatorId::new(&config.id)?;
    let mut spec = OperatorSpec::new(id, config.operator.clone());
    for (key, value) in &config.config {
        let parameter = crate::param_scope::json_to_parameter(value.clone())?;
        spec.config.insert(key.clone(), parameter);
    }
    Ok(spec)
}

/// Splits a manifest `build:` command line into argv the way `astrs-wire`'s
/// [`astrs_wire::BuildStep`] carries it — never through a shell (blueprint
/// §16).
///
/// # Errors
///
/// [`CoordinatorError::BadCommandLine`] if `command` cannot be tokenised
/// (an unterminated quote or a trailing backslash).
pub fn split_command_line(command: &str) -> Result<Vec<String>> {
    shlex::split(command).ok_or_else(|| CoordinatorError::BadCommandLine {
        command: command.to_owned(),
        reason: "unterminated quote or trailing backslash".to_owned(),
    })
}

/// A path-safe rendering of a node id, for the synthetic clone directory a
/// `git:` source builds into.
fn sanitize_path_component(id: &str) -> String {
    id.chars()
        .map(|ch| if ch == '/' { '_' } else { ch })
        .collect()
}

/// Builds the `git clone` (+ `git checkout`, when `rev` is given) steps a
/// git-hosted source resolves to — the machinery a manifest `git:` source
/// and a resolved `hub:` source share (see [`build_steps_for_node`] and
/// [`crate::hub_index`]'s module docs). `branch_or_tag` makes the clone
/// shallow when `rev` is absent, exactly as a `git:` source's own
/// `branch`/`tag` fields do; a resolved hub package always pins an exact
/// `rev` (never a bare branch/tag shorthand — see
/// [`crate::hub_index::IndexVersion::rev`]), so its caller always passes
/// `branch_or_tag: None`.
///
/// Returns the steps plus the directory they clone into, which becomes the
/// caller's new working directory for whatever build step runs next.
fn clone_and_checkout_steps(
    wire_id: &WireNodeId,
    url: &str,
    branch_or_tag: Option<&str>,
    rev: Option<&str>,
    clone_dir: &str,
    working_dir: Option<&str>,
) -> (Vec<astrs_wire::BuildStep>, String) {
    let mut steps = Vec::new();
    let mut argv = vec!["git".to_owned(), "clone".to_owned()];
    if let Some(branch) = branch_or_tag {
        argv.push("--branch".to_owned());
        argv.push(branch.to_owned());
    }
    // A pinned `rev` needs the branch history to check it out from, so
    // only a branch/tag-only clone is shallow.
    if rev.is_none() {
        argv.push("--depth".to_owned());
        argv.push("1".to_owned());
    }
    argv.push(url.to_owned());
    argv.push(clone_dir.to_owned());
    let mut clone_step = astrs_wire::BuildStep::new(wire_id.clone(), argv);
    if let Some(dir) = working_dir {
        clone_step = clone_step.with_working_dir(dir.to_owned());
    }
    steps.push(clone_step);

    if let Some(rev) = rev {
        let checkout = astrs_wire::BuildStep::new(
            wire_id.clone(),
            vec!["git".to_owned(), "checkout".to_owned(), rev.to_owned()],
        )
        .with_working_dir(clone_dir.to_owned());
        steps.push(checkout);
    }

    (steps, clone_dir.to_owned())
}

/// Builds the [`astrs_wire::BuildStep`]s a node's manifest fields imply: an
/// optional clone (`git clone` for `git:`, or a resolved `hub:` package's
/// repository — see [`clone_and_checkout_steps`]), plus a checkout for a
/// pinned revision, followed by the `build:` command line, in the
/// directory the clone produced when both are present.
///
/// `git:` and `hub:` are checked as `if`/`else if`, so at most one ever
/// lowers — correct only because [`astrs_manifest::Manifest::validate`]
/// already rejects a node naming both. This function itself performs no
/// such check: a [`Node`] built and passed in by hand with both set (as
/// [`expand_node_fragment`] accepts, deliberately skipping whole-manifest
/// validation for a dynamically-added node) silently prefers `git:` and
/// drops the `hub:` source rather than erroring.
///
/// # Errors
///
/// - [`CoordinatorError::BadCommandLine`] if `build:` cannot be tokenised.
/// - [`CoordinatorError::Id`] if the node id fails the wire's charset check.
/// - [`CoordinatorError::invalid`] if a `hub:` source cannot be resolved
///   against the local package index — see [`crate::hub_index::resolve_from_env`].
pub fn build_steps_for_node(
    node: &Node,
    dataflow_working_dir: Option<&str>,
) -> Result<Vec<astrs_wire::BuildStep>> {
    let wire_id = WireNodeId::new(&node.id)?;
    let mut steps = Vec::new();
    let mut working_dir = node
        .working_dir
        .clone()
        .or_else(|| dataflow_working_dir.map(str::to_owned));

    if let Some(url) = &node.git {
        let clone_dir = format!(".astrs-build/{}", sanitize_path_component(&node.id));
        let branch_or_tag = node.branch.as_deref().or(node.tag.as_deref());
        let (clone_steps, cloned_into) = clone_and_checkout_steps(
            &wire_id,
            url,
            branch_or_tag,
            node.rev.as_deref(),
            &clone_dir,
            working_dir.as_deref(),
        );
        steps.extend(clone_steps);
        working_dir = Some(cloned_into);
    } else if let Some(hub) = &node.hub {
        let resolved = crate::hub_index::resolve_from_env(hub)
            .map_err(|error| CoordinatorError::invalid(format!("node {}: {error}", node.id)))?;
        let clone_dir = format!(".astrs-build/{}", sanitize_path_component(&node.id));
        // A resolved hub package always pins an exact `rev` (never a bare
        // branch/tag shorthand), so it never takes the shallow-clone path.
        let (clone_steps, cloned_into) = clone_and_checkout_steps(
            &wire_id,
            &resolved.git,
            None,
            Some(&resolved.rev),
            &clone_dir,
            working_dir.as_deref(),
        );
        steps.extend(clone_steps);
        // A monorepo package's own `build:` line runs from its declared
        // subdirectory, not the clone root — the same relationship a
        // hand-written `git:` + `path:` pair expresses by hand.
        working_dir = Some(match &resolved.subdir {
            Some(subdir) => format!("{cloned_into}/{subdir}"),
            None => cloned_into,
        });
    }

    if let Some(command) = &node.build {
        let argv = split_command_line(command)?;
        if !argv.is_empty() {
            let mut step = astrs_wire::BuildStep::new(wire_id, argv);
            if let Some(dir) = &working_dir {
                step = step.with_working_dir(dir.clone());
            }
            for (key, value) in &node.env {
                step = step.with_env(key.clone(), value.to_raw_string());
            }
            steps.push(step);
        }
    }

    Ok(steps)
}

/// Builds one output port's wire specification.
///
/// # Errors
///
/// As [`port_name_to_data_id`] / [`urn_to_type_urn`].
pub fn output_spec_for(
    name: &astrs_graph::PortName,
    graph_node: &GraphNode,
    node: &Node,
) -> Result<OutputSpec> {
    let mut spec = OutputSpec::new(port_name_to_data_id(name)?);
    if let Some(urn) = graph_node.output_type(name) {
        spec = spec.with_type(urn_to_type_urn(urn)?);
    }
    spec.shm_pool_size = node.shm_pool_size;
    Ok(spec)
}

/// Builds one input port's wire specification from its graph edge.
///
/// A virtual `astrs/...` input (blueprint §8.4) gets a specification too,
/// with the reserved producer port [`astrs_wire::virtual_port_ref`] encodes.
/// It carries **no route**: [`routes_for_node`] emits `RouteSpec`s only for
/// [`EdgeSource::NodeOutput`] edges, because nothing on the network produces
/// a timer tick — the daemon serves it locally from its own timer wheel or
/// log fan-out (§11.1). What the specification does is *tell the daemon the
/// input exists*, which is the one thing the daemon cannot work out for
/// itself: it receives a fully expanded spawn specification and no manifest.
///
/// This used to return `Ok(None)`, and the consequence was silent: the
/// daemon's `subscribe_spec_virtual_inputs` — written to recover exactly
/// these sources from `spec.inputs` — had nothing to recover, so a node whose
/// only input was `astrs/timer/*` never ticked on a cluster while working
/// perfectly under `astrs run`, whose local planner does include them.
///
/// # Errors
///
/// As [`port_name_to_data_id`] / [`urn_to_type_urn`], plus
/// [`CoordinatorError::Id`] if a virtual source does not encode to a legal
/// port reference.
pub fn input_spec_for(
    edge_key: &EdgeKey,
    edge: &Edge,
    graph_node: &GraphNode,
) -> Result<Option<InputSpec>> {
    let data_id = port_name_to_data_id(&edge_key.input)?;
    let source = match &edge.from {
        EdgeSource::NodeOutput {
            node: producer,
            output,
        } => PortRef::new(node_id_to_wire(producer)?, port_name_to_data_id(output)?),
        EdgeSource::Virtual(source) => astrs_wire::virtual_port_ref(source)?,
    };
    let mut spec = InputSpec::new(data_id, source)
        .with_queue(edge.queue.size, queue_policy_to_wire(edge.queue.policy));
    if let Some(timeout) = edge.queue.timeout {
        spec.timeout = Some(duration_ms(timeout));
    }
    if let Some(urn) = graph_node.input_type(&edge_key.input) {
        spec = spec.with_type(urn_to_type_urn(urn)?);
    }
    Ok(Some(spec))
}

/// Builds the input ports of a `record:` sugar node (blueprint §14) from
/// its list of `node/output` strings — a recorder's inputs are declared
/// directly on [`Node::record`], not through [`Node::inputs`], so
/// `astrs-graph` never models them as edges (see [`build_node_spawn_spec`]
/// for where the two paths reconverge).
///
/// Inputs are named positionally (`in0`, `in1`, ...) rather than after
/// their producer's port, since two recorded ports may share an output
/// name (`camera/frames` and `lidar/frames` both output `frames`) and the
/// recorder itself only needs each incoming message's own metadata to
/// know its origin.
///
/// # Errors
///
/// [`CoordinatorError::invalid`] if an entry is not a well-formed
/// `node/output` reference.
pub fn record_node_inputs(ports: &[String]) -> Result<Vec<InputSpec>> {
    ports
        .iter()
        .enumerate()
        .map(|(index, port)| {
            let source: PortRef = port.parse().map_err(|_| {
                CoordinatorError::invalid(format!(
                    "record port {port:?} is not a `node/output` reference"
                ))
            })?;
            let id = DataId::new(format!("in{index}"))?;
            Ok(InputSpec::new(id, source))
        })
        .collect()
}

/// Resolves a node's effective placement label, mirroring
/// [`astrs_graph::MachineId::resolve`]'s override precedence (node
/// `deploy:` wins over the manifest-wide default) but rendering the result
/// as the [`DeploySpec`] `NodeSpawnSpec` actually carries.
///
/// # Errors
///
/// [`CoordinatorError::Id`] if a configured machine name fails the wire's
/// charset check.
pub fn deploy_spec_for(manifest: &Manifest, node: &Node) -> Result<DeploySpec> {
    let node_deploy = node.deploy.as_ref();
    let graph_deploy = manifest.deploy.as_ref();

    let machine_name = node_deploy
        .and_then(|deploy| deploy.machine.as_deref())
        .or_else(|| graph_deploy.and_then(|deploy| deploy.machine.as_deref()));
    let machine = machine_name.map(MachineName::new).transpose()?;

    let mut labels = graph_deploy
        .map(|deploy| deploy.labels.clone())
        .unwrap_or_default();
    if let Some(deploy) = node_deploy {
        labels.extend(deploy.labels.iter().map(|(k, v)| (k.clone(), v.clone())));
    }

    let working_dir = node_deploy
        .and_then(|deploy| deploy.working_dir.clone())
        .or_else(|| graph_deploy.and_then(|deploy| deploy.working_dir.clone()));

    Ok(DeploySpec {
        machine,
        labels,
        working_dir,
    })
}

/// Builds the restart budget a node's manifest fields imply.
#[must_use]
pub fn restart_config_for(node: &Node) -> RestartConfig {
    let mut config = RestartConfig {
        policy: restart_policy_to_wire(node.effective_restart_policy()),
        ..RestartConfig::never()
    };
    config.max_restarts = node.max_restarts;
    if let Some(delay) = node.restart_delay {
        config.restart_delay = duration_ms(delay);
    }
    if let Some(cap) = node.max_restart_delay {
        config.max_restart_delay = duration_ms(cap);
    }
    if let Some(window) = node.restart_window {
        config.restart_window = duration_ms(window);
    }
    config
}

/// Builds the logging configuration a node's manifest fields imply.
///
/// # Errors
///
/// [`CoordinatorError::Id`] if `send_stdout_as` fails the wire's charset
/// check.
pub fn log_config_for(node: &Node) -> Result<LogConfig> {
    let send_stdout_as = node
        .send_stdout_as
        .as_deref()
        .map(DataId::new)
        .transpose()?;
    Ok(LogConfig {
        send_stdout_as,
        min_log_level: node
            .min_log_level
            .map(log_level_to_wire)
            .unwrap_or(astrs_wire::LogLevel::Info),
        max_log_size: node.max_log_size,
        max_rotated_files: Some(node.max_rotated_files),
    })
}

/// Composes every field above into the complete [`NodeSpawnSpec`] a daemon
/// spawns from.
///
/// `graph` and `graph_node` supply the declared ports, their types and
/// their wiring (via `graph`'s edges); `manifest` and `node` supply
/// everything else. `generation` is the caller's responsibility — `1` for
/// a first spawn, incremented on every restart (blueprint §3.5/§6.2).
///
/// # Errors
///
/// As [`node_source_for`], [`build_steps_for_node`] (called separately by
/// the caller — this function does not build), [`output_spec_for`],
/// [`input_spec_for`], [`deploy_spec_for`] and [`log_config_for`].
pub fn build_node_spawn_spec(
    dataflow: astrs_wire::DataflowId,
    generation: u64,
    manifest: &Manifest,
    node: &Node,
    graph: &DataflowGraph,
    graph_node: &GraphNode,
    dataflow_working_dir: Option<&str>,
) -> Result<NodeSpawnSpec> {
    let wire_id = WireNodeId::new(&node.id)?;
    let source = node_source_for(node)?;
    let mut spec = NodeSpawnSpec::new(dataflow, wire_id, generation, source);

    spec.args = node.args.clone();
    spec.env = env_to_string_map(&node.effective_env(&manifest.env));
    spec.working_dir = node
        .working_dir
        .clone()
        .or_else(|| dataflow_working_dir.map(str::to_owned));
    spec.restart = restart_config_for(node);
    spec.logging = log_config_for(node)?;
    spec.deploy = deploy_spec_for(manifest, node)?;
    if let Some(cpu_affinity) = &node.cpu_affinity {
        spec.cpu_affinity = cpu_affinity.iter().map(|&core| core as u16).collect();
    }
    spec.health_check_timeout = node.health_check_timeout.map(duration_ms);
    spec.finish_grace = node.finish_grace_secs.map(duration_ms);
    spec.pattern = node.pattern.map(pattern_to_wire);

    if let Some(record_ports) = &node.record {
        spec.inputs = record_node_inputs(record_ports)?;
    } else {
        for name in &node.outputs {
            let port_name = astrs_graph::PortName::new(name.clone());
            spec.outputs
                .push(output_spec_for(&port_name, graph_node, node)?);
        }
        for name in graph_node.inputs.keys() {
            let edge_key = EdgeKey::new(graph_node.id.clone(), name.clone());
            if let Some(edge) = graph.edge(&edge_key)
                && let Some(mut input_spec) = input_spec_for(&edge_key, edge, graph_node)?
            {
                // `input_spec_for` builds from the *graph* edge alone (queue
                // size/policy/timeout, which `astrs_graph::Edge` also
                // carries) — `priority_lane`/`deadline` (§11.3) live only on
                // the manifest `Input` itself, so they are read from `node`
                // directly here rather than threaded through
                // `astrs_graph::Edge`, which has no field for either.
                if let Some(manifest_input) = node.inputs.get(name.as_ref()) {
                    let is_status = matches!(
                        &edge.from,
                        EdgeSource::Virtual(source) if source == "astrs/status"
                    );
                    input_spec.priority_lane =
                        priority_lane_to_wire(manifest_input.priority_lane, is_status);
                    input_spec.deadline = manifest_input.deadline.map(duration_ms);
                }
                spec.inputs.push(input_spec);
            }
        }
    }

    Ok(spec)
}

/// Expands a single-node manifest fragment (blueprint §8.3's per-node
/// schema, written with no `nodes:` wrapper — exactly one node's fields at
/// the document's top level) into a [`NodeSpawnSpec`].
///
/// This is what `astrs node add`/`astrs node replace` (blueprint §17) need
/// client-side: `crate::handlers::topology`'s `add_node`/`replace_node`
/// take the already-expanded [`NodeSpawnSpec`] directly — there is no
/// manifest text for the coordinator itself to expand for a dynamic node
/// (see that module's own docs) — so the caller supplying one from a
/// fragment file needs the same manifest-to-spec expansion
/// `build_node_spawn_spec` (this module's own, private) already does for
/// a normal `Start`, minus the parts that only make sense for a *whole*
/// graph.
///
/// Builds a throwaway one-node [`Manifest`] around the parsed node and
/// runs it through [`DataflowGraph::from_manifest`] exactly as an
/// ordinary manifest would be — deliberately *without*
/// [`astrs_manifest::Manifest::validate`]'s whole-graph checks first. An
/// input naming a producer this one-node fragment does not itself declare
/// is the *ordinary* case here (the entire point of a dynamic add is
/// wiring to a node already running elsewhere in the live dataflow), not
/// a malformed manifest: [`DataflowGraph::from_manifest`] happily
/// represents such a reference as a dangling edge (`astrs_graph`'s
/// internal manifest-to-graph wiring only ever rejects unparseable source
/// text, never an unresolved one), and whether it actually resolves is
/// exactly what [`astrs_graph::apply`] checks next, against the
/// coordinator's *real* tracked graph, back in `crate::handlers::topology`
/// — the same gate a manifest-driven node's edges pass through there too.
///
/// # Errors
///
/// [`CoordinatorError::invalid`] if `node_yaml` does not parse as one
/// node's fields (blueprint §8.3; `#[serde(deny_unknown_fields)]` applies
/// at this single-node level exactly as it does inside a full manifest).
/// Otherwise as this module's own `build_node_spawn_spec`.
pub fn expand_node_fragment(
    dataflow: astrs_wire::DataflowId,
    generation: u64,
    node_yaml: &str,
) -> Result<NodeSpawnSpec> {
    let node: Node = astrs_yaml::from_str(node_yaml).map_err(|source| {
        CoordinatorError::invalid(format!("node manifest fragment is not valid: {source}"))
    })?;
    expand_node(dataflow, generation, &node)
}

/// [`expand_node_fragment`] for a caller that already holds the parsed node.
///
/// The same expansion, entered one step later. `astrs replay`'s live form
/// reaches it this way: it rewrites a node it took out of the coordinator's
/// *own* expanded manifest, so re-serializing that node to YAML only to parse
/// it straight back would be a round trip through a format neither end
/// wanted — and one more place for a serialization quirk to change what gets
/// spawned.
///
/// # Errors
///
/// As [`expand_node_fragment`], minus the parse.
pub fn expand_node(
    dataflow: astrs_wire::DataflowId,
    generation: u64,
    node: &Node,
) -> Result<NodeSpawnSpec> {
    let node = node.clone();
    let manifest = Manifest {
        nodes: vec![node.clone()],
        ..Manifest::default()
    };
    let (graph, _diagnostics) = DataflowGraph::from_manifest(&manifest)?;
    let graph_id = astrs_graph::NodeId::new(node.id.clone());
    let graph_node = graph.node(&graph_id).ok_or_else(|| {
        CoordinatorError::invalid(format!(
            "node {:?} did not survive its own graph construction",
            node.id
        ))
    })?;
    build_node_spawn_spec(
        dataflow, generation, &manifest, &node, &graph, graph_node, None,
    )
}

// ---------------------------------------------------------------------
// The inverse direction: a dynamic-topology request's `NodeSpawnSpec`
// back into `astrs-graph`'s model, so it can be validated (and the
// coordinator's tracked graph kept in sync) through
// `astrs_graph::diff::apply` — see `crate::handlers::topology`.
// ---------------------------------------------------------------------

/// The inverse of [`pattern_to_wire`].
///
/// `NodePattern` is `#[non_exhaustive]`; blueprint §9.4's pattern set is
/// currently exactly the four service/action roles below, so a variant
/// added ahead of a matching manifest-side one has nothing to fall back to
/// except the closest of those four rather than a genuine equivalent —
/// unreachable today, and worth a loud default (a service server, the
/// least presumptuous role) rather than a silent guess if it ever isn't.
#[must_use]
pub const fn pattern_to_manifest(pattern: NodePattern) -> astrs_manifest::Pattern {
    match pattern {
        NodePattern::ServiceClient => astrs_manifest::Pattern::ServiceClient,
        NodePattern::ActionServer => astrs_manifest::Pattern::ActionServer,
        NodePattern::ActionClient => astrs_manifest::Pattern::ActionClient,
        _ => astrs_manifest::Pattern::ServiceServer,
    }
}

/// The inverse of [`queue_policy_to_wire`], with the same closed-set
/// fallback rationale as [`pattern_to_manifest`].
#[must_use]
pub const fn queue_policy_to_manifest(policy: WireQueuePolicy) -> astrs_manifest::QueuePolicy {
    match policy {
        WireQueuePolicy::Backpressure => astrs_manifest::QueuePolicy::Backpressure,
        _ => astrs_manifest::QueuePolicy::DropOldest,
    }
}

/// Builds the [`astrs_graph::Edge`] a dynamic-topology input spec implies.
///
/// The exact inverse of [`input_spec_for`]'s source handling, virtual
/// sources included: a spec whose producer is the reserved `astrs` node
/// (§8.4) becomes an [`EdgeSource::Virtual`] again rather than an edge from a
/// node that does not exist. Without that, adding a node whose input is a
/// timer would fail [`astrs_graph::apply`]'s referential-integrity check
/// looking for a producer called `astrs`.
///
/// The tracked edge's queue *timeout* is always `None`, never `input`'s
/// real value: `live.graph`'s edges are consulted only by
/// [`astrs_graph::apply`]'s referential-integrity and type-safety checks
/// (see [`graph_node_for_spawn_spec`]'s docs — neither reads queue
/// timing), and the spawned node's real timeout still reaches the daemon
/// untouched, in the very [`NodeSpawnSpec`] this is built from — nothing
/// that actually governs delivery ever sees the dropped value.
#[must_use]
pub fn edge_for_input_spec(input: &InputSpec) -> Edge {
    let from = if astrs_wire::is_virtual_port(&input.source) {
        EdgeSource::Virtual(astrs_wire::virtual_source_text(&input.source))
    } else {
        EdgeSource::NodeOutput {
            node: astrs_graph::NodeId::new(input.source.node().as_str()),
            output: astrs_graph::PortName::new(input.source.port().as_str().to_owned()),
        }
    };
    Edge {
        from,
        queue: astrs_graph::QueueConfig {
            size: input.queue_size,
            policy: queue_policy_to_manifest(input.queue_policy),
            timeout: None,
        },
    }
}

/// Converts a fully-resolved [`NodeSpawnSpec`] back into the
/// [`GraphNode`] shape [`astrs_graph::apply`] validates against — the
/// inverse of [`build_node_spawn_spec`], needed because a dynamic
/// [`astrs_wire::ControlRequest::AddNode`]/`ReplaceNode` carries a spawn
/// spec directly (there is no manifest text for a coordinator to build a
/// [`GraphNode`] from the usual way).
///
/// Every [`GraphNode`] field has a total, infallible source in
/// `NodeSpawnSpec` — ports and their type URNs from `outputs`/`inputs`,
/// pattern from `pattern`, placement from `deploy`, and
/// [`GraphNode::spawns_process`] from whether `source` is
/// [`astrs_wire::NodeSource::Dynamic`] (blueprint §8.3's external-attach
/// nodes are the one source kind with no daemon-owned process either
/// here or in the manifest-driven path — see that field's own docs).
#[must_use]
pub fn graph_node_for_spawn_spec(spec: &NodeSpawnSpec) -> GraphNode {
    let outputs = spec
        .outputs
        .iter()
        .map(|output| {
            let urn = output
                .type_urn
                .as_ref()
                .map(|urn| astrs_manifest::Urn::new(urn.as_str()));
            (
                astrs_graph::PortName::new(output.id.as_str().to_owned()),
                astrs_graph::OutputPort { type_urn: urn },
            )
        })
        .collect();
    let inputs = spec
        .inputs
        .iter()
        .map(|input| {
            let urn = input
                .type_urn
                .as_ref()
                .map(|urn| astrs_manifest::Urn::new(urn.as_str()));
            (
                astrs_graph::PortName::new(input.id.as_str().to_owned()),
                astrs_graph::InputPort { type_urn: urn },
            )
        })
        .collect();
    let machine = spec
        .deploy
        .machine
        .as_ref()
        .map(|name| astrs_graph::MachineId::Named(name.to_string()))
        .unwrap_or(astrs_graph::MachineId::CoordinatorLocal);

    GraphNode {
        id: wire_node_id_to_graph(&spec.node),
        outputs,
        inputs,
        pattern: spec.pattern.map(pattern_to_manifest),
        machine,
        spawns_process: !matches!(spec.source, NodeSource::Dynamic),
    }
}

/// Builds the [`astrs_graph::TopologyOp`]s an `AddNode` request implies:
/// the node itself, plus one `AddEdge` per declared input whose source is
/// an ordinary producer port — a virtual `astrs/...` source (blueprint
/// §8.4), like a manifest node's, has no graph-tracked edge, since
/// [`crate::graph_bridge`]'s own [`input_spec_for`] never emits one for a
/// virtual-sourced input either.
///
/// Every `AddNode` in this crate's dynamic-topology handlers dispatches a
/// [`NodeSpawnSpec`] whose inputs are already fully resolved — there is no
/// virtual-source equivalent on this path (dynamic nodes name real
/// producers only), but this checks anyway rather than assuming it.
#[must_use]
pub fn topology_ops_for_add_node(spec: &NodeSpawnSpec) -> Vec<astrs_graph::TopologyOp> {
    let id = wire_node_id_to_graph(&spec.node);
    let node = graph_node_for_spawn_spec(spec);
    let mut ops = vec![astrs_graph::TopologyOp::AddNode {
        id: id.clone(),
        node,
    }];
    for input in &spec.inputs {
        let key = EdgeKey::new(
            id.clone(),
            astrs_graph::PortName::new(input.id.as_str().to_owned()),
        );
        ops.push(astrs_graph::TopologyOp::AddEdge {
            key,
            edge: edge_for_input_spec(input),
        });
    }
    ops
}

/// Builds the ops [`astrs_graph::apply`] needs to remove `id` from `graph`
/// cleanly: a `RemoveEdge` for every edge already tracked that names `id`
/// as producer or consumer, then the `RemoveNode` itself — `apply`
/// rejects a `RemoveNode` left with a dangling edge reference rather than
/// dropping it implicitly (see [`astrs_graph`]'s `diff` module docs), so
/// those removals must be listed explicitly rather than assumed.
#[must_use]
pub fn topology_ops_for_remove_node(
    graph: &DataflowGraph,
    id: &astrs_graph::NodeId,
) -> Vec<astrs_graph::TopologyOp> {
    let mut ops: Vec<astrs_graph::TopologyOp> = graph
        .edges
        .iter()
        .filter(|(key, edge)| {
            &key.consumer == id
                || matches!(&edge.from, EdgeSource::NodeOutput { node, .. } if node == id)
        })
        .map(|(key, _)| astrs_graph::TopologyOp::RemoveEdge { key: key.clone() })
        .collect();
    ops.push(astrs_graph::TopologyOp::RemoveNode { id: id.clone() });
    ops
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_manifest::Manifest;

    fn manifest(yaml: &str) -> Manifest {
        let manifest = Manifest::from_yaml_str(yaml).unwrap();
        manifest.validate().unwrap();
        manifest
    }

    fn graph_of(manifest: &Manifest) -> DataflowGraph {
        let (graph, diagnostics) = DataflowGraph::from_manifest(manifest).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        graph
    }

    #[test]
    fn a_graph_node_id_converts_to_the_matching_wire_id() {
        let graph_id = astrs_graph::NodeId::new("camera");
        let wire_id = node_id_to_wire(&graph_id).unwrap();
        assert_eq!(wire_id.as_str(), "camera");
    }

    #[test]
    fn queue_and_restart_and_pattern_map_one_to_one() {
        assert_eq!(
            queue_policy_to_wire(astrs_manifest::QueuePolicy::Backpressure),
            WireQueuePolicy::Backpressure
        );
        assert_eq!(
            restart_policy_to_wire(astrs_manifest::RestartPolicy::OnFailure),
            WireRestartPolicy::OnFailure
        );
        assert_eq!(
            pattern_to_wire(astrs_manifest::Pattern::ActionServer),
            NodePattern::ActionServer
        );
        assert_eq!(
            log_level_to_wire(astrs_manifest::LogLevel::Warn),
            astrs_wire::LogLevel::Warn
        );
    }

    #[test]
    fn duration_conversion_is_millisecond_accurate() {
        let secs = astrs_manifest::DurationSecs::from_secs_f64(1.5).unwrap();
        assert_eq!(duration_ms(secs), DurationMs::new(1_500));
    }

    #[test]
    fn env_map_renders_every_scalar_kind_without_dollar_expansion() {
        let mut env = BTreeMap::new();
        env.insert("A".to_owned(), EnvValue::String("$HOME".to_owned()));
        env.insert("B".to_owned(), EnvValue::Bool(true));
        env.insert("C".to_owned(), EnvValue::Int(3));
        env.insert("D".to_owned(), EnvValue::Float(1.5));
        let rendered = env_to_string_map(&env);
        assert_eq!(
            rendered.get("A"),
            Some(&"$HOME".to_owned()),
            "never expanded here"
        );
        assert_eq!(rendered.get("B"), Some(&"true".to_owned()));
        assert_eq!(rendered.get("C"), Some(&"3".to_owned()));
        assert_eq!(rendered.get("D"), Some(&"1.5".to_owned()));
    }

    #[test]
    fn node_source_recognises_every_kind() {
        let mut node = Node::with_path("camera", "./camera");
        assert!(matches!(
            node_source_for(&node).unwrap(),
            NodeSource::Executable { .. }
        ));

        node.path = Some(astrs_manifest::DYNAMIC_PATH_SENTINEL.to_owned());
        assert!(matches!(
            node_source_for(&node).unwrap(),
            NodeSource::Dynamic
        ));

        let mut recorder = Node::with_path("rec", "unused");
        recorder.path = None;
        recorder.record = Some(vec!["camera/frames".to_owned()]);
        assert!(matches!(
            node_source_for(&recorder).unwrap(),
            NodeSource::Recorder { .. }
        ));

        let mut runtime = Node::with_path("ops", "unused");
        runtime.path = None;
        runtime.operators = Some(vec![astrs_manifest::OperatorConfig {
            id: "crop".to_owned(),
            operator: "vision::Crop".to_owned(),
            dylib: None,
            wasm: None,
            hub: None,
            inputs: BTreeMap::new(),
            outputs: Vec::new(),
            config: BTreeMap::new(),
        }]);
        assert!(matches!(
            node_source_for(&runtime).unwrap(),
            NodeSource::Runtime { .. }
        ));
    }

    #[test]
    fn a_node_naming_no_source_is_rejected() {
        let mut node = Node::with_path("x", "unused");
        node.path = None;
        let err = node_source_for(&node).unwrap_err();
        assert!(matches!(err, CoordinatorError::InvalidArgument(_)));
    }

    // ---- hub: sources (blueprint §22) ----------------------------------
    //
    // `node_source_for`'s and `build_steps_for_node`'s own hub branches
    // only need `crate::hub_index::resolve_from_env` to see *some* local
    // index — the resolution algorithm itself (version matching, "bad
    // rev", malformed entries, a missing index entirely) is exercised
    // exhaustively in `crate::hub_index`'s own tests, not duplicated here.

    fn hub_scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "astrs-coordinator-graph-bridge-hub-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("packages")).unwrap();
        dir
    }

    fn write_hub_package(index_root: &std::path::Path, package: &crate::hub_index::IndexPackage) {
        let text = serde_json::to_string(package).unwrap();
        std::fs::write(
            index_root
                .join("packages")
                .join(format!("{}.json", package.name)),
            text,
        )
        .unwrap();
    }

    /// Runs `body` with [`crate::hub_index::ENV_HUB_CACHE_DIR`] pointed at
    /// `index_root` — see `hub_index`'s own tests for why mutating the
    /// process environment is acceptable here (nextest's one-process-per-
    /// test isolation) and why this test file owns the variable outright.
    fn with_hub_cache_dir<R>(index_root: &std::path::Path, body: impl FnOnce() -> R) -> R {
        unsafe {
            std::env::set_var(crate::hub_index::ENV_HUB_CACHE_DIR, index_root);
        }
        let result = body();
        unsafe {
            std::env::remove_var(crate::hub_index::ENV_HUB_CACHE_DIR);
        }
        result
    }

    fn hub_node(id: &str, hub: astrs_manifest::HubSource) -> Node {
        let mut node = Node::with_path(id, "unused");
        node.path = None;
        node.hub = Some(hub);
        node
    }

    #[test]
    fn a_bare_hub_source_defaults_its_executable_path_to_the_node_id() {
        let dir = hub_scratch("node-source-no-subdir");
        write_hub_package(
            &dir,
            &crate::hub_index::IndexPackage {
                name: "yolo-detector".to_owned(),
                description: "detector".to_owned(),
                versions: vec![crate::hub_index::IndexVersion {
                    version: "v1".to_owned(),
                    git: "https://example.invalid/yolo".to_owned(),
                    rev: "abc123".to_owned(),
                    subdir: None,
                }],
            },
        );
        let node = hub_node(
            "detector",
            astrs_manifest::HubSource::latest("yolo-detector"),
        );

        let source = with_hub_cache_dir(&dir, || node_source_for(&node).unwrap());
        assert_eq!(
            source,
            NodeSource::Executable {
                path: "detector".to_owned()
            }
        );
    }

    #[test]
    fn a_hub_source_with_a_subdir_joins_it_onto_the_node_id() {
        let dir = hub_scratch("node-source-with-subdir");
        write_hub_package(
            &dir,
            &crate::hub_index::IndexPackage {
                name: "yolo-detector".to_owned(),
                description: "detector".to_owned(),
                versions: vec![crate::hub_index::IndexVersion {
                    version: "v1".to_owned(),
                    git: "https://example.invalid/yolo".to_owned(),
                    rev: "abc123".to_owned(),
                    subdir: Some("nodes/yolo".to_owned()),
                }],
            },
        );
        let node = hub_node(
            "detector",
            astrs_manifest::HubSource::latest("yolo-detector"),
        );

        let source = with_hub_cache_dir(&dir, || node_source_for(&node).unwrap());
        assert_eq!(
            source,
            NodeSource::Executable {
                path: "nodes/yolo/detector".to_owned()
            }
        );
    }

    #[test]
    fn an_unresolvable_hub_source_is_a_clear_error_naming_the_node() {
        // No `ASTRS_HUB_CACHE_DIR` fixture at all here: whatever the real
        // process environment happens to have is almost certainly not a
        // package index, so this exercises the same "index missing" path
        // `hub_index`'s own tests cover directly, but through
        // `node_source_for`'s wrapping — the node id must be in the
        // message, which `hub_index::HubIndexError` alone cannot supply.
        let dir = hub_scratch("node-source-unresolvable");
        let node = hub_node("detector", astrs_manifest::HubSource::latest("nonexistent"));

        let err = with_hub_cache_dir(&dir, || node_source_for(&node).unwrap_err());
        let message = err.to_string();
        assert!(message.contains("detector"), "{message}");
        assert!(message.contains("astrs hub update"), "{message}");
    }

    #[test]
    fn an_unexpanded_module_reference_is_a_clear_error() {
        let mut node = Node::with_path("m", "unused");
        node.path = None;
        node.module = Some("./m.yaml".to_owned());
        let err = node_source_for(&node).unwrap_err();
        assert!(err.to_string().contains("unexpanded"));
    }

    #[test]
    fn build_steps_split_the_command_line_by_shlex_rules() {
        let mut node = Node::with_path("camera", "./target/release/camera");
        node.build = Some("cargo build --release -p camera-node".to_owned());
        let steps = build_steps_for_node(&node, None).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].program(), Some("cargo"));
        assert_eq!(steps[0].args(), ["build", "--release", "-p", "camera-node"]);
    }

    #[test]
    fn a_malformed_build_command_is_a_typed_error() {
        let mut node = Node::with_path("camera", "./camera");
        node.build = Some("cargo build \"unterminated".to_owned());
        let err = build_steps_for_node(&node, None).unwrap_err();
        assert!(matches!(err, CoordinatorError::BadCommandLine { .. }));
    }

    #[test]
    fn git_sources_clone_before_building_and_reuse_the_clone_dir() {
        let mut node = Node::with_path("detector", "target/release/yolo-node");
        node.git = Some("https://example.invalid/astrs-yolo".to_owned());
        node.tag = Some("v0.3.1".to_owned());
        node.build = Some("cargo build --release".to_owned());
        let steps = build_steps_for_node(&node, None).unwrap();
        assert_eq!(steps.len(), 2, "clone then build");
        assert_eq!(steps[0].program(), Some("git"));
        assert!(steps[0].args().contains(&"--branch".to_owned()));
        assert!(steps[0].args().contains(&"v0.3.1".to_owned()));
        assert_eq!(steps[1].program(), Some("cargo"));
        assert_eq!(
            steps[1].working_dir.as_deref(),
            Some(".astrs-build/detector")
        );
    }

    #[test]
    fn a_pinned_rev_adds_a_checkout_step_and_skips_the_shallow_clone() {
        let mut node = Node::with_path("detector", "target/release/yolo-node");
        node.git = Some("https://example.invalid/astrs-yolo".to_owned());
        node.rev = Some("deadbeef".to_owned());
        let steps = build_steps_for_node(&node, None).unwrap();
        assert_eq!(steps.len(), 2, "clone then checkout, no build: set");
        assert!(!steps[0].args().contains(&"--depth".to_owned()));
        assert_eq!(steps[1].command, vec!["git", "checkout", "deadbeef"]);
    }

    #[test]
    fn hub_sources_lower_onto_a_full_clone_plus_checkout_of_the_resolved_package() {
        let dir = hub_scratch("build-steps-no-subdir");
        write_hub_package(
            &dir,
            &crate::hub_index::IndexPackage {
                name: "yolo-detector".to_owned(),
                description: "detector".to_owned(),
                versions: vec![crate::hub_index::IndexVersion {
                    version: "v0.3.1".to_owned(),
                    git: "https://example.invalid/astrs-yolo".to_owned(),
                    rev: "deadbeef".to_owned(),
                    subdir: None,
                }],
            },
        );
        let node = hub_node(
            "detector",
            astrs_manifest::HubSource::pinned("yolo-detector", "v0.3.1"),
        );

        let steps = with_hub_cache_dir(&dir, || build_steps_for_node(&node, None).unwrap());
        assert_eq!(steps.len(), 2, "clone then checkout, no build: set");
        assert_eq!(steps[0].program(), Some("git"));
        assert_eq!(
            steps[0].command,
            vec![
                "git",
                "clone",
                "https://example.invalid/astrs-yolo",
                ".astrs-build/detector",
            ],
            "a resolved hub package always pins an exact rev, so it is never a shallow clone"
        );
        assert!(!steps[0].args().contains(&"--depth".to_owned()));
        assert_eq!(steps[1].command, vec!["git", "checkout", "deadbeef"]);
        assert_eq!(
            steps[1].working_dir.as_deref(),
            Some(".astrs-build/detector")
        );
    }

    #[test]
    fn hub_sources_with_a_subdir_run_the_build_line_from_within_it() {
        let dir = hub_scratch("build-steps-with-subdir");
        write_hub_package(
            &dir,
            &crate::hub_index::IndexPackage {
                name: "yolo-detector".to_owned(),
                description: "detector".to_owned(),
                versions: vec![crate::hub_index::IndexVersion {
                    version: "v1".to_owned(),
                    git: "https://example.invalid/astrs-yolo".to_owned(),
                    rev: "cafef00d".to_owned(),
                    subdir: Some("nodes/yolo".to_owned()),
                }],
            },
        );
        let mut node = hub_node(
            "detector",
            astrs_manifest::HubSource::latest("yolo-detector"),
        );
        node.build = Some("cargo build --release".to_owned());

        let steps = with_hub_cache_dir(&dir, || build_steps_for_node(&node, None).unwrap());
        assert_eq!(steps.len(), 3, "clone, checkout, then build");
        assert_eq!(steps[2].program(), Some("cargo"));
        assert_eq!(
            steps[2].working_dir.as_deref(),
            Some(".astrs-build/detector/nodes/yolo"),
            "the build line runs from the package's own subdirectory, not the clone root"
        );
    }

    #[test]
    fn an_unresolvable_hub_source_fails_build_steps_with_a_clear_error() {
        let dir = hub_scratch("build-steps-unresolvable");
        let node = hub_node("detector", astrs_manifest::HubSource::latest("nonexistent"));

        let err = with_hub_cache_dir(&dir, || build_steps_for_node(&node, None).unwrap_err());
        let message = err.to_string();
        assert!(message.contains("detector"), "{message}");
        assert!(message.contains("nonexistent"), "{message}");
    }

    #[test]
    fn record_node_inputs_names_ports_positionally() {
        let inputs =
            record_node_inputs(&["camera/frames".to_owned(), "lidar/frames".to_owned()]).unwrap();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0].id.as_str(), "in0");
        assert_eq!(inputs[0].source.to_string(), "camera/frames");
        assert_eq!(inputs[1].id.as_str(), "in1");
    }

    #[test]
    fn record_node_inputs_rejects_a_malformed_port_reference() {
        let err = record_node_inputs(&["not-a-port-ref".to_owned()]).unwrap_err();
        assert!(matches!(err, CoordinatorError::InvalidArgument(_)));
    }

    #[test]
    fn deploy_spec_prefers_the_node_override_over_the_graph_default() {
        let manifest = manifest(
            "\
deploy: { machine: robot-1 }
nodes:
  - id: a
    path: ./a
    deploy: { machine: robot-2, labels: { gpu: \"true\" } }
",
        );
        let spec = deploy_spec_for(&manifest, &manifest.nodes[0]).unwrap();
        assert_eq!(
            spec.machine.map(|m| m.into_string()),
            Some("robot-2".to_owned())
        );
        assert_eq!(spec.labels.get("gpu").map(String::as_str), Some("true"));
    }

    #[test]
    fn deploy_spec_falls_back_to_the_graph_default_when_the_node_sets_nothing() {
        let manifest = manifest(
            "\
deploy: { machine: robot-1 }
nodes:
  - id: a
    path: ./a
",
        );
        let spec = deploy_spec_for(&manifest, &manifest.nodes[0]).unwrap();
        assert_eq!(
            spec.machine.map(|m| m.into_string()),
            Some("robot-1".to_owned())
        );
    }

    #[test]
    fn build_node_spawn_spec_wires_a_typed_edge_end_to_end() {
        let manifest = manifest(
            "\
nodes:
  - id: camera
    path: ./camera
    outputs: [frames]
    output_types: { frames: \"std/media/v1/Image\" }
  - id: detector
    path: ./detector
    inputs:
      frames:
        source: camera/frames
        queue_size: 3
        queue_policy: backpressure
    input_types: { frames: \"std/media/v1/Image\" }
",
        );
        let graph = graph_of(&manifest);
        let dataflow = astrs_wire::DataflowId::generate();

        let camera_node = manifest.nodes.iter().find(|n| n.id == "camera").unwrap();
        let camera_graph_node = graph.node(&astrs_graph::NodeId::new("camera")).unwrap();
        let camera_spec = build_node_spawn_spec(
            dataflow,
            1,
            &manifest,
            camera_node,
            &graph,
            camera_graph_node,
            None,
        )
        .unwrap();
        assert_eq!(camera_spec.outputs.len(), 1);
        assert_eq!(camera_spec.outputs[0].id.as_str(), "frames");
        assert_eq!(
            camera_spec.outputs[0]
                .type_urn
                .as_ref()
                .map(TypeUrn::as_str),
            Some("std/media/v1/Image")
        );

        let detector_node = manifest.nodes.iter().find(|n| n.id == "detector").unwrap();
        let detector_graph_node = graph.node(&astrs_graph::NodeId::new("detector")).unwrap();
        let detector_spec = build_node_spawn_spec(
            dataflow,
            1,
            &manifest,
            detector_node,
            &graph,
            detector_graph_node,
            None,
        )
        .unwrap();
        assert_eq!(detector_spec.inputs.len(), 1);
        let input = &detector_spec.inputs[0];
        assert_eq!(input.id.as_str(), "frames");
        assert_eq!(input.source.to_string(), "camera/frames");
        assert_eq!(input.queue_size, 3);
        assert_eq!(input.queue_policy, WireQueuePolicy::Backpressure);
    }

    #[test]
    fn build_node_spawn_spec_resolves_priority_lane_and_deadline() {
        let manifest = manifest(
            "\
nodes:
  - id: camera
    path: ./camera
    outputs: [frames]
  - id: detector
    path: ./detector
    inputs:
      frames:
        source: camera/frames
        priority_lane: control
        deadline: 20ms
      status: astrs/status
",
        );
        let graph = graph_of(&manifest);
        let dataflow = astrs_wire::DataflowId::generate();
        let detector_node = manifest.nodes.iter().find(|n| n.id == "detector").unwrap();
        let detector_graph_node = graph.node(&astrs_graph::NodeId::new("detector")).unwrap();
        let spec = build_node_spawn_spec(
            dataflow,
            1,
            &manifest,
            detector_node,
            &graph,
            detector_graph_node,
            None,
        )
        .unwrap();

        let frames = spec
            .inputs
            .iter()
            .find(|input| input.id.as_str() == "frames")
            .expect("planned");
        assert_eq!(frames.priority_lane, WirePriorityLane::Control);
        assert_eq!(frames.deadline, Some(DurationMs::new(20)));

        // `status: astrs/status` names no `priority_lane:` at all — the
        // cluster-dispatch path must give it the same automatic control-lane
        // promotion the local planner does (§8.4), not silently demote it to
        // the manifest field's own default.
        let status = spec
            .inputs
            .iter()
            .find(|input| input.id.as_str() == "status")
            .expect("planned");
        assert_eq!(status.priority_lane, WirePriorityLane::Control);
        assert!(status.deadline.is_none());
    }

    #[test]
    fn priority_lane_to_wire_promotes_but_never_demotes_the_status_port() {
        assert_eq!(
            priority_lane_to_wire(astrs_manifest::PriorityLane::Control, false),
            WirePriorityLane::Control
        );
        assert_eq!(
            priority_lane_to_wire(astrs_manifest::PriorityLane::Data, false),
            WirePriorityLane::Data
        );
        assert_eq!(
            priority_lane_to_wire(astrs_manifest::PriorityLane::Data, true),
            WirePriorityLane::Control,
            "the status port's automatic promotion survives an unset field"
        );
        assert_eq!(
            priority_lane_to_wire(astrs_manifest::PriorityLane::Control, true),
            WirePriorityLane::Control
        );
    }

    /// Replaces `build_node_spawn_spec_omits_virtual_sourced_inputs`, which
    /// asserted the opposite and was wrong.
    ///
    /// A virtual source has no coordinator-brokered *route* — nothing on the
    /// network produces a timer tick — but the daemon still has to be told
    /// the input exists, because it receives a fully expanded specification
    /// and no manifest. Omitting it meant a cluster node whose only input was
    /// `astrs/timer/*` never ticked, while the same manifest worked under
    /// `astrs run` (whose local planner does include them).
    #[test]
    fn build_node_spawn_spec_carries_virtual_sourced_inputs() {
        let manifest = manifest(
            "\
nodes:
  - id: planner
    path: ./planner
    inputs:
      tick: astrs/timer/hz/50
",
        );
        let graph = graph_of(&manifest);
        let dataflow = astrs_wire::DataflowId::generate();
        let node = &manifest.nodes[0];
        let graph_node = graph.node(&astrs_graph::NodeId::new("planner")).unwrap();
        let spec =
            build_node_spawn_spec(dataflow, 1, &manifest, node, &graph, graph_node, None).unwrap();

        assert_eq!(spec.inputs.len(), 1, "{:?}", spec.inputs);
        let tick = &spec.inputs[0];
        assert_eq!(tick.id.as_str(), "tick");
        // The exact wire form the daemon decodes (`astrs_wire::virtual_source`),
        // spelled out rather than derived: this is a protocol agreement between
        // two processes, and a change to it must be visible in a diff.
        assert_eq!(tick.source.node().as_str(), "astrs");
        assert_eq!(tick.source.port().as_str(), "timer.hz.50");
        assert_eq!(
            astrs_wire::virtual_source_text(&tick.source),
            "astrs/timer/hz/50"
        );

        // …and still no route: a timer is served by the daemon that hosts the
        // node, not brokered across the cluster.
        let resolved = crate::placement::ResolvedPlacement::default();
        let routes = crate::placement::routes_for_node(
            dataflow,
            &astrs_graph::NodeId::new("planner"),
            &graph,
            &resolved,
        )
        .unwrap();
        assert!(routes.is_empty(), "{routes:?}");
    }

    /// A specification carrying a virtual input converts back to a *virtual*
    /// edge, not to an edge from a node called `astrs` that does not exist —
    /// the round trip `astrs node add`'s graph validation depends on.
    #[test]
    fn a_virtual_input_spec_converts_back_to_a_virtual_edge() {
        let input = InputSpec::new(
            DataId::new("tick").unwrap(),
            astrs_wire::virtual_port_ref("astrs/timer/millis/20").unwrap(),
        );
        match edge_for_input_spec(&input).from {
            EdgeSource::Virtual(source) => assert_eq!(source, "astrs/timer/millis/20"),
            other => panic!("expected a virtual edge, got {other:?}"),
        }

        let ordinary = InputSpec::new(
            DataId::new("frames").unwrap(),
            PortRef::from_parts("camera", "image").unwrap(),
        );
        match edge_for_input_spec(&ordinary).from {
            EdgeSource::NodeOutput { node, output } => {
                assert_eq!(node.as_str(), "camera");
                assert_eq!(output.as_str(), "image");
            }
            other => panic!("expected a producer edge, got {other:?}"),
        }
    }

    #[test]
    fn build_node_spawn_spec_uses_the_dataflow_working_dir_when_the_node_sets_none() {
        let manifest = manifest("nodes:\n  - id: a\n    path: ./a\n");
        let graph = graph_of(&manifest);
        let dataflow = astrs_wire::DataflowId::generate();
        let node = &manifest.nodes[0];
        let graph_node = graph.node(&astrs_graph::NodeId::new("a")).unwrap();
        let spec = build_node_spawn_spec(
            dataflow,
            2,
            &manifest,
            node,
            &graph,
            graph_node,
            Some("/graphs/demo"),
        )
        .unwrap();
        assert_eq!(spec.working_dir.as_deref(), Some("/graphs/demo"));
        assert_eq!(spec.generation, 2);
        assert!(spec.is_restart());
    }

    fn spawn_spec_with_ports(id: &str) -> NodeSpawnSpec {
        let mut spec = NodeSpawnSpec::new(
            astrs_wire::DataflowId::generate(),
            WireNodeId::new(id).unwrap(),
            1,
            NodeSource::Executable {
                path: format!("./{id}"),
            },
        );
        spec.outputs.push(
            OutputSpec::new(DataId::new("out").unwrap())
                .with_type(TypeUrn::new("std/core/v1/Int32").unwrap()),
        );
        let mut input = InputSpec::new(
            DataId::new("in").unwrap(),
            PortRef::new(
                WireNodeId::new("upstream").unwrap(),
                DataId::new("feed").unwrap(),
            ),
        )
        .with_type(TypeUrn::new("std/core/v1/Int32").unwrap());
        input.queue_size = 3;
        input.queue_policy = WireQueuePolicy::Backpressure;
        spec.inputs.push(input);
        spec.deploy.machine = Some(MachineName::new("robot-1").unwrap());
        spec.pattern = Some(NodePattern::ActionServer);
        spec
    }

    #[test]
    fn graph_node_for_spawn_spec_carries_every_field_over() {
        let spec = spawn_spec_with_ports("extra");
        let node = graph_node_for_spawn_spec(&spec);
        assert_eq!(node.id, astrs_graph::NodeId::new("extra"));
        assert_eq!(
            node.outputs
                .get(&astrs_graph::PortName::new("out".to_owned()))
                .and_then(|p| p.type_urn.as_ref())
                .map(astrs_manifest::Urn::as_str),
            Some("std/core/v1/Int32")
        );
        assert!(
            node.inputs
                .contains_key(&astrs_graph::PortName::new("in".to_owned()))
        );
        assert_eq!(node.pattern, Some(astrs_manifest::Pattern::ActionServer));
        assert_eq!(
            node.machine,
            astrs_graph::MachineId::Named("robot-1".to_owned())
        );
        assert!(node.spawns_process);
    }

    #[test]
    fn graph_node_for_spawn_spec_marks_a_dynamic_node_as_not_spawning_a_process() {
        let mut spec = spawn_spec_with_ports("attached");
        spec.source = NodeSource::Dynamic;
        assert!(!graph_node_for_spawn_spec(&spec).spawns_process);
    }

    #[test]
    fn topology_ops_for_add_node_wires_every_real_producer_input() {
        let spec = spawn_spec_with_ports("extra");
        let ops = topology_ops_for_add_node(&spec);
        assert_eq!(
            ops.len(),
            2,
            "one AddNode plus one AddEdge for the one real input"
        );
        assert!(
            matches!(&ops[0], astrs_graph::TopologyOp::AddNode { id, .. } if id.as_str() == "extra")
        );
        match &ops[1] {
            astrs_graph::TopologyOp::AddEdge { key, edge } => {
                assert_eq!(key.consumer, astrs_graph::NodeId::new("extra"));
                assert_eq!(key.input, astrs_graph::PortName::new("in".to_owned()));
                assert!(matches!(
                    &edge.from,
                    astrs_graph::EdgeSource::NodeOutput { node, output }
                        if node.as_str() == "upstream" && output.as_str() == "feed"
                ));
                assert_eq!(edge.queue.size, 3);
                assert_eq!(edge.queue.policy, astrs_manifest::QueuePolicy::Backpressure);
            }
            other => panic!("expected an AddEdge, got {other:?}"),
        }
    }

    #[test]
    fn topology_ops_for_add_node_applies_cleanly_to_a_graph_with_the_producer() {
        let manifest =
            manifest("nodes:\n  - id: upstream\n    path: ./upstream\n    outputs: [feed]\n");
        let graph = graph_of(&manifest);
        let spec = spawn_spec_with_ports("extra");
        let ops = topology_ops_for_add_node(&spec);
        let candidate = astrs_graph::apply(&graph, &ops).unwrap();
        assert!(candidate.node(&astrs_graph::NodeId::new("extra")).is_some());
    }

    #[test]
    fn topology_ops_for_add_node_is_rejected_when_the_producer_does_not_exist() {
        let manifest = manifest("nodes:\n  - id: solo\n    path: ./solo\n");
        let graph = graph_of(&manifest);
        let spec = spawn_spec_with_ports("extra");
        let ops = topology_ops_for_add_node(&spec);
        let err = astrs_graph::apply(&graph, &ops).unwrap_err();
        assert!(matches!(
            err,
            astrs_graph::ApplyError::DanglingProducer { .. }
        ));
    }

    #[test]
    fn topology_ops_for_remove_node_removes_every_edge_that_referenced_it() {
        let graph = graph_of(&manifest(
            "nodes:\n  - id: camera\n    path: ./camera\n    outputs: [frames]\n  \
             - id: detector\n    path: ./detector\n    inputs: { frames: camera/frames }\n",
        ));
        let ops = topology_ops_for_remove_node(&graph, &astrs_graph::NodeId::new("camera"));
        // The producer removal plus the one edge that referenced it.
        assert_eq!(ops.len(), 2);
        assert!(
            ops.iter()
                .any(|op| matches!(op, astrs_graph::TopologyOp::RemoveEdge { .. }))
        );
        assert!(
            matches!(ops.last(), Some(astrs_graph::TopologyOp::RemoveNode { id }) if id.as_str() == "camera")
        );
        // And it must actually apply cleanly -- the whole point of
        // collecting the edge removals up front.
        astrs_graph::apply(&graph, &ops).unwrap();
    }

    #[test]
    fn topology_ops_for_remove_node_on_an_isolated_node_is_just_the_one_op() {
        let graph = graph_of(&manifest("nodes:\n  - id: solo\n    path: ./solo\n"));
        let ops = topology_ops_for_remove_node(&graph, &astrs_graph::NodeId::new("solo"));
        assert_eq!(ops.len(), 1);
        astrs_graph::apply(&graph, &ops).unwrap();
    }

    #[test]
    fn pattern_and_queue_policy_inverses_round_trip() {
        assert_eq!(
            pattern_to_manifest(NodePattern::ServiceClient),
            astrs_manifest::Pattern::ServiceClient
        );
        assert_eq!(
            queue_policy_to_manifest(WireQueuePolicy::Backpressure),
            astrs_manifest::QueuePolicy::Backpressure
        );
        assert_eq!(
            queue_policy_to_manifest(WireQueuePolicy::DropOldest),
            astrs_manifest::QueuePolicy::DropOldest
        );
    }

    // ---- `expand_node_fragment`: `astrs node add`/`replace`'s client-side
    // manifest-fragment expansion ------------------------------------------

    #[test]
    fn expand_node_fragment_builds_a_spec_from_a_bare_node_yaml() {
        let dataflow = astrs_wire::DataflowId::generate();
        let spec = expand_node_fragment(
            dataflow,
            1,
            "id: extra\npath: ./extra\noutputs: [out]\nenv: { MODE: fast }\n",
        )
        .unwrap();
        assert_eq!(spec.dataflow, dataflow);
        assert_eq!(spec.node.as_str(), "extra");
        assert_eq!(spec.generation, 1);
        assert_eq!(spec.outputs.len(), 1);
        assert_eq!(spec.outputs[0].id.as_str(), "out");
        assert_eq!(spec.env.get("MODE").map(String::as_str), Some("fast"));
        assert!(matches!(spec.source, NodeSource::Executable { .. }));
    }

    #[test]
    fn expand_node_fragment_wires_an_input_to_a_producer_the_fragment_does_not_declare() {
        // The entire point of a dynamic add: the fragment names exactly
        // one node, and its input's producer is some other node already
        // running in the live dataflow — never present in this fragment.
        let dataflow = astrs_wire::DataflowId::generate();
        let spec = expand_node_fragment(
            dataflow,
            1,
            "id: detector\npath: ./detector\ninputs:\n  frames:\n    source: camera/image\n    queue_size: 5\n",
        )
        .unwrap();
        assert_eq!(spec.inputs.len(), 1);
        assert_eq!(spec.inputs[0].id.as_str(), "frames");
        assert_eq!(spec.inputs[0].source.to_string(), "camera/image");
        assert_eq!(spec.inputs[0].queue_size, 5);
    }

    #[test]
    fn expand_node_fragment_defaults_the_generation_and_dataflow_from_its_arguments() {
        let dataflow = astrs_wire::DataflowId::generate();
        let spec = expand_node_fragment(dataflow, 0, "id: a\npath: ./a\n").unwrap();
        assert_eq!(spec.dataflow, dataflow);
        assert_eq!(spec.generation, 0, "generation is the caller's to assign");
    }

    #[test]
    fn expand_node_fragment_rejects_malformed_yaml() {
        let dataflow = astrs_wire::DataflowId::generate();
        let err = expand_node_fragment(dataflow, 1, "not: [valid, node, fields").unwrap_err();
        assert!(matches!(err, CoordinatorError::InvalidArgument(_)));
    }

    #[test]
    fn expand_node_fragment_rejects_an_unknown_field() {
        // `#[serde(deny_unknown_fields)]` on `astrs_manifest::Node` applies
        // at this single-node level exactly as inside a full manifest.
        let dataflow = astrs_wire::DataflowId::generate();
        let err = expand_node_fragment(
            dataflow,
            1,
            "id: a\npath: ./a\nthis_field_does_not_exist: true\n",
        )
        .unwrap_err();
        assert!(matches!(err, CoordinatorError::InvalidArgument(_)));
    }

    #[test]
    fn expand_node_fragment_rejects_a_node_naming_no_source() {
        let dataflow = astrs_wire::DataflowId::generate();
        let err = expand_node_fragment(dataflow, 1, "id: a\n").unwrap_err();
        assert!(matches!(err, CoordinatorError::InvalidArgument(_)));
    }
}
