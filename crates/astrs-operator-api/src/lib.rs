//! The AstRS operator API.
//!
//! Operators are lightweight stages that share one event loop inside an
//! `astrs-runtime` host (blueprint §9.3):
//!
//! - [`Operator`]: the trait a stage implements — event handling plus
//!   optional `on_start`/`on_stop`/`on_reload` lifecycle hooks, all with
//!   default no-op bodies.
//! - [`OpEvent`]: a lean mirror of `astrs_wire::NodeEvent` — the five
//!   variants an operator, as opposed to the node hosting it, ever needs.
//! - [`OpOutput`]: the buffered send side an operator writes through
//!   instead of holding a connection.
//! - [`Status`]: what an [`Operator::on_event`] call decided —
//!   [`Status::Continue`] or [`Status::Finished`].
//! - [`OpError`] / [`OpResult`]: this crate's typed failure type.
//! - [`OperatorRegistry`] and [`register_operator!`](crate::register_operator): the static
//!   registration table `astrs-runtime` iterates to build the operators a
//!   manifest's `operators:` node names.
//!
//! Operators are Rust trait objects by default — the flagship,
//! zero-overhead path (blueprint §9.3). The optional `dylib` feature adds a
//! second path: [`export_dylib_operator!`](crate::export_dylib_operator)
//! exports a type through a stable `#[repr(C)]` vtable
//! ([`dylib::OperatorVTable`]) a shared library can be `dlopen`ed and
//! driven through, consumed by `astrs-runtime`'s `dylib-operators` feature
//! (blueprint §9.3, §22). See the [`dylib`] module docs for the ABI itself.
//!
//! # Typed messages
//!
//! `astrs-data` owns the [`astrs_data::AstrsMessage`] trait
//! (`astrs-operator-macros` — a normal dependency of this crate — hosts
//! the derive itself, `#[derive(AstrsMessage)]`, plus `#[operator]`
//! sugar for [`Operator`]; see that crate's docs for why the trait and the
//! derive that implements it live in different crates). Both the trait and
//! the derive/attribute macros are re-exported here — the trait as
//! [`AstrsMessage`], and (in the macro namespace, so the shared name is no
//! conflict) the derive as [`derive@AstrsMessage`] and the attribute as
//! [`macro@operator`] — so `use astrs_operator_api::*;` reaches everything
//! a node or operator author needs in one import, matching the unified
//! feel of blueprint §9.1's own `use astrs::prelude::*;`.
//!
//! # Quick tour
//!
//! ```
//! use astrs_operator_api::{Operator, OpEvent, OpOutput, OpResult, Status, register_operator};
//!
//! #[derive(Default)]
//! struct Echo;
//!
//! impl Operator for Echo {
//!     fn on_event(&mut self, event: &OpEvent, out: &mut OpOutput) -> OpResult<Status> {
//!         match event {
//!             OpEvent::Input { metadata, payload, .. } => {
//!                 out.send_bytes("echo", metadata.clone(), payload.clone())?;
//!                 Ok(Status::Continue)
//!             }
//!             OpEvent::Stop { .. } => Ok(Status::Finished),
//!             _ => Ok(Status::Continue),
//!         }
//!     }
//! }
//!
//! let registry = astrs_operator_api::OperatorRegistry::from_entries([
//!     register_operator!(Echo),
//! ])?;
//! let mut echo = registry.build("Echo")?;
//! let mut out = OpOutput::new();
//! # use astrs_time::HlcTimestamp;
//! # use astrs_wire::{DataId, Metadata};
//! let event = OpEvent::Input {
//!     id: DataId::new("in")?,
//!     source: "camera/image".parse()?,
//!     metadata: Metadata::new(HlcTimestamp::EPOCH),
//!     payload: vec![1, 2, 3],
//! };
//! assert_eq!(echo.on_event(&event, &mut out)?, Status::Continue);
//! assert_eq!(out.drain()[0].payload(), &[1, 2, 3]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#[cfg(feature = "dylib")]
pub mod dylib;
mod error;
mod event;
mod operator;
mod output;
mod registry;

pub use crate::error::{OpError, OpResult};
pub use crate::event::OpEvent;
pub use crate::operator::{Operator, Status};
pub use crate::output::{OpOutput, OpSend};
pub use crate::registry::{OperatorConstructor, OperatorRegistry};

pub use astrs_data::AstrsMessage;
// `AstrsMessage` here names the *derive macro* (macro namespace); the line
// above names the *trait* (type namespace) — the same pattern `serde`
// itself uses to offer `Serialize` as both a trait and a derive from one
// `use` path, and legal for the same reason: the two never collide.
pub use astrs_operator_macros::{AstrsMessage, operator};
