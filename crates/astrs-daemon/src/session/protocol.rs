//! The node conversation, as a pure state machine.
//!
//! One node's half of the daemon↔node leg (§7.3, §24.1), expressed as a
//! function from *(current state, [`astrs_wire::NodeRequest`])* to a list of
//! [`SessionAction`]s. No socket, no tokio, no `DaemonState`: the transport is
//! [`super::actor`]'s problem and the bookkeeping is [`crate::server`]'s, and
//! keeping them out of here is what makes the conversation testable against a
//! scripted fake node with no I/O at all.
//!
//! ```text
//!   node                                        daemon
//!    ├── Register(NodeHandshake) ───────────────►│ Accepted → Registered{spec}
//!    │◄── Registered { spec, session } ──────────┤
//!    ├── Subscribe { inputs } ──────────────────►│ Subscribed
//!    ├── SendMessage { output, payload } ───────►│ Publish → the router
//!    ├── NextEvent { timeout, max_batch } ──────►│ Deliver ≤ max_batch
//!    ├── OutputDone / CloseOutputs ─────────────►│ CloseOutput(s)
//!    ├── ExtStore / ExtLoad / ExtDrop ──────────►│ the extension table
//!    ├── RouteUpgradeAck ───────────────────────►│ (stage 2)
//!    └── EventStreamDropped ────────────────────►│ Disconnect
//! ```
//!
//! # Registration is the gate
//!
//! Everything except `Register` requires a registered session
//! ([`SessionAction::Refuse`] with
//! [`crate::DaemonError::UnregisteredSession`] otherwise). A node that
//! publishes before it registers is not a node the daemon can route for: it
//! has no identity, so its output has no producer port.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::session::{SessionAction, SessionProtocol};
//! use astrs_wire::{DataflowId, NodeHandshake, NodeId, NodeRequest, SessionId};
//!
//! let mut protocol = SessionProtocol::new(SessionId::from_u128(1));
//! assert!(!protocol.is_registered());
//!
//! // Anything before Register is refused.
//! let refused = protocol.handle(NodeRequest::Subscribe { inputs: Vec::new() });
//! assert!(matches!(refused.as_slice(), [SessionAction::Refuse { .. }]));
//!
//! let handshake = NodeHandshake::new(DataflowId::from_u128(7), NodeId::new("camera")?, 0);
//! let actions = protocol.handle(NodeRequest::Register(handshake));
//! assert!(matches!(actions.as_slice(), [SessionAction::Register { .. }]));
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use std::time::Duration;

use astrs_wire::{
    DataId, DataflowId, DurationMs, ExtensionKey, Metadata, NodeHandshake, NodeId, NodeRequest,
    OutputPayload, SessionId,
};

use crate::error::DaemonError;

/// The largest event batch the daemon will answer a `NextEvent` with.
///
/// A node may ask for more; it gets this. The cap exists because the batch is
/// built in one pass while the event loop holds its state, and an unbounded
/// `max_batch` would let one node's request stall every other node's.
pub const MAX_EVENT_BATCH: u32 = 1_024;

