//! Service endpoints: `rq/`/`rr/` in, correlated metadata out.
//!
//! §9.4 is explicit that AstRS has no separate RPC subsystem — "services
//! (`request_id`) … ride ordinary edges plus metadata correlation". A ROS 2
//! service is the same idea with a different correlation token: two shared
//! topics (`rq/<name>Request`, `rr/<name>Reply`) and a twenty-four-octet
//! [`SampleIdentity`] the server echoes verbatim. Bridging one is therefore
//! a *translation between two correlation schemes*, and this module is that
//! translation and nothing else.
//!
//! ```text
//!   role: server                        role: client
//!   ─────────────                       ─────────────
//!   rq/…Request ─▶ request_port         request_port ─▶ rq/…Request
//!         (remember identity)                 (mint identity)
//!   response_port ─▶ rr/…Reply          rr/…Reply ─▶ response_port
//!         (echo identity)                     (match identity, drop others)
//! ```
//!
//! # Why the raw endpoints rather than `ServiceServer<S>`
//!
//! `astrs-ros2`'s typed [`ServiceServer`](astrs_ros2::service::ServiceServer)
//! is generic over a `ServiceType` marker, and a marker exists only for the
//! handful of services that crate declares. A bridge is handed
//! `"example_interfaces/srv/AddTwoInts"` as a *string*, so it uses the
//! type-erased half — [`RawPublisher`]/[`RawSubscription`] with
//! [`TopicKind::Request`]/[`TopicKind::Reply`], which is exactly the seam
//! `for_kind` exists for — and reaches the request and response *messages*
//! through [`crate::codec::registry`] like any other type. One mechanism,
//! and every service whose two halves are generated works, not just the
//! declared five.
//!
//! # Dropping a reply that is not ours
//!
//! Every client of a service subscribes to the same `rr/` topic, so a client
//! bridge sees replies to other clients' requests. [`SampleIdentity::
//! was_written_by`] is the filter, applied before the body is decoded —
//! another client's reply must not be able to fail this bridge by carrying a
//! body it cannot parse.

use std::collections::BTreeMap;
use std::sync::Arc;

use astrs_manifest::Ros2Role;
use astrs_node_api::{Node, Payload, RawOutput};
use astrs_ros2::names::{FullName, TopicKind};
use astrs_ros2::node::Ros2Node;
use astrs_ros2::pubsub::{MessageInfo, RawPublisher, RawSubscription};
use astrs_ros2::service::SampleIdentity;
use astrs_rtps::structure::Guid;
use astrs_wire::Metadata;

use crate::codec::{join_identity, split_identity};
use crate::error::{BridgeError, BridgeResult};
use crate::plan::ServiceBridge;
use crate::resolve::{ResolvedPlan, TypeBinding};
use crate::topic::inbound_metadata;

/// One bridged service, both halves.
#[derive(Debug)]
pub struct ServiceEndpoint {
    /// What the plan said.
    pub bridge: ServiceBridge,
    /// How the request message is carried.
    pub request: TypeBinding,
    /// How the response message is carried.
    pub response: TypeBinding,
    /// The request type's name, resolved to `'static` once at startup.
    ///
    /// [`crate::CodecError`] carries `&'static str`, because for a
    /// generated type the name *is* static. A discovered type's name is
    /// owned, so it is promoted here — once per endpoint, at open time —
    /// rather than on every message, which would leak without bound on a
    /// busy service (§3: no needless allocation on hot paths).
    pub request_type: &'static str,
    /// The response type's name, likewise.
    pub response_type: &'static str,
    /// The DDS publication: `rq/` for a client, `rr/` for a server.
    pub publisher: RawPublisher,
    /// The AstRS output: the request port for a server, the response port
    /// for a client.
    pub output: RawOutput,
    /// The correlation table, keyed by the `request_id` metadata value.
    ///
    /// A server remembers the identity it must echo; a client remembers the
    /// identity it minted, so a reply can be matched back to the AstRS
    /// message that caused it. One table serves both because both map the
    /// same way round: `request_id` → the twenty-four octets.
    pending: BTreeMap<String, SampleIdentity>,
    /// The GUID a client's requests are written under, for filtering
    /// replies. Unused by a server.
    client_guid: Guid,
    /// The next client-local request sequence number.
    next_sequence: i64,
}

