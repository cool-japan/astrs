//! Action endpoints: five ROS 2 endpoints, one AstRS correlation scheme.
//!
//! An action is not a primitive: it is three services (`send_goal`,
//! `cancel_goal`, `get_result`) and two topics (`feedback`, `status`), all
//! under `<action>/_action/…`. §9.4 carries the same idea on ordinary edges
//! with a `goal_id` and a `goal_status` FSM
//! (`Accepted→Executing→{Succeeded,Aborted,Canceled}`), so bridging one is —
//! exactly as for [`crate::service`] — a translation between two
//! correlation schemes rather than a new subsystem.
//!
//! ```text
//!   role: server (the AstRS graph does the work)
//!   ───────────────────────────────────────────
//!   rq/…send_goal   ─▶ goal_port      meta: goal_id, goal_status=accepted
//!                   ◀─ (accepted)
//!   result_port     ─▶ rr/…get_result meta: goal_id, goal_status=terminal
//!   feedback_port   ─▶ rt/…feedback
//!   rq/…cancel_goal ─▶ (goal marked canceling, status republished)
//!
//!   role: client (the AstRS graph issues goals)
//!   ───────────────────────────────────────────
//!   goal_port     ─▶ rq/…send_goal
//!                 ◀─ accepted ─▶ rq/…get_result (issued automatically)
//!   rr/…get_result ─▶ result_port     meta: goal_id, goal_status
//!   rt/…feedback   ─▶ feedback_port   meta: goal_id
//! ```
//!
//! # What crosses the AstRS edge
//!
//! The *wire* types, as columnar batches: `<Action>_SendGoal_Request`
//! (which is `{ goal_id, goal }`), `<Action>_GetResult_Response` (`{ status,
//! result }`) and `<Action>_FeedbackMessage` (`{ goal_id, feedback }`), each
//! with `goal_id` and, where it applies, `goal_status` in
//! [`astrs_wire::Metadata`]. Carrying the wire type rather than a
//! re-modelled one means the whole action surface goes through
//! [`crate::codec::registry`] with no per-action Rust code, and §11.2's
//! eviction immunity applies automatically — those are exactly the keys
//! [`astrs_wire::metadata::keys::CORRELATION`] lists.
//!
//! # Reading a goal id without a type
//!
//! Three of the five wire types begin with the goal's UUID, and one begins
//! with a status byte. That is not luck: `astrs-idl`'s action code generator
//! emits `SendGoal_Request { goal_id, goal }`, `GetResult_Request { goal_id
//! }`, `FeedbackMessage { goal_id, feedback }` and `GetResult_Response {
//! status, result }` in that field order, and a `unique_identifier_msgs/UUID`
//! is `uint8[16]` — sixteen octets at alignment one, so it sits at CDR
//! offset zero with no padding in front of it. [`goal_id_of`] and
//! [`status_of`] read those octets directly, which is what lets one
//! implementation serve every action rather than one per generated marker.
//!
//! # Three limits, stated rather than hidden
//!
//! 1. **A client bridge cannot cancel.** [`handle_cancel_request`] serves
//!    the `role: server` side; a `role: client` bridge creates its
//!    `cancel_goal` request writer (so the endpoint exists on the graph)
//!    but nothing writes to it, because the AstRS side has no port that
//!    means "cancel" — §9.4's `goal_status` FSM carries `Canceling` as a
//!    *state*, not as a request. Wiring it needs a manifest field naming a
//!    cancel input, which §10.5's schema does not have.
//! 2. **The auto-issued `get_result` request is written under the
//!    `send_goal` writer's GUID.** [`handle_goal_reply`] mints its
//!    correlation identity from `ActionEndpointSet::client_guid`, which
//!    is the goal publisher's, and then writes on the *result* publisher.
//!    Both halves of this bridge agree, so an AstRS-to-AstRS action works;
//!    a stock `rcl_action` server that keys replies on the requesting
//!    writer's GUID would answer a GUID this bridge does not read on.
//!    Fixing it means carrying a per-endpoint GUID rather than one.
//! 3. **A `role: client` bridge ignores the `status` topic.** The graph
//!    learns a goal's outcome from `get_result`, which is authoritative;
//!    the status array is a summary a bridge would have to re-model to
//!    forward, and §9.4 already carries `goal_status` on the result.

use std::collections::BTreeMap;
use std::sync::Arc;

use astrs_cdr::{CdrWriter, Encoding};
use astrs_manifest::Ros2Role;
use astrs_node_api::{Node, Payload, RawOutput};
use astrs_ros2::action::GoalStatus;
use astrs_ros2::msg::action_msgs::{
    CancelGoalResponse, GoalInfo, GoalStatus as WireGoalStatus, GoalStatusArray,
};
use astrs_ros2::msg::builtin_interfaces::Time;
use astrs_ros2::msg::unique_identifier_msgs::UUID;
use astrs_ros2::names::mangle::{ActionEndpoint, action_name};
use astrs_ros2::names::{FullName, TopicKind};
use astrs_ros2::node::Ros2Node;
use astrs_ros2::pubsub::{MessageInfo, RawPublisher, RawSubscription};
use astrs_ros2::service::SampleIdentity;
use astrs_ros2::time::RosTime;
use astrs_rtps::structure::Guid;
use astrs_wire::Metadata;