/// What the daemon should do about one request.
///
/// Deliberately *descriptive*: the protocol says what should happen, and the
/// caller — which owns the state, the router and the socket — makes it happen.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum SessionAction {
    /// Bind this session to a node and answer with
    /// [`astrs_wire::NodeEvent::Registered`].
    Register {
        /// The handshake the node presented.
        handshake: Box<NodeHandshake>,
    },
    /// Record the node's input subscriptions.
    Subscribe {
        /// The inputs it named; empty means "everything my spec declares".
        inputs: Vec<DataId>,
    },
    /// Fan a published message out to its consumers.
    Publish {
        /// The output it was published on.
        output: DataId,
        /// The metadata riding with it.
        metadata: Box<Metadata>,
        /// The payload, inline or in shared memory.
        payload: OutputPayload,
    },
    /// One output will produce nothing further.
    CloseOutput {
        /// The output that finished.
        output: DataId,
    },
    /// Several outputs will produce nothing further; empty means all of them.
    CloseOutputs {
        /// The outputs that finished.
        outputs: Vec<DataId>,
    },
    /// Deliver up to `max_batch` queued events, waiting at most `timeout`.
    Deliver {
        /// How long to wait for the first event.
        timeout: Option<Duration>,
        /// How many events to deliver at once.
        max_batch: u32,
    },
    /// Store a value in the extension table.
    ExtStore {
        /// The key.
        key: ExtensionKey,
        /// The bytes.
        value: Vec<u8>,
        /// Its time-to-live, if any.
        ttl: Option<Duration>,
    },
    /// Read a value from the extension table and answer with
    /// [`astrs_wire::NodeEvent::ExtValue`].
    ExtLoad {
        /// The key.
        key: ExtensionKey,
    },
    /// Drop a value from the extension table.
    ExtDrop {
        /// The key.
        key: ExtensionKey,
    },
    /// The node accepted (or refused) a shared-memory route upgrade (§6.3).
    RouteUpgradeAck {
        /// The output being upgraded.
        output: DataId,
        /// Whether the node took it.
        accepted: bool,
        /// Why not, when it refused.
        reason: Option<String>,
    },
    /// The node will read no further events; tear the session down.
    Disconnect,
    /// The request cannot be honoured; report it and keep the session.
    Refuse {
        /// Why.
        reason: RefusalReason,
    },
    /// This node's own [`astrs_scheduler::DeadlineMonitor`] measured a
    /// per-input latency budget (§11.3) exceeded — relay it onto
    /// `astrs/status` as [`astrs_wire::NodeEvent::DeadlineViolated`] and
    /// count it.
    ReportDeadlineViolation {
        /// The input whose budget was exceeded.
        input: DataId,
        /// The budget it was held to.
        budget: DurationMs,
        /// The measured latency.
        latency: DurationMs,
    },
}

impl SessionAction {
    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Register { .. } => "register",
            Self::Subscribe { .. } => "subscribe",
            Self::Publish { .. } => "publish",
            Self::CloseOutput { .. } => "close_output",
            Self::CloseOutputs { .. } => "close_outputs",
            Self::Deliver { .. } => "deliver",
            Self::ExtStore { .. } => "ext_store",
            Self::ExtLoad { .. } => "ext_load",
            Self::ExtDrop { .. } => "ext_drop",
            Self::RouteUpgradeAck { .. } => "route_upgrade_ack",
            Self::Disconnect => "disconnect",
            Self::Refuse { .. } => "refuse",
            Self::ReportDeadlineViolation { .. } => "report_deadline_violation",
        }
    }

    /// Whether this action ends the session.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Disconnect)
    }
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefusalReason {
    /// The node spoke before registering.
    NotRegistered,
    /// The node registered twice on one connection.
    AlreadyRegistered {
        /// What it registered as the first time.
        node: NodeId,
    },
    /// The node tried to write a namespace reserved for the daemon (§2.1).
    ReservedNamespace {
        /// The namespace it named.
        namespace: astrs_wire::ExtensionNamespace,
    },
}

impl RefusalReason {
    /// The [`DaemonError`] this refusal corresponds to.
    #[must_use]
    pub fn to_error(&self, session: SessionId, dataflow: DataflowId) -> DaemonError {
        match self {
            Self::NotRegistered => DaemonError::UnregisteredSession { session },
            Self::AlreadyRegistered { node } => DaemonError::DuplicateRegistration {
                dataflow,
                node: node.clone(),
                generation: 0,
            },
            Self::ReservedNamespace { namespace } => DaemonError::ReservedNamespace {
                namespace: *namespace,
            },
        }
    }

    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::NotRegistered => "not_registered",
            Self::AlreadyRegistered { .. } => "already_registered",
            Self::ReservedNamespace { .. } => "reserved_namespace",
        }
    }
}

impl core::fmt::Display for RefusalReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotRegistered => f.write_str("the session has not registered"),
            Self::AlreadyRegistered { node } => {
                write!(f, "the session is already registered as {node}")
            }
            Self::ReservedNamespace { namespace } => {
                write!(
                    f,
                    "extension namespace {namespace} is reserved for the daemon"
                )
            }
        }
    }
}

/// One session's conversation state.
#[derive(Debug, Clone)]
pub struct SessionProtocol {
    /// The session this conversation belongs to.
    session: SessionId,
    /// Who registered on it, once somebody has.
    identity: Option<(DataflowId, NodeId, u64)>,
    /// How many requests have been handled, for diagnostics.
    handled: u64,
    /// Whether the node has said it will read no more events.
    disconnected: bool,
}

