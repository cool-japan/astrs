//! Config → endpoints: the pure half of the bridge.
//!
//! [`plan`] takes the `ros2:` block and the node's own
//! [`NodeSpawnSpec`] and produces a [`BridgePlan`] — a list of endpoints
//! with every name, direction, port and QoS profile already decided. It
//! touches no socket, opens no participant and reads no file, which is what
//! makes the whole of §10.5's declarative surface unit-testable.
//!
//! # The three forms, and the one type field
//!
//! §10.5 shows three shapes of block. Two of them (`topic:` and `topics:`)
//! carry their own `message_type:`; the `service:`/`action:` form has no
//! type field of its own, because [`Ros2Config`] has exactly one —
//! `message_type:` — and that is the field a service or action block uses
//! for its type too:
//!
//! ```yaml
//!   ros2:
//!     compat: humble
//!     service: /add_two_ints
//!     message_type: example_interfaces/srv/AddTwoInts
//!     role: server
//! ```
//!
//! Reading `message_type:` as "the block's interface type" rather than
//! "the topic's message type" is the only reading that makes the
//! service/action form expressible at all, and it keeps the manifest schema
//! (which is frozen, deny-unknown-fields data) untouched.
//!
//! # Which AstRS port an entry bridges onto
//!
//! An entry needs a port on this node — an `outputs:` entry for anything
//! flowing ROS → AstRS, an `inputs:` entry for anything flowing the other
//! way. The manifest has no field naming it, so the port is *matched*
//! against what the node declares, in three passes, most specific first:
//!
//! 1. **By sanitized name.** `/scan` prefers a port called `scan`;
//!    `/turtle1/cmd_vel` prefers `turtle1_cmd_vel`.
//! 2. **By last segment.** `/turtle1/cmd_vel` also accepts `cmd_vel`.
//! 3. **By elimination.** If exactly one entry and exactly one declared
//!    port of that direction are still unmatched, they belong together —
//!    this is what makes the blueprint's own `outputs: [scan]` beside
//!    `topic: /scan` work when the author names the port something else
//!    entirely (`outputs: [laser]`).
//!
//! Anything left over is [`PlanError::NoSuchPort`], which prints the entry,
//! the name it looked for and every port the node actually declares.
//! Ambiguity is never resolved by declaration order: two unmatched entries
//! competing for two unmatched ports is an error, not a coin flip.

use std::collections::BTreeSet;

use astrs_manifest::{BridgeDirection, Qos as ManifestQos, Ros2Config, Ros2Role, Ros2Topic};
use astrs_ros2::action::ActionQos;
use astrs_ros2::names::validate::{ROOT_NAMESPACE, validate_namespace, validate_node_name};
use astrs_ros2::qos::QosProfile;
use astrs_ros2::qos::manifest::from_manifest_qos_with_base;
use astrs_rtps::discovery::RosCompat as RtpsCompat;
use astrs_wire::{DataId, NodeSpawnSpec};

use crate::error::PlanError;

/// One bridged topic.
#[derive(Debug, Clone, PartialEq)]
pub struct TopicBridge {
    /// The ROS 2 topic name, as it will be resolved against the namespace.
    pub topic: String,
    /// The ROS 2 message type, `pkg/msg/Type`.
    pub message_type: String,
    /// Which way it crosses.
    pub direction: BridgeDirection,
    /// The AstRS port it bridges onto.
    pub port: DataId,
    /// The DDS QoS to request.
    pub qos: QosProfile,
}

impl TopicBridge {
    /// How this entry names itself in a diagnostic.
    #[must_use]
    pub fn label(&self) -> String {
        format!("topic {}", self.topic)
    }
}

/// One bridged service.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceBridge {
    /// The ROS 2 service name.
    pub service: String,
    /// The ROS 2 service type, `pkg/srv/Type`.
    pub service_type: String,
    /// Which side of the exchange this bridge is.
    pub role: Ros2Role,
    /// The port requests travel on: an output when this bridge is the ROS
    /// *server* (a ROS caller's request becomes an AstRS message), an input
    /// when it is the ROS *client*.
    pub request_port: DataId,
    /// The port responses travel on — the mirror of
    /// [`Self::request_port`].
    pub response_port: DataId,
    /// The DDS QoS both halves use.
    pub qos: QosProfile,
}

impl ServiceBridge {
    /// How this entry names itself in a diagnostic.
    #[must_use]
    pub fn label(&self) -> String {
        format!("service {}", self.service)
    }
}

/// One bridged action.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionBridge {
    /// The ROS 2 action name.
    pub action: String,
    /// The ROS 2 action type, `pkg/action/Type`.
    pub action_type: String,
    /// Which side of the exchange this bridge is.
    pub role: Ros2Role,
    /// The port goals travel on: an output when this bridge is the ROS
    /// *server*, an input when it is the ROS *client*.
    pub goal_port: DataId,
    /// The port terminal results travel on — the mirror of
    /// [`Self::goal_port`].
    pub result_port: DataId,
    /// The port feedback travels on, when the node declared one.
    ///
    /// Optional because feedback is optional in the action protocol itself:
    /// a goal that never publishes feedback is a legal goal, and a graph
    /// that does not care is a legal graph.
    pub feedback_port: Option<DataId>,
    /// The five QoS profiles the action's endpoints use.
    pub qos: ActionQos,
}