use crate::codec::registry::action_endpoint_type_name;
use crate::codec::{ENCAPSULATION_LEN, encode_value, join_identity, split_identity};
use crate::error::{BridgeError, BridgeResult, ResolveError};
use crate::plan::ActionBridge;
use crate::resolve::{CANCEL_GOAL_SERVICE, GOAL_STATUS_ARRAY, ResolvedPlan, TypeBinding};
use crate::service::static_name;
use crate::topic::{EndpointSlot, inbound_metadata};

/// Octets a `unique_identifier_msgs/UUID` occupies: `uint8[16]`.
pub const GOAL_ID_LEN: usize = 16;

/// The most goals one endpoint tracks before the oldest is forgotten.
///
/// Same reasoning as [`crate::service::MAX_PENDING`]: goal state is
/// unbounded state driven by a remote peer.
pub const MAX_GOALS: usize = 64;

/// The goal id at the start of a `SendGoal_Request`, `GetResult_Request` or
/// `FeedbackMessage` body.
///
/// `body` is a standalone CDR payload — encapsulation header included, the
/// service correlation header already stripped by
/// [`crate::codec::split_identity`].
#[must_use]
pub fn goal_id_of(body: &[u8]) -> Option<[u8; GOAL_ID_LEN]> {
    let bytes = body.get(ENCAPSULATION_LEN..ENCAPSULATION_LEN + GOAL_ID_LEN)?;
    let mut id = [0_u8; GOAL_ID_LEN];
    id.copy_from_slice(bytes);
    Some(id)
}

/// The status byte at the start of a `GetResult_Response` body.
#[must_use]
pub fn status_of(body: &[u8]) -> Option<GoalStatus> {
    let raw = body.get(ENCAPSULATION_LEN).copied()?;
    #[expect(
        clippy::cast_possible_wrap,
        reason = "the field is an IDL `int8`; the octet is its two's-complement form"
    )]
    Some(GoalStatus::from_code(raw as i8))
}

/// A goal id as the `goal_id` metadata value: lower-case hex, no dashes.
///
/// Hex rather than the RFC 4122 dashed form because `goal_id` is a
/// [`astrs_wire::ParamKey`]-adjacent *value* that ends up in logs, `.arec`
/// entries and `astrs top`, and a form with no separators cannot be
/// mis-split by any of them.
#[must_use]
pub fn render_goal_id(id: &[u8; GOAL_ID_LEN]) -> String {
    let mut rendered = String::with_capacity(GOAL_ID_LEN * 2);
    for octet in id {
        rendered.push(hex_digit(octet >> 4));
        rendered.push(hex_digit(octet & 0x0f));
    }
    rendered
}

/// Parse the form [`render_goal_id`] produces.
#[must_use]
pub fn parse_goal_id(rendered: &str) -> Option<[u8; GOAL_ID_LEN]> {
    if rendered.len() != GOAL_ID_LEN * 2 {
        return None;
    }
    let bytes = rendered.as_bytes();
    let mut id = [0_u8; GOAL_ID_LEN];
    for (index, slot) in id.iter_mut().enumerate() {
        let high = hex_value(*bytes.get(index * 2)?)?;
        let low = hex_value(*bytes.get(index * 2 + 1)?)?;
        *slot = (high << 4) | low;
    }
    Some(id)
}

/// One nibble as a lower-case hex digit.
const fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'a' + nibble - 10) as char,
    }
}

/// One lower-case (or upper-case) hex digit as a nibble.
const fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// What the bridge knows about one in-flight goal.
#[derive(Debug, Clone)]
struct GoalState {
    /// Where it is in the §9.4 FSM.
    status: GoalStatus,
    /// The encoded `GetResult_Response` body, once the graph has produced
    /// one.
    result: Option<Vec<u8>>,
    /// `get_result` requests parked until that happens.
    parked: Vec<SampleIdentity>,
    /// When the goal was accepted, for the status array.
    accepted_at: RosTime,
}

impl GoalState {
    fn new(accepted_at: RosTime) -> Self {
        Self {
            status: GoalStatus::Accepted,
            result: None,
            parked: Vec::new(),
            accepted_at,
        }
    }
}