impl SessionProtocol {
    /// A fresh, unregistered conversation.
    #[must_use]
    pub const fn new(session: SessionId) -> Self {
        Self {
            session,
            identity: None,
            handled: 0,
            disconnected: false,
        }
    }

    /// The session id.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Whether a node has registered on this session.
    #[must_use]
    pub const fn is_registered(&self) -> bool {
        self.identity.is_some()
    }

    /// The node that registered, if one has.
    #[must_use]
    pub fn node(&self) -> Option<&NodeId> {
        self.identity.as_ref().map(|(_, node, _)| node)
    }

    /// The dataflow the registered node belongs to.
    #[must_use]
    pub fn dataflow(&self) -> Option<DataflowId> {
        self.identity.as_ref().map(|(dataflow, _, _)| *dataflow)
    }

    /// The generation the registered node presented.
    #[must_use]
    pub fn generation(&self) -> Option<u64> {
        self.identity.as_ref().map(|(_, _, generation)| *generation)
    }

    /// How many requests this conversation has handled.
    #[must_use]
    pub const fn handled(&self) -> u64 {
        self.handled
    }

    /// Whether the node has disconnected its event stream.
    #[must_use]
    pub const fn is_disconnected(&self) -> bool {
        self.disconnected
    }

    /// Records that the daemon accepted a registration.
    ///
    /// Called by the caller *after* it has validated the handshake against its
    /// own state — the protocol proposes, the daemon disposes.
    pub fn confirm_registration(&mut self, dataflow: DataflowId, node: NodeId, generation: u64) {
        self.identity = Some((dataflow, node, generation));
    }