impl ActionBridge {
    /// How this entry names itself in a diagnostic.
    #[must_use]
    pub fn label(&self) -> String {
        format!("action {}", self.action)
    }
}

/// Everything the bridge will create, decided.
#[derive(Debug, Clone, PartialEq)]
pub struct BridgePlan {
    /// The ROS 2 distribution's wire conventions (24- vs 16-byte GID).
    pub compat: RtpsCompat,
    /// The node name this bridge presents on the ROS graph.
    pub node_name: String,
    /// The namespace it mounts under.
    pub namespace: String,
    /// Every bridged topic, in declaration order.
    pub topics: Vec<TopicBridge>,
    /// Every bridged service.
    pub services: Vec<ServiceBridge>,
    /// Every bridged action.
    pub actions: Vec<ActionBridge>,
}

impl BridgePlan {
    /// How many endpoints this plan will create in total.
    #[must_use]
    pub fn endpoint_count(&self) -> usize {
        self.topics.len() + self.services.len() + self.actions.len()
    }

    /// Every ROS 2 interface type this plan needs, with the kind of
    /// interface it is, deduplicated and in a stable order.
    ///
    /// What [`crate::resolve`] is handed at startup: resolving up front
    /// means an unknown type fails before a single participant is created,
    /// which is the difference between a clear error and a bridge that
    /// silently drops every sample on one topic.
    #[must_use]
    pub fn required_types(&self) -> Vec<(&'static str, &str)> {
        let mut seen = BTreeSet::new();
        let mut types = Vec::new();
        for topic in &self.topics {
            if seen.insert(topic.message_type.as_str()) {
                types.push(("message", topic.message_type.as_str()));
            }
        }
        for service in &self.services {
            if seen.insert(service.service_type.as_str()) {
                types.push(("service", service.service_type.as_str()));
            }
        }
        for action in &self.actions {
            if seen.insert(action.action_type.as_str()) {
                types.push(("action", action.action_type.as_str()));
            }
        }
        types
    }
}

/// Turn a `ros2:` block plus a node's declared ports into a plan.
///
/// # Errors
///
/// Every variant of [`PlanError`]: a block that bridges nothing, a
/// half-written single-topic form, a `service:`/`action:` with no `role:`,
/// a port that cannot be matched, a port claimed twice, or a name that is
/// not a legal ROS 2 name.
pub fn plan(config: &Ros2Config, spec: &NodeSpawnSpec) -> Result<BridgePlan, PlanError> {
    let namespace = resolve_namespace(config)?;
    let node_name = resolve_node_name(config, spec.node.as_str())?;

    let mut outputs = PortPool::new(
        "output",
        spec.outputs.iter().map(|output| output.id.clone()),
    );
    let mut inputs = PortPool::new("input", spec.inputs.iter().map(|input| input.id.clone()));

    let entries = collect_entries(config)?;
    if entries.is_empty() {
        return Err(PlanError::Empty);
    }

    // Pass one: register every claim, so pass two (elimination) knows how
    // many claims are still open on each side.
    for entry in &entries {
        match entry {
            Entry::Topic(topic) => {
                let pool = pool_for(&mut outputs, &mut inputs, topic.direction);
                pool.claim(entry_label(entry), port_candidates(&topic.topic, &[]));
            }
            Entry::Service { service, role, .. } => {
                let base = sanitize_ros_name(service);
                let (request_pool, response_pool) = match role {
                    Ros2Role::Server => (&mut outputs, &mut inputs),
                    Ros2Role::Client => (&mut inputs, &mut outputs),
                };
                request_pool.claim(
                    format!("service {service} (request)"),
                    port_candidates(service, &[&format!("{base}_request"), "request"]),
                );
                response_pool.claim(
                    format!("service {service} (response)"),
                    port_candidates(service, &[&format!("{base}_response"), "response"]),
                );
            }
            Entry::Action { action, role, .. } => {
                let base = sanitize_ros_name(action);
                let (goal_pool, back_pool) = match role {
                    Ros2Role::Server => (&mut outputs, &mut inputs),
                    Ros2Role::Client => (&mut inputs, &mut outputs),
                };
                goal_pool.claim(
                    format!("action {action} (goal)"),
                    port_candidates(action, &[&format!("{base}_goal"), "goal"]),
                );
                back_pool.claim(
                    format!("action {action} (result)"),
                    port_candidates(action, &[&format!("{base}_result"), "result"]),
                );
                back_pool.claim_optional(
                    format!("action {action} (feedback)"),
                    vec![format!("{base}_feedback"), "feedback".to_owned()],
                );
            }
        }
    }

    outputs.resolve()?;
    inputs.resolve()?;

    // Pass two: read the assignments back in the same order they were made.
    let mut topics = Vec::new();
    let mut services = Vec::new();
    let mut actions = Vec::new();
    for entry in &entries {
        match entry {
            Entry::Topic(topic) => {
                let pool = pool_for(&mut outputs, &mut inputs, topic.direction);
                let port = pool.take()?;
                topics.push(TopicBridge {
                    topic: topic.topic.clone(),
                    message_type: topic.message_type.clone(),
                    direction: topic.direction,
                    port,
                    qos: topic_qos(config.qos.as_ref()),
                });
            }
            Entry::Service {
                service,
                service_type,
                role,
            } => {
                let (request_pool, response_pool) = match role {
                    Ros2Role::Server => (&mut outputs, &mut inputs),
                    Ros2Role::Client => (&mut inputs, &mut outputs),
                };
                let request_port = request_pool.take()?;
                let response_port = response_pool.take()?;
                services.push(ServiceBridge {
                    service: service.clone(),
                    service_type: service_type.clone(),
                    role: *role,
                    request_port,
                    response_port,
                    qos: service_qos(config.qos.as_ref()),
                });
            }
            Entry::Action {
                action,
                action_type,
                role,
            } => {
                let (goal_pool, back_pool) = match role {
                    Ros2Role::Server => (&mut outputs, &mut inputs),
                    Ros2Role::Client => (&mut inputs, &mut outputs),
                };
                let goal_port = goal_pool.take()?;
                let result_port = back_pool.take()?;
                let feedback_port = back_pool.take_optional();
                actions.push(ActionBridge {
                    action: action.clone(),
                    action_type: action_type.clone(),
                    role: *role,
                    goal_port,
                    result_port,
                    feedback_port,
                    qos: action_qos(config.qos.as_ref()),
                });
            }
        }
    }

    Ok(BridgePlan {
        compat: compat_of(config),
        node_name,
        namespace,
        topics,
        services,
        actions,
    })
}

