//! The AstRS C API: a stable `extern "C"` node surface for non-Rust callers.
//!
//! C, C++, and every other language with a C FFI get the same node lifecycle
//! the Rust API offers — connect to the daemon, receive input events, publish
//! outputs, report status — through an opaque-handle ABI in which no Rust
//! type ever appears in a signature. The crate builds as `staticlib` and
//! `cdylib` (linkable from a C build) alongside the ordinary `lib` target
//! (usable, and doctestable, from Rust). The hand-written header lives at
//! `include/astrs.h`.
//!
//! # This crate exports a C ABI; it never compiles C
//!
//! The workspace's pure-Rust policy (blueprint §18.1) bans crates that
//! *compile* C. Exporting a C-compatible ABI from Rust is the opposite
//! direction and involves no C toolchain at all: no `cc`, no build script, no
//! `-sys` dependency. `astrs-capi` is what lets a C program call AstRS, not
//! what lets AstRS call C.
//!
//! # The surface, in one table
//!
//! | Area | Entry points |
//! |---|---|
//! | Version / limits | [`astrs_version`], [`astrs_max_payload_bytes`] |
//! | Init / teardown | [`astrs_init_node_from_env`], [`astrs_init_node_from_config`], [`astrs_node_destroy`] |
//! | Events | [`astrs_node_next_event`], [`astrs_free_event`] |
//! | Event accessors | [`astrs_event_type`], [`astrs_event_input_id`], [`astrs_event_payload`], [`astrs_event_metadata_key_count`], [`astrs_event_metadata_key_at`] |
//! | Sending | [`astrs_send_output`] |
//! | Errors | [`astrs_last_error_message`] |
//!
//! # A complete event loop, from Rust
//!
//! `examples/node_lifecycle.rs` shows the same call sequence a C caller
//! makes, written in Rust against the raw `extern "C"` functions rather than
//! this crate's types directly — that is the shape any binding in any other
//! language ends up with. The short version:
//!
//! ```text
//!   astrs_init_node_from_env(&node)
//!     loop {
//!       astrs_node_next_event(node, timeout_ms, &event)
//!       switch (astrs_event_type(event)) {
//!         case ASTRS_EVENT_INPUT:
//!           astrs_event_payload(event, &ptr, &len);
//!           astrs_send_output(node, "out", 3, NULL, 0, ptr, len);
//!           break;
//!         case ASTRS_EVENT_STOP:
//!           goto done;
//!       }
//!       astrs_free_event(event);
//!     }
//!   done:
//!   astrs_node_destroy(node);
//! ```
//!
//! # Status codes, never unwinding
//!
//! A Rust panic crossing an FFI boundary is undefined behaviour. Every entry
//! point therefore catches, converts and returns an [`AstrsStatus`] instead
//! of propagating — including the "something went wrong in a way we did not
//! model" case, which becomes [`AstrsStatus::Panic`] rather than an abort.
//!
//! `Ok` is `0` and every failure is negative, so the idiomatic C-side test is
//! the one a C programmer would write anyway:
//!
//! ```c
//! if (astrs_send_output(node, "frames", 6, NULL, 0, buf, len) < 0) { /* handle */ }
//! ```
//!
//! Three functions are the deliberate exception, returning a value directly
//! rather than an [`AstrsStatus`], because none of them can fail even in
//! principle: [`astrs_version`], [`astrs_max_payload_bytes`] and
//! [`astrs_last_error_message`].
//!
//! ```
//! use astrs_capi::AstrsStatus;
//!
//! assert_eq!(AstrsStatus::Ok as i32, 0);
//! assert!(AstrsStatus::Ok.is_ok());
//!
//! for failure in [
//!     AstrsStatus::InvalidArgument,
//!     AstrsStatus::NotConnected,
//!     AstrsStatus::Closed,
//!     AstrsStatus::Timeout,
//!     AstrsStatus::UnknownPort,
//!     AstrsStatus::TypeMismatch,
//!     AstrsStatus::Internal,
//!     AstrsStatus::Panic,
//! ] {
//!     assert!(!failure.is_ok());
//!     assert!((failure as i32) < 0, "{failure:?} must be negative");
//! }
//! ```
//!
//! # Thread safety
//!
//! A single [`AstrsNode`] pointer (and any [`AstrsEvent`] it produced) must
//! be used by at most one thread at a time, with one exception: once an event
//! is returned by [`astrs_node_next_event`], it is read-only, so its own
//! accessors may be called from multiple threads concurrently as long as
//! each supplies its own output destinations. See the doc comments on
//! [`AstrsNode`] and [`AstrsEvent`] for the full contract — the same one
//! dora's C API documents for the same shape of type (dora-rs/dora#540).
//!
//! # Errors
//!
//! Every fallible entry point also records a human-readable diagnostic on a
//! **thread-local** last-error slot, read back with
//! [`astrs_last_error_message`]. Check the status first; the message is only
//! meaningful after a call that reported failure, and only from the same
//! thread that made it.

mod event;
mod ffi;
mod node;
mod status;

#[cfg(test)]
mod tests;

pub use event::{
    ASTRS_TIMEOUT_INFINITE, AstrsEvent, AstrsEventType, astrs_event_input_id,
    astrs_event_metadata_key_at, astrs_event_metadata_key_count, astrs_event_payload,
    astrs_event_type, astrs_free_event, astrs_node_next_event,
};
pub use node::{
    AstrsNode, astrs_init_node_from_config, astrs_init_node_from_env, astrs_max_payload_bytes,
    astrs_node_destroy, astrs_send_output, astrs_version,
};
pub use status::{AstrsStatus, astrs_last_error_message};