/// One bridged action, in whichever role the manifest asked for.
#[derive(Debug)]
pub struct ActionEndpointSet {
    /// What the plan said.
    pub bridge: ActionBridge,
    /// The `send_goal` publication: the reply half for a server, the
    /// request half for a client.
    pub goal_publisher: RawPublisher,
    /// The `cancel_goal` publication, same rule.
    pub cancel_publisher: RawPublisher,
    /// The `get_result` publication, same rule.
    pub result_publisher: RawPublisher,
    /// The `feedback` publication — a server publishes feedback; a client
    /// only subscribes, and holds this to keep the topic in the graph.
    pub feedback_publisher: Option<RawPublisher>,
    /// The `status` publication — a server only.
    pub status_publisher: Option<RawPublisher>,
    /// The AstRS output goals leave on (server), or results arrive on
    /// (client).
    pub primary_output: RawOutput,
    /// The AstRS output feedback arrives on (client only).
    pub feedback_output: Option<RawOutput>,
    /// How the five wire types are carried.
    pub wire: WireBindings,
    /// The three wire type names, promoted to `'static` once at open time.
    ///
    /// See [`crate::service::static_name`]: promoting a discovered type's
    /// owned name per *message* would leak without bound on a busy action,
    /// so it happens per *endpoint* instead.
    pub names: WireNames,
    /// Goal state, by goal id.
    goals: BTreeMap<[u8; GOAL_ID_LEN], GoalState>,
    /// Client-side: which goal a minted `send_goal`/`get_result` identity
    /// belongs to.
    correlations: BTreeMap<SampleIdentity, [u8; GOAL_ID_LEN]>,
    /// The GUID this endpoint's requests are written under.
    client_guid: Guid,
    /// The next client-local request sequence number.
    next_sequence: i64,
}

/// The three wire type names an action's hot paths need, already `'static`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireNames {
    /// `<Action>_SendGoal_Request`.
    pub send_goal_request: &'static str,
    /// `<Action>_GetResult_Response`.
    pub get_result_response: &'static str,
    /// `<Action>_FeedbackMessage`.
    pub feedback_message: &'static str,
}

/// The bindings an action's five synthesized wire types resolved to.
#[derive(Debug, Clone)]
pub struct WireBindings {
    /// `<Action>_SendGoal_Request`.
    pub send_goal_request: TypeBinding,
    /// `<Action>_GetResult_Response`.
    pub get_result_response: TypeBinding,
    /// `<Action>_FeedbackMessage`.
    pub feedback_message: TypeBinding,
}

impl ActionEndpointSet {
    /// How many goals this endpoint is tracking.
    #[must_use]
    pub fn goal_count(&self) -> usize {
        self.goals.len()
    }

    /// The AstRS input port that drives this endpoint's outbound direction.
    #[must_use]
    pub const fn primary_input(&self) -> &astrs_wire::DataId {
        match self.bridge.role {
            Ros2Role::Server => &self.bridge.result_port,
            Ros2Role::Client => &self.bridge.goal_port,
        }
    }

    /// Mint the next client-local request identity.
    fn mint(&mut self) -> SampleIdentity {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        SampleIdentity::from_guid(self.client_guid, sequence)
    }

    /// Record a goal, evicting the oldest when the table is full.
    fn track(&mut self, id: [u8; GOAL_ID_LEN], state: GoalState) {
        if self.goals.len() >= MAX_GOALS
            && let Some(oldest) = self.goals.keys().next().copied()
        {
            self.goals.remove(&oldest);
            tracing::warn!(
                action = %self.bridge.action,
                "the goal table is full; forgetting the oldest goal"
            );
        }
        self.goals.insert(id, state);
    }

    /// The `action_msgs/msg/GoalStatusArray` this endpoint would publish.
    #[must_use]
    fn status_array(&self) -> GoalStatusArray {
        GoalStatusArray {
            status_list: self
                .goals
                .iter()
                .map(|(id, state)| WireGoalStatus {
                    goal_info: GoalInfo {
                        goal_id: UUID { uuid: *id },
                        stamp: Time {
                            sec: state.accepted_at.sec,
                            nanosec: state.accepted_at.nanosec,
                        },
                    },
                    status: state.status.code(),
                })
                .collect(),
        }
    }
}

/// Every DDS subscription one action endpoint pumps, with its slot.
pub type ActionSubscriptions = Vec<(EndpointSlot, Arc<RawSubscription>)>;