/// The RTPS-side spelling of the manifest's `compat:`.
#[must_use]
pub const fn compat_of(config: &Ros2Config) -> RtpsCompat {
    match config.compat {
        astrs_manifest::RosCompat::Humble => RtpsCompat::Humble,
        astrs_manifest::RosCompat::Jazzy => RtpsCompat::Jazzy,
    }
}

/// The QoS a bridged topic uses.
#[must_use]
pub fn topic_qos(qos: Option<&ManifestQos>) -> QosProfile {
    qos.map_or_else(QosProfile::default, |qos| {
        from_manifest_qos_with_base(qos, QosProfile::default())
    })
}

/// The QoS a bridged service uses.
///
/// The base is `services_default` rather than `default`: a service whose
/// reply is dropped wedges its caller, and RELIABLE/KEEP_ALL is what `rmw`
/// gives one. A manifest `qos:` block still overrides field by field.
#[must_use]
pub fn service_qos(qos: Option<&ManifestQos>) -> QosProfile {
    qos.map_or_else(QosProfile::services_default, |qos| {
        from_manifest_qos_with_base(qos, QosProfile::services_default())
    })
}

/// The five profiles a bridged action uses.
///
/// A manifest `qos:` block tunes the *feedback* topic, which is the one an
/// application routinely wants best-effort at a high rate; the three
/// services and the transient-local status topic keep `rcl_action`'s
/// defaults, because a dropped goal response or a status a late-joining
/// client never sees is a protocol failure rather than a tuning choice.
#[must_use]
pub fn action_qos(qos: Option<&ManifestQos>) -> ActionQos {
    qos.map_or_else(ActionQos::default, |qos| {
        ActionQos::default().with_feedback(from_manifest_qos_with_base(qos, QosProfile::default()))
    })
}

/// One thing the block asks to bridge, before ports are matched.
#[derive(Debug, Clone, PartialEq)]
enum Entry {
    Topic(Ros2Topic),
    Service {
        service: String,
        service_type: String,
        role: Ros2Role,
    },
    Action {
        action: String,
        action_type: String,
        role: Ros2Role,
    },
}

/// How an entry names itself before it has a port.
fn entry_label(entry: &Entry) -> String {
    match entry {
        Entry::Topic(topic) => format!("topic {}", topic.topic),
        Entry::Service { service, .. } => format!("service {service}"),
        Entry::Action { action, .. } => format!("action {action}"),
    }
}