/// How many in-flight requests one endpoint remembers before the oldest is
/// forgotten.
///
/// A correlation table is unbounded state driven by a *remote* peer, which
/// is the shape §12 calls a fault-containment hazard: a client that issues
/// requests and never reads replies would grow it without limit. Sixty-four
/// is generous for a service (rcl's own default reply history is ten) and
/// bounded is what matters.
pub const MAX_PENDING: usize = 64;

impl ServiceEndpoint {
    /// The AstRS port this endpoint's *outbound* direction reads from.
    ///
    /// A server answers on the response port; a client asks on the request
    /// port.
    #[must_use]
    pub const fn inbound_port(&self) -> &astrs_wire::DataId {
        match self.bridge.role {
            Ros2Role::Server => &self.bridge.response_port,
            Ros2Role::Client => &self.bridge.request_port,
        }
    }

    /// How many requests are currently correlated.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Remember one correlation, evicting the oldest if the table is full.
    fn remember(&mut self, request_id: String, identity: SampleIdentity) {
        if self.pending.len() >= MAX_PENDING
            && let Some(oldest) = self.pending.keys().next().cloned()
        {
            self.pending.remove(&oldest);
            tracing::warn!(
                service = %self.bridge.service,
                "the service correlation table is full; forgetting the oldest request"
            );
        }
        self.pending.insert(request_id, identity);
    }

    /// The `request_id` a minted identity is remembered under.
    fn mint(&mut self) -> SampleIdentity {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        SampleIdentity::from_guid(self.client_guid, sequence)
    }
}

/// Create both DDS halves of one bridged service, plus its AstRS output.
///
/// # Errors
///
/// [`BridgeError::Ros2`] when an endpoint cannot be created or the service
/// name is not a legal ROS 2 name, and [`BridgeError::Node`] when the AstRS
/// port does not exist.
pub async fn open(
    node: &mut Node,
    ros: &Ros2Node,
    resolved: &ResolvedPlan,
    bridge: ServiceBridge,
) -> BridgeResult<(ServiceEndpoint, Arc<RawSubscription>)> {
    let request = binding_for(
        resolved,
        &crate::codec::registry::request_type_name(&bridge.service_type),
    )?;
    let response = binding_for(
        resolved,
        &crate::codec::registry::response_type_name(&bridge.service_type),
    )?;

    let request_type = static_name(request.ros_type_name());
    let response_type = static_name(response.ros_type_name());

    let name = ros.resolve_service(&bridge.service)?;
    let (publish_kind, publish_type, subscribe_kind, subscribe_type, port) = match bridge.role {
        Ros2Role::Server => (
            TopicKind::Reply,
            response.dds_type_name().to_owned(),
            TopicKind::Request,
            request.dds_type_name().to_owned(),
            bridge.request_port.clone(),
        ),
        Ros2Role::Client => (
            TopicKind::Request,
            request.dds_type_name().to_owned(),
            TopicKind::Reply,
            response.dds_type_name().to_owned(),
            bridge.response_port.clone(),
        ),
    };

    let publisher =
        raw_publisher(ros, name.clone(), publish_kind, publish_type, bridge.qos).await?;
    let subscription =
        raw_subscription(ros, name, subscribe_kind, subscribe_type, bridge.qos).await?;
    let client_guid = publisher.guid();
    let output = node.raw_output(port.as_str())?;

    Ok((
        ServiceEndpoint {
            bridge,
            request,
            response,
            request_type,
            response_type,
            publisher,
            output,
            pending: BTreeMap::new(),
            client_guid,
            next_sequence: 1,
        },
        Arc::new(subscription),
    ))
}

/// The binding a resolved plan holds for `ros_type_name`.
fn binding_for(resolved: &ResolvedPlan, ros_type_name: &str) -> BridgeResult<TypeBinding> {
    resolved.binding(ros_type_name).cloned().ok_or_else(|| {
        BridgeError::Resolve(crate::error::ResolveError::UnknownType {
            kind: "service",
            type_name: ros_type_name.to_owned(),
            search: "it was not bound while the plan was resolved".to_owned(),
        })
    })
}