    /// Turns one request into the actions it implies.
    pub fn handle(&mut self, request: NodeRequest) -> Vec<SessionAction> {
        self.handled = self.handled.saturating_add(1);

        if let NodeRequest::Register(handshake) = request {
            return vec![match self.node() {
                Some(node) => SessionAction::Refuse {
                    reason: RefusalReason::AlreadyRegistered { node: node.clone() },
                },
                None => SessionAction::Register {
                    handshake: Box::new(handshake),
                },
            }];
        }

        if !self.is_registered() {
            return vec![SessionAction::Refuse {
                reason: RefusalReason::NotRegistered,
            }];
        }

        match request {
            // Handled above; the arm keeps the match exhaustive.
            NodeRequest::Register(_) => Vec::new(),
            NodeRequest::Subscribe { inputs } => vec![SessionAction::Subscribe { inputs }],
            NodeRequest::SendMessage {
                output,
                metadata,
                payload,
            } => vec![SessionAction::Publish {
                output,
                metadata: Box::new(metadata),
                payload,
            }],
            NodeRequest::OutputDone { output } => vec![SessionAction::CloseOutput { output }],
            NodeRequest::CloseOutputs { outputs } => {
                vec![SessionAction::CloseOutputs { outputs }]
            }
            NodeRequest::NextEvent { timeout, max_batch } => vec![SessionAction::Deliver {
                timeout: timeout.map(astrs_wire::DurationMs::to_duration),
                max_batch: max_batch.clamp(1, MAX_EVENT_BATCH),
            }],
            NodeRequest::EventStreamDropped => {
                self.disconnected = true;
                vec![SessionAction::Disconnect]
            }
            NodeRequest::ExtStore { key, value, ttl } => {
                if key.is_writable_by_node() {
                    vec![SessionAction::ExtStore {
                        key,
                        value,
                        ttl: ttl.map(astrs_wire::DurationMs::to_duration),
                    }]
                } else {
                    vec![SessionAction::Refuse {
                        reason: RefusalReason::ReservedNamespace {
                            namespace: key.namespace,
                        },
                    }]
                }
            }
            NodeRequest::ExtLoad { key } => vec![SessionAction::ExtLoad { key }],
            NodeRequest::ExtDrop { key } => vec![SessionAction::ExtDrop { key }],
            NodeRequest::RouteUpgradeAck {
                output,
                accepted,
                reason,
            } => vec![SessionAction::RouteUpgradeAck {
                output,
                accepted,
                reason,
            }],
            NodeRequest::ReportDeadlineViolation {
                input,
                budget,
                latency,
            } => vec![SessionAction::ReportDeadlineViolation {
                input,
                budget,
                latency,
            }],
            // `NodeRequest` is `#[non_exhaustive]`: a verb this build does not
            // know is refused rather than silently ignored, so a newer node
            // learns its request went nowhere.
            _ => vec![SessionAction::Refuse {
                reason: RefusalReason::NotRegistered,
            }],
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_wire::{DurationMs, ExtensionNamespace};

    use super::*;

    fn dataflow() -> DataflowId {
        DataflowId::from_u128(7)
    }

    fn node() -> NodeId {
        NodeId::new("camera").unwrap()
    }

    fn data(name: &str) -> DataId {
        DataId::new(name).unwrap()
    }

    fn registered() -> SessionProtocol {
        let mut protocol = SessionProtocol::new(SessionId::from_u128(1));
        protocol.handle(NodeRequest::Register(NodeHandshake::new(
            dataflow(),
            node(),
            3,
        )));
        protocol.confirm_registration(dataflow(), node(), 3);
        protocol
    }

    #[test]
    fn a_fresh_session_knows_nothing() {
        let protocol = SessionProtocol::new(SessionId::from_u128(1));
        assert!(!protocol.is_registered());
        assert!(protocol.node().is_none());
        assert!(protocol.dataflow().is_none());
        assert!(protocol.generation().is_none());
        assert_eq!(protocol.handled(), 0);
        assert!(!protocol.is_disconnected());
        assert_eq!(protocol.session(), SessionId::from_u128(1));
    }

    #[test]
    fn everything_before_register_is_refused() {
        let requests = [
            NodeRequest::Subscribe { inputs: Vec::new() },
            NodeRequest::SendMessage {
                output: data("image"),
                metadata: Metadata::default(),
                payload: OutputPayload::inline(vec![1]),
            },
            NodeRequest::OutputDone {
                output: data("image"),
            },
            NodeRequest::CloseOutputs {
                outputs: Vec::new(),
            },
            NodeRequest::NextEvent {
                timeout: None,
                max_batch: 8,
            },
            NodeRequest::ExtLoad {
                key: ExtensionKey::user("k").unwrap(),
            },
        ];
        for request in requests {
            let mut protocol = SessionProtocol::new(SessionId::from_u128(1));
            let actions = protocol.handle(request);
            assert_eq!(
                actions,
                [SessionAction::Refuse {
                    reason: RefusalReason::NotRegistered
                }]
            );
        }
    }

    #[test]
    fn registering_yields_a_register_action() {
        let mut protocol = SessionProtocol::new(SessionId::from_u128(1));
        let handshake = NodeHandshake::new(dataflow(), node(), 3);
        let actions = protocol.handle(NodeRequest::Register(handshake.clone()));
        match actions.as_slice() {
            [SessionAction::Register { handshake: got }] => {
                assert_eq!(**got, handshake);
            }
            other => panic!("expected a register action, got {other:?}"),
        }
        assert_eq!(protocol.handled(), 1);
    }

    #[test]
    fn confirmation_is_what_makes_a_session_registered() {
        let mut protocol = SessionProtocol::new(SessionId::from_u128(1));
        protocol.handle(NodeRequest::Register(NodeHandshake::new(
            dataflow(),
            node(),
            3,
        )));
        assert!(
            !protocol.is_registered(),
            "the daemon has not accepted it yet"
        );

        protocol.confirm_registration(dataflow(), node(), 3);
        assert!(protocol.is_registered());
        assert_eq!(protocol.node(), Some(&node()));
        assert_eq!(protocol.dataflow(), Some(dataflow()));
        assert_eq!(protocol.generation(), Some(3));
    }

    #[test]
    fn a_second_registration_on_one_connection_is_refused() {
        let mut protocol = registered();
        let actions = protocol.handle(NodeRequest::Register(NodeHandshake::new(
            dataflow(),
            NodeId::new("impostor").unwrap(),
            0,
        )));
        assert_eq!(
            actions,
            [SessionAction::Refuse {
                reason: RefusalReason::AlreadyRegistered { node: node() }
            }]
        );
    }

    #[test]
    fn subscribing_carries_the_named_inputs() {
        let mut protocol = registered();
        let actions = protocol.handle(NodeRequest::Subscribe {
            inputs: vec![data("frames")],
        });
        assert_eq!(
            actions,
            [SessionAction::Subscribe {
                inputs: vec![data("frames")]
            }]
        );
    }

    #[test]
    fn publishing_carries_the_payload_unchanged() {
        let mut protocol = registered();
        let payload = OutputPayload::inline(vec![1, 2, 3]);
        let actions = protocol.handle(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: payload.clone(),
        });
        match actions.as_slice() {
            [
                SessionAction::Publish {
                    output,
                    payload: got,
                    ..
                },
            ] => {
                assert_eq!(*output, data("image"));
                assert_eq!(*got, payload);
            }
            other => panic!("expected a publish action, got {other:?}"),
        }
    }

    #[test]
    fn a_shared_memory_payload_passes_through_too() {
        let mut protocol = registered();
        let payload = OutputPayload::Shm {
            segment: "seg".into(),
            slot: 3,
            len: 4_096,
            generation: 2,
        };
        let actions = protocol.handle(NodeRequest::SendMessage {
            output: data("image"),
            metadata: Metadata::default(),
            payload: payload.clone(),
        });
        match actions.as_slice() {
            [SessionAction::Publish { payload: got, .. }] => {
                assert!(got.is_zero_copy());
                assert_eq!(*got, payload);
            }
            other => panic!("expected a publish action, got {other:?}"),
        }
    }

    #[test]
    fn closing_outputs_comes_in_both_shapes() {
        let mut protocol = registered();
        assert_eq!(
            protocol.handle(NodeRequest::OutputDone {
                output: data("image")
            }),
            [SessionAction::CloseOutput {
                output: data("image")
            }]
        );
        assert_eq!(
            protocol.handle(NodeRequest::CloseOutputs {
                outputs: Vec::new()
            }),
            [SessionAction::CloseOutputs {
                outputs: Vec::new()
            }]
        );
    }

    #[test]
    fn the_event_batch_is_clamped_at_both_ends() {
        let mut protocol = registered();
        for (asked, expected) in [(0u32, 1), (8, 8), (u32::MAX, MAX_EVENT_BATCH)] {
            let actions = protocol.handle(NodeRequest::NextEvent {
                timeout: None,
                max_batch: asked,
            });
            match actions.as_slice() {
                [SessionAction::Deliver { max_batch, .. }] => {
                    assert_eq!(*max_batch, expected, "asked for {asked}");
                }
                other => panic!("expected a deliver action, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_event_timeout_converts_to_a_duration() {
        let mut protocol = registered();
        let actions = protocol.handle(NodeRequest::NextEvent {
            timeout: Some(DurationMs::new(250)),
            max_batch: 4,
        });
        match actions.as_slice() {
            [SessionAction::Deliver { timeout, .. }] => {
                assert_eq!(*timeout, Some(Duration::from_millis(250)));
            }
            other => panic!("expected a deliver action, got {other:?}"),
        }
    }

    #[test]
    fn dropping_the_event_stream_disconnects() {
        let mut protocol = registered();
        let actions = protocol.handle(NodeRequest::EventStreamDropped);
        assert_eq!(actions, [SessionAction::Disconnect]);
        assert!(protocol.is_disconnected());
        assert!(actions[0].is_terminal());
    }

    #[test]
    fn a_user_namespace_store_is_allowed() {
        let mut protocol = registered();
        let key = ExtensionKey::user("pool").unwrap();
        let actions = protocol.handle(NodeRequest::ExtStore {
            key: key.clone(),
            value: vec![1],
            ttl: Some(DurationMs::new(500)),
        });
        assert_eq!(
            actions,
            [SessionAction::ExtStore {
                key,
                value: vec![1],
                ttl: Some(Duration::from_millis(500)),
            }]
        );
    }

    #[test]
    fn a_reserved_namespace_store_is_refused() {
        let mut protocol = registered();
        let key = ExtensionKey::new(ExtensionNamespace::GpuHandle, "pool").unwrap();
        let actions = protocol.handle(NodeRequest::ExtStore {
            key,
            value: vec![1],
            ttl: None,
        });
        assert_eq!(
            actions,
            [SessionAction::Refuse {
                reason: RefusalReason::ReservedNamespace {
                    namespace: ExtensionNamespace::GpuHandle
                }
            }]
        );
    }

    #[test]
    fn reading_and_dropping_a_reserved_key_is_still_allowed() {
        let mut protocol = registered();
        let key = ExtensionKey::new(ExtensionNamespace::GpuHandle, "pool").unwrap();
        assert_eq!(
            protocol.handle(NodeRequest::ExtLoad { key: key.clone() }),
            [SessionAction::ExtLoad { key: key.clone() }]
        );
        assert_eq!(
            protocol.handle(NodeRequest::ExtDrop { key: key.clone() }),
            [SessionAction::ExtDrop { key }]
        );
    }

    #[test]
    fn a_route_upgrade_acknowledgement_passes_through() {
        let mut protocol = registered();
        let actions = protocol.handle(NodeRequest::RouteUpgradeAck {
            output: data("image"),
            accepted: false,
            reason: Some("no shm".into()),
        });
        assert_eq!(
            actions,
            [SessionAction::RouteUpgradeAck {
                output: data("image"),
                accepted: false,
                reason: Some("no shm".into()),
            }]
        );
    }

    #[test]
    fn a_reported_deadline_violation_passes_through() {
        let mut protocol = registered();
        let actions = protocol.handle(NodeRequest::ReportDeadlineViolation {
            input: data("frames"),
            budget: DurationMs::new(50),
            latency: DurationMs::new(80),
        });
        assert_eq!(
            actions,
            [SessionAction::ReportDeadlineViolation {
                input: data("frames"),
                budget: DurationMs::new(50),
                latency: DurationMs::new(80),
            }]
        );
    }

    #[test]
    fn a_deadline_violation_before_registering_is_refused_like_anything_else() {
        let mut protocol = SessionProtocol::new(SessionId::from_u128(1));
        let actions = protocol.handle(NodeRequest::ReportDeadlineViolation {
            input: data("frames"),
            budget: DurationMs::new(50),
            latency: DurationMs::new(80),
        });
        assert!(matches!(actions.as_slice(), [SessionAction::Refuse { .. }]));
    }

    #[test]
    fn every_action_has_a_distinct_label() {
        let actions = [
            SessionAction::Register {
                handshake: Box::new(NodeHandshake::new(dataflow(), node(), 0)),
            },
            SessionAction::Subscribe { inputs: Vec::new() },
            SessionAction::Publish {
                output: data("o"),
                metadata: Box::new(Metadata::default()),
                payload: OutputPayload::empty(),
            },
            SessionAction::CloseOutput { output: data("o") },
            SessionAction::CloseOutputs {
                outputs: Vec::new(),
            },
            SessionAction::Deliver {
                timeout: None,
                max_batch: 1,
            },
            SessionAction::ExtStore {
                key: ExtensionKey::user("k").unwrap(),
                value: Vec::new(),
                ttl: None,
            },
            SessionAction::ExtLoad {
                key: ExtensionKey::user("k").unwrap(),
            },
            SessionAction::ExtDrop {
                key: ExtensionKey::user("k").unwrap(),
            },
            SessionAction::RouteUpgradeAck {
                output: data("o"),
                accepted: true,
                reason: None,
            },
            SessionAction::Disconnect,
            SessionAction::Refuse {
                reason: RefusalReason::NotRegistered,
            },
            SessionAction::ReportDeadlineViolation {
                input: data("frames"),
                budget: DurationMs::new(50),
                latency: DurationMs::new(80),
            },
        ];
        let mut names: Vec<&str> = actions.iter().map(SessionAction::kind_name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "labels collide");
        assert!(actions.iter().filter(|a| a.is_terminal()).count() == 1);
    }

    #[test]
    fn refusals_map_to_errors_and_render() {
        let session = SessionId::from_u128(1);
        for reason in [
            RefusalReason::NotRegistered,
            RefusalReason::AlreadyRegistered { node: node() },
            RefusalReason::ReservedNamespace {
                namespace: ExtensionNamespace::Internal,
            },
        ] {
            assert!(!reason.to_string().is_empty());
            assert!(!reason.kind_name().is_empty());
            let error = reason.to_error(session, dataflow());
            assert!(error.is_client_error(), "{error}");
        }
    }

    #[test]
    fn the_request_counter_advances() {
        let mut protocol = registered();
        let before = protocol.handled();
        protocol.handle(NodeRequest::Subscribe { inputs: Vec::new() });
        protocol.handle(NodeRequest::Subscribe { inputs: Vec::new() });
        assert_eq!(protocol.handled(), before + 2);
    }
}
