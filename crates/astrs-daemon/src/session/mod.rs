//! Node sessions — the daemon↔node leg (§7.3, §24.1).
//!
//! | Module | Concern |
//! |---|---|
//! | [`protocol`] | The conversation as a pure state machine: request in, actions out |
//! | [`actor`] | One task per socket: read requests, write events, report the close |
//! | [`channel`] | The internal event channel every background task reports through |
//!
//! The split is the point. [`protocol::SessionProtocol`] has no I/O and no
//! daemon state, so a whole conversation can be replayed against a scripted
//! fake node in a synchronous test; [`actor::SessionActor`] has no state at
//! all, so a socket that misbehaves cannot corrupt anything; and
//! [`channel::DaemonHandle`] is the only way anything reaches the event loop,
//! so the loop's state has exactly one writer.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::session::{SessionAction, SessionProtocol};
//! use astrs_wire::{DataflowId, NodeHandshake, NodeId, NodeRequest, SessionId};
//!
//! let mut protocol = SessionProtocol::new(SessionId::from_u128(1));
//! let handshake = NodeHandshake::new(DataflowId::from_u128(1), NodeId::new("camera")?, 0);
//! let actions = protocol.handle(NodeRequest::Register(handshake));
//! assert_eq!(actions.len(), 1);
//! assert_eq!(actions[0].kind_name(), "register");
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

pub mod actor;
pub mod channel;
pub mod protocol;

pub use actor::{SESSION_OUTBOX_DEPTH, SessionActor, SessionSink, encode_event, encode_request};
pub use channel::{DaemonEvent, DaemonEvents, DaemonHandle, event_channel};
pub use protocol::{MAX_EVENT_BATCH, RefusalReason, SessionAction, SessionProtocol};