/// A `rq/`/`rr/` publication under this bridge's graph identity.
async fn raw_publisher(
    ros: &Ros2Node,
    name: FullName,
    kind: TopicKind,
    dds_type: String,
    qos: astrs_ros2::qos::QosProfile,
) -> BridgeResult<RawPublisher> {
    Ok(RawPublisher::for_kind(
        Arc::clone(ros.context()),
        name,
        kind,
        dds_type,
        qos,
        Some(ros.fully_qualified_name()),
    )
    .await?)
}

/// A `rq/`/`rr/` subscription under this bridge's graph identity.
async fn raw_subscription(
    ros: &Ros2Node,
    name: FullName,
    kind: TopicKind,
    dds_type: String,
    qos: astrs_ros2::qos::QosProfile,
) -> BridgeResult<RawSubscription> {
    Ok(RawSubscription::for_kind(
        Arc::clone(ros.context()),
        name,
        kind,
        dds_type,
        qos,
        Some(ros.fully_qualified_name()),
    )
    .await?)
}

/// A DDS request reached a `role: server` bridge: publish it as an AstRS
/// message correlated by `request_id`.
///
/// # Errors
///
/// [`BridgeError::Node`] when the output refuses the message. A request
/// that will not split or decode is logged and dropped (`Ok(false)`).
pub fn forward_request(
    node: &Node,
    endpoint: &mut ServiceEndpoint,
    payload: &[u8],
    info: &MessageInfo,
) -> BridgeResult<bool> {
    let Ok((identity, body)) = split_identity(payload, endpoint.request_type) else {
        tracing::warn!(service = %endpoint.bridge.service, "dropping a malformed service request");
        return Ok(false);
    };

    let request_id = identity.to_string();
    let mut meta = inbound_metadata(node, info.source_timestamp);
    meta.set_request_id(request_id.clone());

    if !emit(
        &endpoint.request,
        &mut endpoint.output,
        &body,
        meta,
        &endpoint.bridge.service,
    )? {
        return Ok(false);
    }
    endpoint.remember(request_id, identity);
    Ok(true)
}

/// A DDS reply reached a `role: client` bridge: match it and publish it as
/// an AstRS message.
///
/// # Errors
///
/// As [`forward_request`]. A reply for another client is dropped silently
/// and reported as `Ok(false)` — it is not this bridge's business, not a
/// fault.
pub fn forward_reply(
    node: &Node,
    endpoint: &mut ServiceEndpoint,
    payload: &[u8],
    info: &MessageInfo,
) -> BridgeResult<bool> {
    let Ok((identity, body)) = split_identity(payload, endpoint.response_type) else {
        tracing::warn!(service = %endpoint.bridge.service, "dropping a malformed service reply");
        return Ok(false);
    };
    if !identity.was_written_by(endpoint.client_guid) {
        return Ok(false);
    }

    let request_id = endpoint
        .pending
        .iter()
        .find(|(_, pending)| **pending == identity)
        .map(|(id, _)| id.clone());
    let Some(request_id) = request_id else {
        tracing::debug!(
            service = %endpoint.bridge.service,
            %identity,
            "a reply arrived for a request this bridge no longer remembers"
        );
        return Ok(false);
    };
    endpoint.pending.remove(&request_id);

    let mut meta = inbound_metadata(node, info.source_timestamp);
    meta.set_request_id(request_id);
    emit(
        &endpoint.response,
        &mut endpoint.output,
        &body,
        meta,
        &endpoint.bridge.service,
    )
}

/// An AstRS message reached the bridge on this service's inbound port.
///
/// For a server that is the *response* to a request it forwarded; for a
/// client it is a new *request*. Both end up as one identity-prefixed CDR
/// sample on the corresponding DDS topic.
///
/// # Errors
///
/// [`BridgeError::Ros2`] when the DDS write fails. A message whose
/// `request_id` is unknown to a server is logged and dropped (`Ok(false)`):
/// a response with nobody to send it to is a graph bug, not a bridge fault.
pub async fn forward_outbound(
    endpoint: &mut ServiceEndpoint,
    meta: &Metadata,
    payload: &Payload,
) -> BridgeResult<bool> {
    let (binding, type_name, identity) = match endpoint.bridge.role {
        Ros2Role::Server => {
            let Some(request_id) = meta.request_id() else {
                tracing::warn!(
                    service = %endpoint.bridge.service,
                    "dropping a service response with no `request_id` metadata"
                );
                return Ok(false);
            };
            let Some(identity) = endpoint.pending.remove(request_id) else {
                tracing::warn!(
                    service = %endpoint.bridge.service,
                    request_id,
                    "dropping a service response whose request is no longer in flight"
                );
                return Ok(false);
            };
            (&endpoint.response, endpoint.response_type, identity)
        }
        Ros2Role::Client => {
            let identity = endpoint.mint();
            let request_id = meta
                .request_id()
                .map_or_else(|| identity.to_string(), ToOwned::to_owned);
            endpoint.remember(request_id, identity);
            (&endpoint.request, endpoint.request_type, identity)
        }
    };

    let Some(body) = encode_body(binding, payload, &endpoint.bridge.service) else {
        return Ok(false);
    };
    let Ok(wire) = join_identity(identity, &body, type_name) else {
        tracing::warn!(
            service = %endpoint.bridge.service,
            "dropping a service message whose body would not take a correlation header"
        );
        return Ok(false);
    };

    endpoint.publisher.publish_bytes(wire).await?;
    Ok(true)
}