/// Read every entry the block declares, in a fixed order: the single-topic
/// form, then the bulk list, then the service, then the action.
fn collect_entries(config: &Ros2Config) -> Result<Vec<Entry>, PlanError> {
    let mut entries = Vec::new();

    match (&config.topic, &config.message_type, config.direction) {
        (Some(topic), Some(message_type), Some(direction)) => {
            entries.push(Entry::Topic(Ros2Topic {
                topic: topic.clone(),
                message_type: message_type.clone(),
                direction,
            }));
        }
        (Some(_), None, _) if config.service.is_none() && config.action.is_none() => {
            return Err(PlanError::IncompleteTopic {
                present: "topic",
                missing: "message_type",
            });
        }
        (Some(_), Some(_), None) => {
            return Err(PlanError::IncompleteTopic {
                present: "topic",
                missing: "direction",
            });
        }
        (None, _, Some(_)) if config.topics.is_empty() => {
            return Err(PlanError::IncompleteTopic {
                present: "direction",
                missing: "topic",
            });
        }
        _ => {}
    }

    entries.extend(config.topics.iter().cloned().map(Entry::Topic));

    if let Some(service) = &config.service {
        entries.push(Entry::Service {
            service: service.clone(),
            service_type: interface_type(config, "service")?,
            role: role_of(config, "service", service)?,
        });
    }
    if let Some(action) = &config.action {
        entries.push(Entry::Action {
            action: action.clone(),
            action_type: interface_type(config, "action")?,
            role: role_of(config, "action", action)?,
        });
    }

    Ok(entries)
}

/// The `message_type:` a service or action block carries, or a message
/// saying which field is missing.
fn interface_type(config: &Ros2Config, kind: &'static str) -> Result<String, PlanError> {
    config
        .message_type
        .clone()
        .ok_or(PlanError::IncompleteTopic {
            present: kind,
            missing: "message_type",
        })
}

/// The `role:` a service or action block carries.
fn role_of(config: &Ros2Config, kind: &'static str, name: &str) -> Result<Ros2Role, PlanError> {
    config.role.ok_or_else(|| PlanError::MissingRole {
        kind,
        name: name.to_owned(),
    })
}

/// The namespace this bridge mounts under, validated.
fn resolve_namespace(config: &Ros2Config) -> Result<String, PlanError> {
    let namespace = config
        .namespace
        .clone()
        .unwrap_or_else(|| ROOT_NAMESPACE.to_owned());
    let absolute = if namespace.starts_with('/') {
        namespace
    } else {
        format!("/{namespace}")
    };
    validate_namespace(&absolute).map_err(|fault| PlanError::BadRosName {
        name: absolute.clone(),
        kind: "namespace",
        reason: fault.to_string(),
    })?;
    Ok(absolute)
}

/// The ROS node name this bridge presents, validated.
///
/// The manifest node id is the default, sanitized: AstRS ids allow `-` and
/// `.` and ROS node names do not, so `lidar-in` becomes `lidar_in` rather
/// than failing a deployment over a hyphen the author never chose for ROS.
fn resolve_node_name(config: &Ros2Config, node_id: &str) -> Result<String, PlanError> {
    let name = config
        .node_name
        .clone()
        .unwrap_or_else(|| sanitize_node_name(node_id));
    validate_node_name(&name).map_err(|fault| PlanError::BadRosName {
        name: name.clone(),
        kind: "node",
        reason: fault.to_string(),
    })?;
    Ok(name)
}

/// A manifest node id as a legal ROS 2 node name.
///
/// `[A-Za-z0-9_.-]+` in, `[A-Za-z_][A-Za-z0-9_]*` out.
#[must_use]
pub fn sanitize_node_name(node_id: &str) -> String {
    let mut name = String::with_capacity(node_id.len());
    for character in node_id.chars() {
        if character.is_ascii_alphanumeric() {
            name.push(character);
        } else {
            name.push('_');
        }
    }
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        name.insert(0, '_');
    }
    name
}

/// A ROS 2 name as an AstRS port name: no leading slash, `/` → `_`.
///
/// `/scan` → `scan`, `/turtle1/cmd_vel` → `turtle1_cmd_vel`.
#[must_use]
pub fn sanitize_ros_name(name: &str) -> String {
    let trimmed = name.trim_start_matches('/');
    let mut port = String::with_capacity(trimmed.len());
    for character in trimmed.chars() {
        if character.is_ascii_alphanumeric() || character == '_' || character == '.' {
            port.push(character);
        } else {
            port.push('_');
        }
    }
    if port.is_empty() {
        "port".to_owned()
    } else {
        port
    }
}

/// The last `/`-separated segment of a ROS name, sanitized.
#[must_use]
pub fn last_segment(name: &str) -> String {
    let trimmed = name.trim_end_matches('/');
    let segment = trimmed.rsplit('/').next().unwrap_or(trimmed);
    sanitize_ros_name(segment)
}

/// The candidate port names one claim will accept, most specific first.
fn port_candidates(ros_name: &str, extra: &[&str]) -> Vec<String> {
    let mut candidates = Vec::with_capacity(extra.len() + 2);
    candidates.extend(extra.iter().map(|name| (*name).to_owned()));
    let full = sanitize_ros_name(ros_name);
    if !candidates.contains(&full) {
        candidates.push(full);
    }
    let last = last_segment(ros_name);
    if !candidates.contains(&last) {
        candidates.push(last);
    }
    candidates
}

/// Pick the pool a direction draws from.
fn pool_for<'pool>(
    outputs: &'pool mut PortPool,
    inputs: &'pool mut PortPool,
    direction: BridgeDirection,
) -> &'pool mut PortPool {
    match direction {
        BridgeDirection::ToAstrs => outputs,
        BridgeDirection::FromAstrs => inputs,
    }
}