/// Create all five ROS 2 endpoints for one bridged action.
///
/// # Errors
///
/// [`BridgeError::Ros2`] when an endpoint cannot be created,
/// [`BridgeError::Node`] when an AstRS port does not exist, and
/// [`BridgeError::Resolve`] when the plan did not bind one of the five wire
/// types (which [`crate::resolve::resolve_plan`] rules out).
pub async fn open(
    node: &mut Node,
    ros: &Ros2Node,
    resolved: &ResolvedPlan,
    bridge: ActionBridge,
    index: usize,
) -> BridgeResult<(ActionEndpointSet, ActionSubscriptions)> {
    let wire = WireBindings {
        send_goal_request: binding(resolved, &bridge.action_type, "SendGoal_Request")?,
        get_result_response: binding(resolved, &bridge.action_type, "GetResult_Response")?,
        feedback_message: binding(resolved, &bridge.action_type, "FeedbackMessage")?,
    };
    let names = WireNames {
        send_goal_request: static_name(wire.send_goal_request.ros_type_name()),
        get_result_response: static_name(wire.get_result_response.ros_type_name()),
        feedback_message: static_name(wire.feedback_message.ros_type_name()),
    };
    let send_goal_response = binding(resolved, &bridge.action_type, "SendGoal_Response")?;
    let get_result_request = binding(resolved, &bridge.action_type, "GetResult_Request")?;
    let cancel_request = named_binding(
        resolved,
        &crate::codec::registry::request_type_name(CANCEL_GOAL_SERVICE),
    )?;
    let cancel_response = named_binding(
        resolved,
        &crate::codec::registry::response_type_name(CANCEL_GOAL_SERVICE),
    )?;
    let status_binding = named_binding(resolved, GOAL_STATUS_ARRAY)?;

    let server = bridge.role == Ros2Role::Server;
    let mut subscriptions = ActionSubscriptions::new();

    // The three services. A server publishes the reply halves and
    // subscribes to the request halves; a client does the mirror image.
    let (goal_publisher, goal_subscription) = service_pair(
        ros,
        &bridge.action,
        ActionEndpoint::SendGoal,
        server,
        get_result_or(&wire.send_goal_request, &send_goal_response, server),
        bridge.qos.goal_service,
    )
    .await?;
    subscriptions.push((
        if server {
            EndpointSlot::ActionGoalRequest(index)
        } else {
            EndpointSlot::ActionGoalReply(index)
        },
        Arc::new(goal_subscription),
    ));

    let (cancel_publisher, cancel_subscription) = service_pair(
        ros,
        &bridge.action,
        ActionEndpoint::CancelGoal,
        server,
        get_result_or(&cancel_request, &cancel_response, server),
        bridge.qos.cancel_service,
    )
    .await?;
    subscriptions.push((
        if server {
            EndpointSlot::ActionCancelRequest(index)
        } else {
            EndpointSlot::ActionCancelReply(index)
        },
        Arc::new(cancel_subscription),
    ));

    let (result_publisher, result_subscription) = service_pair(
        ros,
        &bridge.action,
        ActionEndpoint::GetResult,
        server,
        get_result_or(&get_result_request, &wire.get_result_response, server),
        bridge.qos.result_service,
    )
    .await?;
    subscriptions.push((
        if server {
            EndpointSlot::ActionResultRequest(index)
        } else {
            EndpointSlot::ActionResultReply(index)
        },
        Arc::new(result_subscription),
    ));

    // The two topics.
    let feedback_name = FullName::topic(action_name(&bridge.action, ActionEndpoint::Feedback))?;
    let feedback_type = wire.feedback_message.dds_type_name().to_owned();
    let (feedback_publisher, feedback_output) = if server {
        (
            Some(
                RawPublisher::for_kind(
                    Arc::clone(ros.context()),
                    feedback_name,
                    TopicKind::Topic,
                    feedback_type,
                    bridge.qos.feedback,
                    Some(ros.fully_qualified_name()),
                )
                .await?,
            ),
            None,
        )
    } else {
        let subscription = RawSubscription::for_kind(
            Arc::clone(ros.context()),
            feedback_name,
            TopicKind::Topic,
            feedback_type,
            bridge.qos.feedback,
            Some(ros.fully_qualified_name()),
        )
        .await?;
        subscriptions.push((EndpointSlot::ActionFeedback(index), Arc::new(subscription)));
        let output = match &bridge.feedback_port {
            Some(port) => Some(node.raw_output(port.as_str())?),
            None => None,
        };
        (None, output)
    };

    let status_name = FullName::topic(action_name(&bridge.action, ActionEndpoint::Status))?;
    let status_type = status_binding.dds_type_name().to_owned();
    let status_publisher = if server {
        Some(
            RawPublisher::for_kind(
                Arc::clone(ros.context()),
                status_name,
                TopicKind::Topic,
                status_type,
                bridge.qos.status,
                Some(ros.fully_qualified_name()),
            )
            .await?,
        )
    } else {
        let subscription = RawSubscription::for_kind(
            Arc::clone(ros.context()),
            status_name,
            TopicKind::Topic,
            status_type,
            bridge.qos.status,
            Some(ros.fully_qualified_name()),
        )
        .await?;
        subscriptions.push((EndpointSlot::ActionStatus(index), Arc::new(subscription)));
        None
    };

    let primary_port = if server {
        bridge.goal_port.clone()
    } else {
        bridge.result_port.clone()
    };
    let primary_output = node.raw_output(primary_port.as_str())?;
    let client_guid = goal_publisher.guid();

    Ok((
        ActionEndpointSet {
            bridge,
            goal_publisher,
            cancel_publisher,
            result_publisher,
            feedback_publisher,
            status_publisher,
            primary_output,
            feedback_output,
            wire,
            names,
            goals: BTreeMap::new(),
            correlations: BTreeMap::new(),
            client_guid,
            next_sequence: 1,
        },
        subscriptions,
    ))
}

/// `(publish_type, subscribe_type)` for one service half of an action.
fn get_result_or<'a>(
    request: &'a TypeBinding,
    response: &'a TypeBinding,
    server: bool,
) -> (&'a TypeBinding, &'a TypeBinding) {
    if server {
        (response, request)
    } else {
        (request, response)
    }
}

