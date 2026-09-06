//! `daemon ↔ node`: [`NodeRequest`] and [`NodeEvent`] (§7.3).
//!
//! | Module | Contents |
//! |---|---|
//! | [`request`] | [`NodeRequest`] — the eleven §24.1 verbs a node uses, plus one tail append |
//! | [`event`] | [`NodeEvent`] — the thirteen §24.1 events plus five tail appends |
//! | [`types`] | [`NodeHandshake`], [`OutputPayload`], [`ExtensionKey`], [`ExtensionNamespace`] |
//! | [`config`] | [`NodeConfig`] — the `ASTRS_NODE_CONFIG` blob the daemon hands a node before it can connect at all |
//!
//! This is the busiest leg in the system: every message a node sends or
//! receives crosses it until the route is upgraded to shared memory (§6.3),
//! after which the payloads bypass it entirely and only the control events
//! remain.
//!
//! ```text
//!   node                                       daemon
//!    ├──────── Register(NodeHandshake) ──────────►│
//!    │◄─────── Registered { spec } ───────────────┤
//!    ├──────── Subscribe { inputs } ─────────────►│
//!    │◄─────── Input { id, metadata, payload } ───┤
//!    ├──────── SendMessage { output, payload } ──►│
//!    │◄─────── RouteUpgrade { segment } ──────────┤   (§6.3)
//!    ├──────── RouteUpgradeAck ──────────────────►│
//!    ├──────── OutputDone / CloseOutputs ────────►│
//!    │◄─────── Stop { cause } ────────────────────┤
//! ```
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{
//!     DataflowId, FrameFlags, FrameLimits, NodeHandshake, NodeId, NodeRequest, WireMessage,
//! };
//!
//! let limits = FrameLimits::uds();
//! let register = NodeRequest::Register(NodeHandshake::new(
//!     DataflowId::from_u128(1),
//!     NodeId::new("camera")?,
//!     3,
//! ));
//! let bytes = register.to_frame(FrameFlags::EMPTY, &limits)?;
//! let decoded = NodeRequest::from_bytes(&bytes, &limits)?;
//! assert_eq!(decoded.handshake().map(|handshake| handshake.generation), Some(3));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod config;
pub mod event;
pub mod request;
pub mod types;

pub use config::{
    DEFAULT_ZERO_COPY_THRESHOLD, ENV_NODE_CONFIG, ENV_RUN_PARENT_PID, NodeConfig, NodeConfigError,
};
pub use event::NodeEvent;
pub use request::{DEFAULT_EVENT_BATCH, NodeRequest};
pub use types::{
    ExtensionKey, ExtensionNamespace, MAX_EXTENSION_NAME_LEN, NodeHandshake, OutputPayload,
};