/// One claim on a port of a given direction.
#[derive(Debug, Clone)]
struct Claim {
    /// The entry making it, for diagnostics.
    label: String,
    /// The names it will accept, most specific first.
    candidates: Vec<String>,
    /// Whether an unmatched claim is an error.
    required: bool,
    /// What it was matched to, once [`PortPool::resolve`] has run.
    assigned: Option<DataId>,
}

/// The declared ports of one direction, and the claims on them.
///
/// Written as a two-phase structure — every claim registered, *then*
/// resolved — precisely so that the elimination pass can know whether it is
/// looking at the last unmatched claim, which a streaming assignment could
/// never know.
#[derive(Debug)]
struct PortPool {
    /// `"output"` or `"input"`.
    direction: &'static str,
    /// Every port the node declares, in declaration order.
    declared: Vec<DataId>,
    /// The claims, in registration order.
    claims: Vec<Claim>,
    /// The read cursor [`PortPool::take`] advances.
    cursor: usize,
}

impl PortPool {
    fn new(direction: &'static str, declared: impl Iterator<Item = DataId>) -> Self {
        Self {
            direction,
            declared: declared.collect(),
            claims: Vec::new(),
            cursor: 0,
        }
    }

    fn claim(&mut self, label: String, candidates: Vec<String>) {
        self.claims.push(Claim {
            label,
            candidates,
            required: true,
            assigned: None,
        });
    }

    fn claim_optional(&mut self, label: String, candidates: Vec<String>) {
        self.claims.push(Claim {
            label,
            candidates,
            required: false,
            assigned: None,
        });
    }

    /// Match every claim to a declared port.
    fn resolve(&mut self) -> Result<(), PlanError> {
        let mut taken: Vec<Option<usize>> = vec![None; self.declared.len()];

        // Passes one and two: by name, most specific candidate first. A
        // claim's whole candidate list is tried before the next claim's, so
        // a specific name (`scan_request`) beats a generic one (`request`)
        // regardless of which claim asked first.
        let mut rank = 0;
        loop {
            let mut progressed = false;
            for (claim_index, claim) in self.claims.iter_mut().enumerate() {
                if claim.assigned.is_some() {
                    continue;
                }
                let Some(candidate) = claim.candidates.get(rank) else {
                    continue;
                };
                progressed = true;
                if let Some(port_index) = self
                    .declared
                    .iter()
                    .position(|port| port.as_str() == candidate)
                    && taken[port_index].is_none()
                {
                    taken[port_index] = Some(claim_index);
                    claim.assigned = Some(self.declared[port_index].clone());
                }
            }
            if !progressed {
                break;
            }
            rank += 1;
        }

        // Pass three: elimination, but only when it is unambiguous.
        let unmatched: Vec<usize> = self
            .claims
            .iter()
            .enumerate()
            .filter(|(_, claim)| claim.assigned.is_none() && claim.required)
            .map(|(index, _)| index)
            .collect();
        let free: Vec<usize> = taken
            .iter()
            .enumerate()
            .filter(|(_, owner)| owner.is_none())
            .map(|(index, _)| index)
            .collect();
        if unmatched.len() == 1 && free.len() == 1 {
            let claim_index = unmatched[0];
            let port_index = free[0];
            taken[port_index] = Some(claim_index);
            self.claims[claim_index].assigned = Some(self.declared[port_index].clone());
        }

        // Anything still unmatched and required is a configuration error.
        if let Some(claim) = self
            .claims
            .iter()
            .find(|claim| claim.assigned.is_none() && claim.required)
        {
            return Err(PlanError::NoSuchPort {
                entry: claim.label.clone(),
                article: if self.direction == "output" {
                    "an"
                } else {
                    "a"
                },
                direction: self.direction,
                wanted: claim
                    .candidates
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "?".to_owned()),
                declared: self.describe_declared(),
            });
        }

