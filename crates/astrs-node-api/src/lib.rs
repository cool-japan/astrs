//! **astrs-node-api** — the AstRS node API (blueprint §9.1).
//!
//! The flagship user-facing surface: everything a robotics node needs to join
//! a dataflow, read its inputs and publish its outputs, and nothing it does
//! not.
//!
//! ```no_run
//! use astrs_node_api::prelude::*;
//!
//! fn main() -> Result<(), NodeError> {
//!     let (mut node, mut events) = Node::init_from_env()?;
//!     let mut echo = node.raw_output("echo")?;
//!
//!     while let Some(event) = events.recv() {
//!         match event {
//!             Event::Input { id, data, meta } if id == "frames" => {
//!                 echo.send_bytes(data.to_vec(), meta.follow())?;
//!             }
//!             Event::Stop(_) => break,
//!             _ => {}
//!         }
//!     }
//!     Ok(())
//! }
//! ```
//!
//! # The surface, in one table
//!
//! | Area | Entry points |
//! |---|---|
//! | Init | [`Node::init_from_env`], [`Node::builder`], [`Node::init_from_node_id`], [`Node::init_testing`] |
//! | Events | [`EventStream::recv`], [`EventStream::recv_async`], `Iterator`, [`Stream`], [`EventStream::merge_external`] |
//! | Sending | [`Output::send`], [`RawOutput::send_bytes`], [`RawOutput::send_batch`], [`RawOutput::send_array`], [`RawOutput::allocate`] (and [`RawOutput::send_arrow`] under `arrow-interop`) |
//! | Patterns | [`Node::service_request`], [`Node::service_response`], [`Node::goal`], [`Node::goal_status`], [`Node::stream_chunk`] |
//! | Logging | [`Node::log_error`] … [`Node::log_trace`], [`Node::log_with_fields`] |
//! | Introspection | [`Node::id`], [`Node::dataflow_id`], [`Node::descriptor`], [`Node::is_restart`], [`Node::restart_count`], [`Node::hlc_now`] |
//! | Extensions | [`Node::ext_store`], [`Node::ext_load`], [`Node::ext_drop`] |
//!
//! # How the pieces fit
//!
//! ```text
//!   Node ──────────────► SessionHandle ──► writer task ──► framed UDS/TCP ──► daemon
//!    │                        ▲                                    │
//!    │ output::<T>()          │ NodeRequest                        │ NodeEvent
//!    ▼                        │                                    ▼
//!   Output<T> ── threshold ──►│                            reader task ──► EventMux
//!    │            (§6.2)      │                                    │      (queue policy)
//!    └── SHM Producer ────────┘                                    ▼
//!         (after RouteUpgrade, §6.3)                        EventStream ──► your loop
//! ```
//!
//! Every layer is testable without a daemon: [`Node::init_testing`] spins an
//! in-process [`testing::MockDaemon`] speaking the real §7.3 wire
//! protocol over a `tokio::io::duplex` pair, so a node's whole lifecycle —
//! register, subscribe, inputs, patterns, stop — is a unit test.
//!
//! # Zero copy, both ways
//!
//! §6.3's slow-start handshake is wired at **both** ends, and a node writes no
//! code for either:
//!
//! | End | What arrives | What this crate does |
//! |---|---|---|
//! | producer | `NodeEvent::RouteUpgrade` | opens a shared-memory producer; [`RawOutput::allocate`] hands back the ring slot itself ([`session::routes`]) |
//! | consumer | `NodeEvent::InputRouteUpgrade` | attaches the segment and reads samples in place, so `Event::Input`'s payload is [`Payload::is_zero_copy`] ([`session::inputs`]) |
//!
//! Both directions fall back the same way: a downgrade, a dead producer or a
//! stale generation puts the route back on the reliable daemon path, and the
//! node's event loop does not notice. [`Node::route_planes`] and
//! [`Node::input_planes`] report where each end currently is; a message's
//! [`Payload::slot`] says where in the ring it came from, which is what turns
//! "zero copy" into something a test can check.
//!
//! [`Payload::zero_copy`] stays public for a node that brokers a segment
//! itself — a recorder replaying into a ring, an embedder outside the
//! daemon's graph — but nothing in a normal pipeline needs it any more.
//!
//! # Features
//!
//! | Feature | Default | What it adds |
//! |---|---|---|
//! | `arrow-interop` | no | [`RawOutput::send_arrow`] / [`Output::send_arrow`] — publishing arrow-rs values through `astrs-data`'s bridge (blueprint §6.1, §9.1). See [`output::arrow`]. |

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod env;
pub mod error;
pub mod events;
pub mod message;
pub mod node;
pub mod orphan;
pub mod output;
pub mod patterns;
pub mod payload;
pub mod runtime;
pub mod session;
pub mod signal;
pub mod testing;