/// Create one `rq/`+`rr/` pair for an action endpoint.
async fn service_pair(
    ros: &Ros2Node,
    action: &str,
    endpoint: ActionEndpoint,
    server: bool,
    types: (&TypeBinding, &TypeBinding),
    qos: astrs_ros2::qos::QosProfile,
) -> BridgeResult<(RawPublisher, RawSubscription)> {
    let name = FullName::service(action_name(action, endpoint))?;
    let (publish_kind, subscribe_kind) = if server {
        (TopicKind::Reply, TopicKind::Request)
    } else {
        (TopicKind::Request, TopicKind::Reply)
    };
    let publisher = RawPublisher::for_kind(
        Arc::clone(ros.context()),
        name.clone(),
        publish_kind,
        types.0.dds_type_name().to_owned(),
        qos,
        Some(ros.fully_qualified_name()),
    )
    .await?;
    let subscription = RawSubscription::for_kind(
        Arc::clone(ros.context()),
        name,
        subscribe_kind,
        types.1.dds_type_name().to_owned(),
        qos,
        Some(ros.fully_qualified_name()),
    )
    .await?;
    Ok((publisher, subscription))
}

/// The binding for one of an action's synthesized wire types.
fn binding(resolved: &ResolvedPlan, action_type: &str, suffix: &str) -> BridgeResult<TypeBinding> {
    named_binding(resolved, &action_endpoint_type_name(action_type, suffix))
}

/// The binding for a fully-spelled type name.
fn named_binding(resolved: &ResolvedPlan, ros_type_name: &str) -> BridgeResult<TypeBinding> {
    resolved.binding(ros_type_name).cloned().ok_or_else(|| {
        BridgeError::Resolve(ResolveError::UnknownType {
            kind: "action",
            type_name: ros_type_name.to_owned(),
            search: "it was not bound while the plan was resolved".to_owned(),
        })
    })
}

/// A `send_goal` request reached a `role: server` bridge.
///
/// The goal goes to the graph with `goal_id` and `goal_status = accepted`,
/// and the ROS caller is answered immediately: §9.4's FSM starts at
/// `Accepted`, and a bridge that waited for the graph to acknowledge would
/// be inventing a state the protocol does not have.
///
/// # Errors
///
/// [`BridgeError::Node`] when the goal output refuses the message, and
/// [`BridgeError::Ros2`] when the acceptance reply cannot be written.
pub async fn handle_goal_request(
    node: &Node,
    endpoint: &mut ActionEndpointSet,
    payload: &[u8],
    info: &MessageInfo,
) -> BridgeResult<bool> {
    let Ok((identity, body)) = split_identity(payload, endpoint.names.send_goal_request) else {
        tracing::warn!(action = %endpoint.bridge.action, "dropping a malformed send_goal request");
        return Ok(false);
    };
    let Some(goal_id) = goal_id_of(&body) else {
        tracing::warn!(action = %endpoint.bridge.action, "a send_goal request carried no goal id");
        return Ok(false);
    };

    let now = node.hlc_now();
    let mut meta = inbound_metadata(node, info.source_timestamp);
    meta.set_goal_id(render_goal_id(&goal_id));
    meta.set_goal_status(astrs_wire::GoalStatus::Accepted);

    let sent = emit(
        &endpoint.wire.send_goal_request,
        &mut endpoint.primary_output,
        &body,
        meta,
        &endpoint.bridge.action,
    )?;
    if !sent {
        return Ok(false);
    }

    endpoint.track(goal_id, GoalState::new(RosTime::from_hlc(now)));

    let response = send_goal_response(true, RosTime::from_hlc(now))?;
    let wire = join_identity(identity, &response, "action_msgs/msg/GoalInfo")?;
    endpoint.goal_publisher.publish_bytes(wire).await?;
    publish_status(endpoint).await?;
    Ok(true)
}

/// A `get_result` request reached a `role: server` bridge: answer it if the
/// graph has already produced a result, otherwise park it.
///
/// # Errors
///
/// [`BridgeError::Ros2`] when the reply cannot be written.
pub async fn handle_result_request(
    endpoint: &mut ActionEndpointSet,
    payload: &[u8],
) -> BridgeResult<bool> {
    let type_name = endpoint.names.get_result_response;
    let Ok((identity, body)) = split_identity(payload, type_name) else {
        return Ok(false);
    };
    let Some(goal_id) = goal_id_of(&body) else {
        return Ok(false);
    };

    let ready = endpoint
        .goals
        .get(&goal_id)
        .and_then(|state| state.result.clone());
    match ready {
        Some(result) => {
            let wire = join_identity(identity, &result, type_name)?;
            endpoint.result_publisher.publish_bytes(wire).await?;
            Ok(true)
        }
        None => {
            if let Some(state) = endpoint.goals.get_mut(&goal_id) {
                state.parked.push(identity);
            } else {
                tracing::debug!(
                    action = %endpoint.bridge.action,
                    "a get_result request named a goal this bridge does not know"
                );
            }
            Ok(false)
        }
    }
}

