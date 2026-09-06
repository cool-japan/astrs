//! `cli ↔ coordinator`: [`ControlRequest`] and [`ControlReply`] (§7.3).
//!
//! | Module | Contents |
//! |---|---|
//! | [`request`] | [`ControlRequest`] — the thirty-five verbs of §24.1 |
//! | [`reply`] | [`ControlReply`] — the eleven §24.1 answers plus three tail appends |
//! | [`types`] | [`RequestScope`], [`DataflowSource`], [`LogQuery`], [`TopicQuery`], [`ParamScope`], [`ErrorCode`] |
//!
//! The leg is strictly request/response: one reply per request, in order, on
//! the same connection. Subscriptions opened by a request (`LogSubscribe`,
//! `TopicSubscribe`) deliver their data as separate [`crate::FrameKind::Log`]
//! and [`crate::FrameKind::Data`] frames tagged with a
//! [`crate::SubscriptionId`], never as replies — so a slow topic tap can never
//! delay the answer to the next command.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{ControlReply, ControlRequest, ErrorCode, FrameFlags, FrameLimits, WireMessage};
//!
//! let limits = FrameLimits::uds();
//! let request = ControlRequest::List { all: false };
//! let request_bytes = request.to_frame(FrameFlags::EMPTY, &limits)?;
//!
//! // The coordinator answers on the same connection.
//! let reply = match ControlRequest::from_bytes(&request_bytes, &limits)? {
//!     ControlRequest::List { .. } => ControlReply::DataflowList {
//!         dataflows: Vec::new(),
//!         nodes: Vec::new(),
//!     },
//!     _ => ControlReply::error(ErrorCode::Unsupported, "not handled here"),
//! };
//! assert!(reply.is_success());
//! # Ok::<(), astrs_wire::WireError>(())
//! ```

pub mod reply;
pub mod request;
pub mod types;

pub use reply::ControlReply;
pub use request::ControlRequest;
pub use types::{
    DEFAULT_LOG_LIMIT, DataflowSource, ErrorCode, LogQuery, ParamScope, RequestScope, TopicQuery,
};