/// Decode one service body and put it on an AstRS output.
fn emit(
    binding: &TypeBinding,
    output: &mut RawOutput,
    body: &[u8],
    meta: Metadata,
    service: &str,
) -> BridgeResult<bool> {
    match binding {
        TypeBinding::Generated(codec) => match codec.decode(body) {
            Ok(decoded) => {
                output.send_batch(&decoded.batch, meta)?;
                Ok(true)
            }
            Err(error) => {
                tracing::warn!(service, %error, "dropping an undecodable service message");
                Ok(false)
            }
        },
        TypeBinding::Discovered(found) => {
            let mut meta = meta;
            crate::topic::annotate_opaque(&mut meta, service, &found.ros_type_name);
            output.send_bytes(body, meta)?;
            Ok(true)
        }
    }
}

/// Encode one AstRS payload as a service body, or say why it could not be.
fn encode_body(binding: &TypeBinding, payload: &Payload, service: &str) -> Option<Vec<u8>> {
    match binding {
        TypeBinding::Generated(codec) => {
            let batch = payload
                .batch()
                .inspect_err(|error| {
                    tracing::warn!(service, %error, "a service message is not a columnar batch");
                })
                .ok()?;
            codec
                .encode(batch)
                .inspect_err(|error| {
                    tracing::warn!(service, %error, "a service message will not encode as CDR");
                })
                .ok()
        }
        TypeBinding::Discovered(_) => Some(payload.to_vec()),
    }
}

/// A `&str` as the `&'static str` this crate's error type wants.
///
/// **Call this once per endpoint, at open time, never per message.** The
/// codec errors carry `&'static str` because for a generated type the name
/// *is* static; a type discovered on an ament tree has an owned name, and
/// the only way to keep one error type for both halves is to promote it.
/// A registry hit costs nothing (the static name already exists); a
/// discovered name leaks one string, which is bounded by the plan — one per
/// endpoint — rather than by traffic.
pub fn static_name(name: &str) -> &'static str {
    // A generated type's name is already static; look it up rather than
    // leaking a second copy of the same string.
    crate::codec::registry::lookup(name).map_or_else(
        || Box::leak(name.to_owned().into_boxed_str()) as &'static str,
        |codec| codec.ros_type_name,
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn a_generated_type_name_is_not_leaked_twice() {
        let first = static_name("std_srvs/srv/SetBool_Request");
        let second = static_name("std_srvs/srv/SetBool_Request");
        assert!(
            std::ptr::eq(first, second),
            "a registered name resolves to the one static string"
        );
    }

    #[test]
    fn an_unregistered_name_still_becomes_static() {
        assert_eq!(
            static_name("my_msgs/srv/Custom_Request"),
            "my_msgs/srv/Custom_Request"
        );
    }

    #[test]
    fn an_identity_renders_as_a_stable_request_id() {
        let identity = SampleIdentity::new([3; 16], 17);
        let rendered = identity.to_string();
        assert!(rendered.ends_with("#17"), "{rendered}");
        assert_eq!(SampleIdentity::new([3; 16], 17).to_string(), rendered);
        assert_ne!(SampleIdentity::new([3; 16], 18).to_string(), rendered);
    }

    #[test]
    fn the_pending_table_is_bounded() {
        assert_eq!(MAX_PENDING, 64);
    }
}