/// A `cancel_goal` request reached a `role: server` bridge.
///
/// Every goal this bridge is tracking that is not already terminal moves to
/// `Canceling`, the caller is told which, and the status topic is
/// republished. Whether a goal actually *stops* is the graph's business —
/// the bridge reports the request, it does not enforce it.
///
/// # Errors
///
/// [`BridgeError::Ros2`] when the reply cannot be written.
pub async fn handle_cancel_request(
    endpoint: &mut ActionEndpointSet,
    payload: &[u8],
) -> BridgeResult<bool> {
    let Ok((identity, _body)) = split_identity(payload, CANCEL_GOAL_SERVICE) else {
        return Ok(false);
    };

    let mut canceling = Vec::new();
    for (id, state) in &mut endpoint.goals {
        if state.status.is_terminal() {
            continue;
        }
        state.status = GoalStatus::Canceling;
        canceling.push(GoalInfo {
            goal_id: UUID { uuid: *id },
            stamp: Time {
                sec: state.accepted_at.sec,
                nanosec: state.accepted_at.nanosec,
            },
        });
    }

    let accepted = !canceling.is_empty();
    let response = CancelGoalResponse {
        return_code: if accepted {
            CancelGoalResponse::ERROR_NONE
        } else {
            CancelGoalResponse::ERROR_REJECTED
        },
        goals_canceling: canceling,
    };
    let body = encode_value(&response, CANCEL_GOAL_SERVICE)?;
    let wire = join_identity(identity, &body, CANCEL_GOAL_SERVICE)?;
    endpoint.cancel_publisher.publish_bytes(wire).await?;
    publish_status(endpoint).await?;
    Ok(accepted)
}

/// A `send_goal` reply reached a `role: client` bridge: if the goal was
/// accepted, ask for its result.
///
/// # Errors
///
/// [`BridgeError::Ros2`] when the `get_result` request cannot be written.
pub async fn handle_goal_reply(
    endpoint: &mut ActionEndpointSet,
    payload: &[u8],
) -> BridgeResult<bool> {
    let Ok((identity, body)) = split_identity(payload, "action_msgs/msg/GoalInfo") else {
        return Ok(false);
    };
    if !identity.was_written_by(endpoint.client_guid) {
        return Ok(false);
    }
    let Some(goal_id) = endpoint.correlations.remove(&identity) else {
        return Ok(false);
    };
    // `<Action>_SendGoal_Response` is `{ bool accepted, Time stamp }`, so
    // the acceptance flag is the first octet of the body.
    let accepted = body.get(ENCAPSULATION_LEN).copied().unwrap_or(0) != 0;
    if !accepted {
        endpoint.goals.remove(&goal_id);
        tracing::info!(action = %endpoint.bridge.action, "a goal was rejected by the server");
        return Ok(false);
    }

    let request_identity = endpoint.mint();
    endpoint.correlations.insert(request_identity, goal_id);
    let request = goal_id_request(&goal_id)?;
    let wire = join_identity(request_identity, &request, "action_msgs/msg/GoalInfo")?;
    endpoint.result_publisher.publish_bytes(wire).await?;
    Ok(true)
}

/// A `get_result` reply reached a `role: client` bridge: publish it as the
/// goal's terminal AstRS message.
///
/// # Errors
///
/// [`BridgeError::Node`] when the output refuses the message.
pub fn handle_result_reply(
    node: &Node,
    endpoint: &mut ActionEndpointSet,
    payload: &[u8],
    info: &MessageInfo,
) -> BridgeResult<bool> {
    let Ok((identity, body)) = split_identity(payload, endpoint.names.get_result_response) else {
        return Ok(false);
    };
    if !identity.was_written_by(endpoint.client_guid) {
        return Ok(false);
    }
    let Some(goal_id) = endpoint.correlations.remove(&identity) else {
        return Ok(false);
    };
    endpoint.goals.remove(&goal_id);

    let status = status_of(&body).unwrap_or(GoalStatus::Unknown);
    let mut meta = inbound_metadata(node, info.source_timestamp);
    meta.set_goal_id(render_goal_id(&goal_id));
    meta.set_goal_status(wire_status(status));

    emit(
        &endpoint.wire.get_result_response,
        &mut endpoint.primary_output,
        &body,
        meta,
        &endpoint.bridge.action,
    )
}

/// A feedback sample reached a `role: client` bridge.
///
/// # Errors
///
/// [`BridgeError::Node`] when the feedback output refuses the message.
pub fn handle_feedback(
    node: &Node,
    endpoint: &mut ActionEndpointSet,
    payload: &[u8],
    info: &MessageInfo,
) -> BridgeResult<bool> {
    let Some(output) = endpoint.feedback_output.as_mut() else {
        return Ok(false);
    };
    let Some(goal_id) = goal_id_of(payload) else {
        return Ok(false);
    };
    let mut meta = inbound_metadata(node, info.source_timestamp);
    meta.set_goal_id(render_goal_id(&goal_id));
    emit(
        &endpoint.wire.feedback_message,
        output,
        payload,
        meta,
        &endpoint.bridge.action,
    )
}

/// An AstRS message reached this action's primary input.
///
/// For a server that is a goal's *result*; for a client it is a new *goal*.
///
/// # Errors
///
/// [`BridgeError::Ros2`] when a DDS write fails.
pub async fn forward_primary(
    endpoint: &mut ActionEndpointSet,
    meta: &Metadata,
    payload: &Payload,
) -> BridgeResult<bool> {
    match endpoint.bridge.role {
        Ros2Role::Server => forward_result(endpoint, meta, payload).await,
        Ros2Role::Client => forward_goal(endpoint, meta, payload).await,
    }
}