        Ok(())
    }

    /// The next assignment, in claim order.
    ///
    /// # Errors
    ///
    /// [`PlanError::DuplicatePort`] can never come from here — `resolve`
    /// guarantees one claim per port — but the signature keeps the caller
    /// honest about the cursor running past the claims, which would be a
    /// bug in this module rather than in a manifest.
    fn take(&mut self) -> Result<DataId, PlanError> {
        while let Some(claim) = self.claims.get(self.cursor) {
            self.cursor += 1;
            if let Some(port) = &claim.assigned {
                return Ok(port.clone());
            }
            if claim.required {
                break;
            }
        }
        Err(PlanError::NoSuchPort {
            entry: "the bridge plan".to_owned(),
            article: if self.direction == "output" {
                "an"
            } else {
                "a"
            },
            direction: self.direction,
            wanted: "one more port".to_owned(),
            declared: self.describe_declared(),
        })
    }

    /// The next assignment when the claim at the cursor is optional.
    fn take_optional(&mut self) -> Option<DataId> {
        let claim = self.claims.get(self.cursor)?;
        if claim.required {
            return None;
        }
        self.cursor += 1;
        claim.assigned.clone()
    }

    /// The declared ports, rendered for an error message.
    fn describe_declared(&self) -> String {
        if self.declared.is_empty() {
            return format!("no {}s at all", self.direction);
        }
        let names: Vec<&str> = self.declared.iter().map(DataId::as_str).collect();
        format!("{}s {}", self.direction, names.join(", "))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_manifest::{Durability, RosCompat};
    use astrs_wire::{DataflowId, InputSpec, NodeId, NodeSource, OutputSpec, PortRef};

    use super::*;

    fn spec(outputs: &[&str], inputs: &[&str]) -> NodeSpawnSpec {
        let mut spec = NodeSpawnSpec::new(
            DataflowId::from_u128(1),
            NodeId::new("lidar-in").unwrap(),
            0,
            NodeSource::Ros2Bridge {
                config: "{}".into(),
            },
        );
        for name in outputs {
            spec = spec.with_output(OutputSpec::new(DataId::new(*name).unwrap()));
        }
        for name in inputs {
            spec = spec.with_input(InputSpec::new(
                DataId::new(*name).unwrap(),
                PortRef::from_parts("upstream", "out").unwrap(),
            ));
        }
        spec
    }

    fn config(yaml: &str) -> Ros2Config {
        astrs_yaml::from_str(yaml).expect("the fixture parses")
    }

    #[test]
    fn the_blueprint_lidar_example_plans_one_subscription() {
        let cfg = config(
            "\
compat: humble
topic: /scan
message_type: sensor_msgs/msg/LaserScan
direction: to_astrs
qos: { reliable: true, keep_last: 10 }
",
        );
        let plan = plan(&cfg, &spec(&["scan"], &[])).unwrap();

        assert_eq!(plan.compat, RtpsCompat::Humble);
        assert_eq!(plan.node_name, "lidar_in", "the id's hyphen is sanitized");
        assert_eq!(plan.namespace, "/");
        assert_eq!(plan.topics.len(), 1);
        assert_eq!(plan.services.len(), 0);
        assert_eq!(plan.actions.len(), 0);

        let topic = &plan.topics[0];
        assert_eq!(topic.topic, "/scan");
        assert_eq!(topic.message_type, "sensor_msgs/msg/LaserScan");
        assert_eq!(topic.direction, BridgeDirection::ToAstrs);
        assert_eq!(topic.port.as_str(), "scan");
        assert!(topic.qos.is_reliable());
        assert_eq!(plan.endpoint_count(), 1);
    }

    #[test]
    fn the_bulk_form_plans_one_endpoint_per_entry_in_both_directions() {
        let cfg = config(
            "\
compat: jazzy
topics:
  - topic: /scan
    message_type: sensor_msgs/msg/LaserScan
    direction: to_astrs
  - topic: /cmd_vel
    message_type: geometry_msgs/msg/Twist
    direction: from_astrs
",
        );
        let plan = plan(&cfg, &spec(&["scan"], &["cmd_vel"])).unwrap();

        assert_eq!(plan.compat, RtpsCompat::Jazzy);
        assert_eq!(plan.topics.len(), 2);
        assert_eq!(plan.topics[0].port.as_str(), "scan");
        assert_eq!(plan.topics[0].direction, BridgeDirection::ToAstrs);
        assert_eq!(plan.topics[1].port.as_str(), "cmd_vel");
        assert_eq!(plan.topics[1].direction, BridgeDirection::FromAstrs);
    }

    #[test]
    fn a_namespaced_topic_matches_its_last_segment() {
        let cfg = config(
            "\
compat: humble
topic: /turtle1/cmd_vel
message_type: geometry_msgs/msg/Twist
direction: from_astrs
",
        );
        let by_leaf = plan(&cfg, &spec(&[], &["cmd_vel"])).unwrap();
        assert_eq!(by_leaf.topics[0].port.as_str(), "cmd_vel");

        // …and its fully flattened spelling, when that is what is declared.
        let by_full = plan(&cfg, &spec(&[], &["turtle1_cmd_vel"])).unwrap();
        assert_eq!(by_full.topics[0].port.as_str(), "turtle1_cmd_vel");
    }

    #[test]
    fn a_single_entry_claims_a_differently_named_single_port() {
        let cfg = config(
            "\
compat: humble
topic: /scan
message_type: sensor_msgs/msg/LaserScan
direction: to_astrs
",
        );
        let plan = plan(&cfg, &spec(&["laser"], &[])).unwrap();
        assert_eq!(
            plan.topics[0].port.as_str(),
            "laser",
            "elimination matches the one entry to the one port"
        );
    }

    #[test]
    fn two_unmatched_entries_are_an_error_rather_than_a_guess() {
        let cfg = config(
            "\
compat: humble
topics:
  - topic: /a
    message_type: std_msgs/msg/Bool
    direction: to_astrs
  - topic: /b
    message_type: std_msgs/msg/Bool
    direction: to_astrs
",
        );
        let error = plan(&cfg, &spec(&["one", "two"], &[])).unwrap_err();
        match error {
            PlanError::NoSuchPort {
                entry, declared, ..
            } => {
                assert!(entry.contains("topic /a"), "{entry}");
                assert!(declared.contains("one, two"), "{declared}");
            }
            other => panic!("expected NoSuchPort, got {other}"),
        }
    }

    #[test]
    fn a_topic_with_no_matching_port_names_what_the_node_declares() {
        let cfg = config(
            "\
compat: humble
topic: /scan
message_type: sensor_msgs/msg/LaserScan
direction: to_astrs
",
        );
        let error = plan(&cfg, &spec(&[], &[])).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("topic /scan"), "{text}");
        assert!(text.contains("no outputs at all"), "{text}");
    }

    #[test]
    fn a_block_that_bridges_nothing_is_refused() {
        let cfg = config("compat: humble\n");
        assert!(matches!(
            plan(&cfg, &spec(&[], &[])).unwrap_err(),
            PlanError::Empty
        ));
    }

    #[test]
    fn a_topic_without_a_message_type_names_the_missing_field() {
        let cfg = config("compat: humble\ntopic: /scan\ndirection: to_astrs\n");
        match plan(&cfg, &spec(&["scan"], &[])).unwrap_err() {
            PlanError::IncompleteTopic { present, missing } => {
                assert_eq!(present, "topic");
                assert_eq!(missing, "message_type");
            }
            other => panic!("expected IncompleteTopic, got {other}"),
        }
    }

    #[test]
    fn a_topic_without_a_direction_names_the_missing_field() {
        let cfg = config("compat: humble\ntopic: /scan\nmessage_type: std_msgs/msg/Bool\n");
        match plan(&cfg, &spec(&["scan"], &[])).unwrap_err() {
            PlanError::IncompleteTopic { missing, .. } => assert_eq!(missing, "direction"),
            other => panic!("expected IncompleteTopic, got {other}"),
        }
    }

    #[test]
    fn a_service_server_takes_requests_out_and_responses_in() {
        let cfg = config(
            "\
compat: humble
service: /add_two_ints
message_type: example_interfaces/srv/AddTwoInts
role: server
",
        );
        let plan = plan(
            &cfg,
            &spec(&["add_two_ints_request"], &["add_two_ints_response"]),
        )
        .unwrap();

        assert_eq!(plan.services.len(), 1);
        let service = &plan.services[0];
        assert_eq!(service.service, "/add_two_ints");
        assert_eq!(service.service_type, "example_interfaces/srv/AddTwoInts");
        assert_eq!(service.role, Ros2Role::Server);
        assert_eq!(service.request_port.as_str(), "add_two_ints_request");
        assert_eq!(service.response_port.as_str(), "add_two_ints_response");
        assert!(service.qos.is_reliable(), "services default to RELIABLE");
    }

    #[test]
    fn a_service_client_mirrors_the_servers_port_directions() {
        let cfg = config(
            "\
compat: humble
service: /add_two_ints
message_type: example_interfaces/srv/AddTwoInts
role: client
",
        );
        let plan = plan(&cfg, &spec(&["response"], &["request"])).unwrap();
        let service = &plan.services[0];
        assert_eq!(service.role, Ros2Role::Client);
        assert_eq!(
            service.request_port.as_str(),
            "request",
            "a client sends requests, so the request port is an input"
        );
        assert_eq!(service.response_port.as_str(), "response");
    }

    #[test]
    fn a_service_without_a_role_says_which_field_is_missing() {
        let cfg = config(
            "compat: humble\nservice: /add_two_ints\nmessage_type: example_interfaces/srv/AddTwoInts\n",
        );
        match plan(&cfg, &spec(&["request"], &["response"])).unwrap_err() {
            PlanError::MissingRole { kind, name } => {
                assert_eq!(kind, "service");
                assert_eq!(name, "/add_two_ints");
            }
            other => panic!("expected MissingRole, got {other}"),
        }
    }

    #[test]
    fn an_action_client_takes_goals_in_and_feedback_and_results_out() {
        let cfg = config(
            "\
compat: humble
action: /fibonacci
message_type: example_interfaces/action/Fibonacci
role: client
",
        );
        let plan = plan(
            &cfg,
            &spec(
                &["fibonacci_result", "fibonacci_feedback"],
                &["fibonacci_goal"],
            ),
        )
        .unwrap();

        assert_eq!(plan.actions.len(), 1);
        let action = &plan.actions[0];
        assert_eq!(action.role, Ros2Role::Client);
        assert_eq!(action.goal_port.as_str(), "fibonacci_goal");
        assert_eq!(action.result_port.as_str(), "fibonacci_result");
        assert_eq!(
            action.feedback_port.as_ref().map(DataId::as_str),
            Some("fibonacci_feedback")
        );
    }

    #[test]
    fn an_action_without_a_feedback_port_still_plans() {
        let cfg = config(
            "\
compat: humble
action: /fibonacci
message_type: example_interfaces/action/Fibonacci
role: server
",
        );
        let plan = plan(&cfg, &spec(&["goal"], &["result"])).unwrap();
        let action = &plan.actions[0];
        assert_eq!(action.goal_port.as_str(), "goal");
        assert_eq!(action.result_port.as_str(), "result");
        assert_eq!(action.feedback_port, None, "feedback is optional");
    }

    #[test]
    fn the_required_types_are_deduplicated_and_kind_tagged() {
        let cfg = config(
            "\
compat: humble
topics:
  - topic: /a
    message_type: std_msgs/msg/Bool
    direction: to_astrs
  - topic: /b
    message_type: std_msgs/msg/Bool
    direction: to_astrs
  - topic: /c
    message_type: std_msgs/msg/String
    direction: to_astrs
",
        );
        let plan = plan(&cfg, &spec(&["a", "b", "c"], &[])).unwrap();
        assert_eq!(
            plan.required_types(),
            vec![
                ("message", "std_msgs/msg/Bool"),
                ("message", "std_msgs/msg/String")
            ]
        );
    }

    #[test]
    fn the_namespace_is_made_absolute_and_validated() {
        let cfg = config(
            "\
compat: humble
namespace: robot1
topic: /scan
message_type: std_msgs/msg/Bool
direction: to_astrs
",
        );
        let plan = plan(&cfg, &spec(&["scan"], &[])).unwrap();
        assert_eq!(plan.namespace, "/robot1");
    }

    #[test]
    fn an_illegal_namespace_is_refused_by_name() {
        let cfg = config(
            "\
compat: humble
namespace: \"has space\"
topic: /scan
message_type: std_msgs/msg/Bool
direction: to_astrs
",
        );
        match plan(&cfg, &spec(&["scan"], &[])).unwrap_err() {
            PlanError::BadRosName { kind, name, .. } => {
                assert_eq!(kind, "namespace");
                assert_eq!(name, "/has space");
            }
            other => panic!("expected BadRosName, got {other}"),
        }
    }

    #[test]
    fn an_explicit_node_name_wins_over_the_manifest_id() {
        let cfg = config(
            "\
compat: humble
node_name: lidar_bridge
topic: /scan
message_type: std_msgs/msg/Bool
direction: to_astrs
",
        );
        let plan = plan(&cfg, &spec(&["scan"], &[])).unwrap();
        assert_eq!(plan.node_name, "lidar_bridge");
    }

    #[test]
    fn the_manifest_qos_block_reaches_the_profile() {
        let cfg = config(
            "\
compat: humble
topic: /scan
message_type: std_msgs/msg/Bool
direction: to_astrs
qos:
  reliable: false
  durability: transient-local
  keep_last: 42
",
        );
        let plan = plan(&cfg, &spec(&["scan"], &[])).unwrap();
        let qos = &plan.topics[0].qos;
        assert!(!qos.is_reliable());
        assert!(qos.is_transient_local());
        assert_eq!(qos.depth(), Some(42));
    }

    #[test]
    fn an_action_qos_block_tunes_feedback_and_leaves_the_services_alone() {
        let cfg = config(
            "\
compat: humble
action: /fibonacci
message_type: example_interfaces/action/Fibonacci
role: server
qos: { reliable: false }
",
        );
        let plan = plan(&cfg, &spec(&["goal"], &["result"])).unwrap();
        let qos = &plan.actions[0].qos;
        assert!(!qos.feedback.is_reliable(), "the block tuned feedback");
        assert!(
            qos.goal_service.is_reliable(),
            "the services keep rcl's default"
        );
        assert!(qos.status.is_transient_local());
    }

    #[test]
    fn sanitizing_a_ros_name_flattens_the_slashes() {
        assert_eq!(sanitize_ros_name("/scan"), "scan");
        assert_eq!(sanitize_ros_name("/turtle1/cmd_vel"), "turtle1_cmd_vel");
        assert_eq!(sanitize_ros_name("/"), "port");
        assert_eq!(last_segment("/turtle1/cmd_vel"), "cmd_vel");
    }

    #[test]
    fn sanitizing_a_node_name_repairs_a_hyphen_and_a_leading_digit() {
        assert_eq!(sanitize_node_name("lidar-in"), "lidar_in");
        assert_eq!(sanitize_node_name("bridge.one"), "bridge_one");
        assert_eq!(sanitize_node_name("2fast"), "_2fast");
        assert_eq!(sanitize_node_name(""), "_");
    }

    #[test]
    fn the_durability_spelling_survives_the_manifest_round_trip() {
        let qos = ManifestQos {
            durability: Some(Durability::TransientLocal),
            ..ManifestQos::default()
        };
        assert!(topic_qos(Some(&qos)).is_transient_local());
        assert!(!topic_qos(None).is_transient_local());
    }

    #[test]
    fn both_distributions_map_onto_their_rtps_spelling() {
        let mut cfg = config("compat: humble\n");
        assert_eq!(compat_of(&cfg), RtpsCompat::Humble);
        cfg.compat = RosCompat::Jazzy;
        assert_eq!(compat_of(&cfg), RtpsCompat::Jazzy);
    }
}