pub use error::{NodeError, Result};
pub use events::{Event, EventStream, Stream};
pub use message::{AstrsMessage, FromPayload};
pub use node::{Node, NodeBuilder};
pub use output::{Output, OutputSample, RawOutput};
pub use patterns::{
    ActionOutcome, GoalId, GoalTracker, RequestId, ServiceRequest, ServiceResponse,
    StreamAssembler, StreamWriter,
};
pub use payload::{Payload, PayloadKind, SlotLocation};
pub use signal::{Signal, WaitOutcome};
pub use testing::{MockDaemon, TestHarness};

/// The read-only views [`Payload::view`] decodes into (blueprint §9.1's
/// `let img: ImageView = data.view()?;`).
///
/// They belong to `astrs-data` — a view is a checked window over columns
/// somebody else built, and the layout rules it checks are that crate's — but
/// the line that *names* them is a node's event loop, so a node reaches them
/// here rather than through a second crate. [`ImageView`] has a
/// [`FromPayload`] implementation (see [`message::media`]); [`TensorView`] is
/// the general n-dimensional form the same module builds on.
pub use astrs_data::tensor::{ImageView, TensorView};

/// Everything a node's `main` needs, in one import.
///
/// ```no_run
/// use astrs_node_api::prelude::*;
///
/// fn main() -> Result<(), NodeError> {
///     let (mut node, mut events) = Node::init_from_env()?;
///     while let Some(event) = events.recv() {
///         if event.is_stop() {
///             break;
///         }
///     }
///     let _ = &mut node;
///     Ok(())
/// }
/// ```
pub mod prelude {
    pub use crate::env::TypeCheckMode;
    pub use crate::error::{NodeError, Result};
    pub use crate::events::{Event, EventStream, Stream};
    pub use crate::message::{AstrsMessage, FromPayload};
    pub use crate::node::{Node, NodeBuilder};
    /// The error `send_arrow` reports, for an application that names it in a
    /// signature rather than only `?`-ing it.
    #[cfg(feature = "arrow-interop")]
    #[cfg_attr(docsrs, doc(cfg(feature = "arrow-interop")))]
    pub use crate::output::{ArrowSendError, ArrowSendResult};
    pub use crate::output::{Output, OutputSample, RawOutput};
    pub use crate::patterns::{
        ActionOutcome, GoalId, GoalTracker, RequestId, ServiceRequest, ServiceResponse,
        StreamAssembler, StreamWriter,
    };
    pub use crate::payload::{Payload, PayloadKind, SlotLocation};
    /// The zero-copy views §9.1's flagship loop names on its very first line
    /// inside the match arm (`let img: ImageView = data.view()?;`). Without
    /// them here that line needs a second `use` naming `astrs-data`, which is
    /// exactly the shape the facade exists to avoid.
    pub use astrs_data::tensor::{ImageView, TensorView};
    pub use astrs_wire::{DataId, DataflowId, GoalStatus, Metadata, NodeId, PortRef, StopCause};
}