/// A `role: server` bridge's graph produced a goal's result.
async fn forward_result(
    endpoint: &mut ActionEndpointSet,
    meta: &Metadata,
    payload: &Payload,
) -> BridgeResult<bool> {
    let Some(goal_id) = meta.goal_id().and_then(parse_goal_id) else {
        tracing::warn!(
            action = %endpoint.bridge.action,
            "dropping an action result with no usable `goal_id` metadata"
        );
        return Ok(false);
    };
    let Some(body) = encode_body(
        &endpoint.wire.get_result_response,
        payload,
        &endpoint.bridge.action,
    ) else {
        return Ok(false);
    };

    let status = meta.goal_status().map_or(GoalStatus::Succeeded, |status| {
        i8::try_from(status.as_i64()).map_or(GoalStatus::Unknown, GoalStatus::from_code)
    });

    let parked = {
        let Some(state) = endpoint.goals.get_mut(&goal_id) else {
            tracing::warn!(
                action = %endpoint.bridge.action,
                "an action result named a goal this bridge does not know"
            );
            return Ok(false);
        };
        state.status = status;
        state.result = Some(body.clone());
        std::mem::take(&mut state.parked)
    };

    let type_name = endpoint.names.get_result_response;
    for identity in parked {
        let wire = join_identity(identity, &body, type_name)?;
        endpoint.result_publisher.publish_bytes(wire).await?;
    }
    publish_status(endpoint).await?;
    Ok(true)
}

/// A `role: client` bridge's graph issued a goal.
async fn forward_goal(
    endpoint: &mut ActionEndpointSet,
    meta: &Metadata,
    payload: &Payload,
) -> BridgeResult<bool> {
    let Some(body) = encode_body(
        &endpoint.wire.send_goal_request,
        payload,
        &endpoint.bridge.action,
    ) else {
        return Ok(false);
    };
    // The goal id is the first field of the encoded request; the metadata
    // key is an override for a graph that would rather choose it.
    let Some(goal_id) = meta
        .goal_id()
        .and_then(parse_goal_id)
        .or_else(|| goal_id_of(&body))
    else {
        tracing::warn!(action = %endpoint.bridge.action, "an action goal carried no goal id");
        return Ok(false);
    };

    let identity = endpoint.mint();
    endpoint.correlations.insert(identity, goal_id);
    let now = RosTime::from_hlc(meta.timestamp);
    endpoint.track(goal_id, GoalState::new(now));

    let wire = join_identity(identity, &body, endpoint.names.send_goal_request)?;
    endpoint.goal_publisher.publish_bytes(wire).await?;
    Ok(true)
}

/// An AstRS message reached a `role: server` bridge's feedback input.
///
/// # Errors
///
/// [`BridgeError::Ros2`] when the DDS write fails.
pub async fn forward_feedback(
    endpoint: &ActionEndpointSet,
    payload: &Payload,
) -> BridgeResult<bool> {
    let Some(publisher) = endpoint.feedback_publisher.as_ref() else {
        return Ok(false);
    };
    let Some(body) = encode_body(
        &endpoint.wire.feedback_message,
        payload,
        &endpoint.bridge.action,
    ) else {
        return Ok(false);
    };
    publisher.publish_bytes(body).await?;
    Ok(true)
}

/// Republish the status topic, when this endpoint owns one.
async fn publish_status(endpoint: &ActionEndpointSet) -> BridgeResult<()> {
    let Some(publisher) = endpoint.status_publisher.as_ref() else {
        return Ok(());
    };
    let body = encode_value(&endpoint.status_array(), GOAL_STATUS_ARRAY)?;
    publisher.publish_bytes(body).await?;
    Ok(())
}

/// The CDR body of a `<Action>_SendGoal_Response`.
///
/// Every action's is `{ bool accepted, builtin_interfaces/Time stamp }`,
/// so one encoder serves all of them: one octet, three of alignment
/// padding, then the two `Time` members.
fn send_goal_response(accepted: bool, stamp: RosTime) -> BridgeResult<Vec<u8>> {
    let mut writer = CdrWriter::new(Encoding::ROS2);
    writer
        .write_bool(accepted)
        .and_then(|()| writer.write_i32(stamp.sec))
        .and_then(|()| writer.write_u32(stamp.nanosec))
        .map_err(|source| {
            BridgeError::Codec(crate::error::CodecError::Cdr {
                type_name: "action SendGoal_Response",
                len: 0,
                source,
            })
        })?;
    Ok(writer.finish())
}

/// The CDR body of a `<Action>_GetResult_Request`: just the goal id.
fn goal_id_request(goal_id: &[u8; GOAL_ID_LEN]) -> BridgeResult<Vec<u8>> {
    let mut writer = CdrWriter::new(Encoding::ROS2);
    writer.write_octets(goal_id);
    Ok(writer.finish())
}

/// The `astrs-wire` spelling of an `astrs-ros2` goal status.
const fn wire_status(status: GoalStatus) -> astrs_wire::GoalStatus {
    match status {
        GoalStatus::Accepted => astrs_wire::GoalStatus::Accepted,
        GoalStatus::Executing => astrs_wire::GoalStatus::Executing,
        GoalStatus::Canceling => astrs_wire::GoalStatus::Canceling,
        GoalStatus::Succeeded => astrs_wire::GoalStatus::Succeeded,
        GoalStatus::Canceled => astrs_wire::GoalStatus::Canceled,
        GoalStatus::Aborted => astrs_wire::GoalStatus::Aborted,
        GoalStatus::Unknown => astrs_wire::GoalStatus::Unknown,
    }
}

/// Decode one action body and put it on an AstRS output.
fn emit(
    binding: &TypeBinding,
    output: &mut RawOutput,
    body: &[u8],
    meta: Metadata,
    action: &str,
) -> BridgeResult<bool> {
    match binding {
        TypeBinding::Generated(codec) => match codec.decode(body) {
            Ok(decoded) => {
                output.send_batch(&decoded.batch, meta)?;
                Ok(true)
            }
            Err(error) => {
                tracing::warn!(action, %error, "dropping an undecodable action message");
                Ok(false)
            }
        },
        TypeBinding::Discovered(found) => {
            let mut meta = meta;
            crate::topic::annotate_opaque(&mut meta, action, &found.ros_type_name);
            output.send_bytes(body, meta)?;
            Ok(true)
        }
    }
}

/// Encode one AstRS payload as an action body.
fn encode_body(binding: &TypeBinding, payload: &Payload, action: &str) -> Option<Vec<u8>> {
    match binding {
        TypeBinding::Generated(codec) => {
            let batch = payload
                .batch()
                .inspect_err(|error| {
                    tracing::warn!(action, %error, "an action message is not a columnar batch");
                })
                .ok()?;
            codec
                .encode(batch)
                .inspect_err(|error| {
                    tracing::warn!(action, %error, "an action message will not encode as CDR");
                })
                .ok()
        }
        TypeBinding::Discovered(_) => Some(payload.to_vec()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn a_goal_id_round_trips_through_its_rendered_form() {
        let id = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let rendered = render_goal_id(&id);
        assert_eq!(rendered, "00112233445566778899aabbccddeeff");
        assert_eq!(parse_goal_id(&rendered), Some(id));
        assert_eq!(parse_goal_id(&rendered.to_uppercase()), Some(id));
    }

    #[test]
    fn a_malformed_goal_id_does_not_parse() {
        assert_eq!(parse_goal_id(""), None);
        assert_eq!(parse_goal_id("00"), None);
        assert_eq!(parse_goal_id(&"z".repeat(32)), None);
    }

    #[test]
    fn the_goal_id_is_read_from_the_start_of_a_body() {
        let mut body = vec![0x00, 0x01, 0x00, 0x00];
        body.extend_from_slice(&[7; GOAL_ID_LEN]);
        body.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(goal_id_of(&body), Some([7; GOAL_ID_LEN]));
    }

    #[test]
    fn a_body_too_short_for_a_goal_id_reports_none() {
        assert_eq!(goal_id_of(&[0x00, 0x01, 0x00, 0x00]), None);
        assert_eq!(goal_id_of(&[]), None);
    }

    #[test]
    fn the_status_byte_is_read_from_the_start_of_a_result_body() {
        let body = vec![0x00, 0x01, 0x00, 0x00, 4, 0, 0, 0];
        assert_eq!(status_of(&body), Some(GoalStatus::Succeeded));
        assert_eq!(status_of(&[0x00, 0x01, 0x00, 0x00]), None);
    }

    #[test]
    fn the_send_goal_response_encoder_matches_the_generated_layout() {
        use astrs_ros2::msg::example_interfaces::FibonacciSendGoalResponse;

        let encoded = send_goal_response(true, RosTime::new(7, 8)).unwrap();
        let decoded: FibonacciSendGoalResponse = astrs_cdr::from_bytes_tolerant(&encoded).unwrap();
        assert!(decoded.accepted);
        assert_eq!(decoded.stamp.sec, 7);
        assert_eq!(decoded.stamp.nanosec, 8);
    }

    #[test]
    fn the_get_result_request_encoder_matches_the_generated_layout() {
        use astrs_ros2::msg::example_interfaces::FibonacciGetResultRequest;

        let id = [3; GOAL_ID_LEN];
        let encoded = goal_id_request(&id).unwrap();
        let decoded: FibonacciGetResultRequest = astrs_cdr::from_bytes_tolerant(&encoded).unwrap();
        assert_eq!(decoded.goal_id.uuid, id);
        assert_eq!(goal_id_of(&encoded), Some(id));
    }

    #[test]
    fn every_ros2_goal_status_has_a_wire_spelling() {
        for status in [
            GoalStatus::Unknown,
            GoalStatus::Accepted,
            GoalStatus::Executing,
            GoalStatus::Canceling,
            GoalStatus::Succeeded,
            GoalStatus::Canceled,
            GoalStatus::Aborted,
        ] {
            assert_eq!(
                i64::from(status.code()),
                wire_status(status).as_i64(),
                "{status:?} must map onto the same discriminant"
            );
        }
    }

    #[test]
    fn the_goal_table_is_bounded() {
        assert_eq!(MAX_GOALS, 64);
    }
}
