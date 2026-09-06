//! Services, actions and streams (blueprint §9.4).
//!
//! > All three ride ordinary edges plus metadata correlation — no separate RPC
//! > subsystem: services (`request_id`), actions (`goal_id`/`goal_status` FSM:
//! > Accepted→Executing→{Succeeded, Aborted, Canceled}), streams
//! > (`session_id`/`segment_id`/`seq`/`fin`/`flush`).
//!
//! That sentence is the whole design. There is no service socket, no action
//! server, no stream channel — there are edges, and there is
//! [`astrs_wire::Metadata`]. What this module adds is the bookkeeping that
//! makes the three patterns *correct* rather than merely expressible:
//!
//! | Pattern | What is easy to get wrong | What this module does |
//! |---|---|---|
//! | [`service`] | Answering with the wrong (or no) `request_id` | [`ServiceRequest`] carries the id; the response helper copies it |
//! | [`action`] | An illegal FSM transition, or a status after a terminal one | [`GoalTracker`] refuses both |
//! | [`stream`] | Gaps, reordering, a segment that never ends | [`StreamAssembler`] detects all three |
//!
//! # Eviction immunity comes for free
//!
//! A message carrying `request_id`, `goal_id` or `goal_status` is
//! eviction-immune in every input queue (§11.2), because
//! [`astrs_wire::Metadata::is_correlated`] says so and
//! [`crate::events::QueuedEvent`] forwards that to the scheduler. So a service
//! response cannot be the message a full queue drops — which is the one drop
//! that would wedge a client forever.

pub mod action;
pub mod service;
pub mod stream;

pub use action::{ActionOutcome, GoalId, GoalTracker};
pub use service::{RequestId, ServiceRequest, ServiceResponse};
pub use stream::{ChunkRef, StreamAssembler, StreamSegment, StreamWriter};

/// A fresh, unique correlation id.
///
/// UUIDv7, so ids sort by creation time — which makes a log of correlated
/// messages readable in the order they happened without a separate timestamp
/// column.
#[must_use]
pub fn fresh_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn fresh_ids_are_unique_and_time_ordered() {
        let first = fresh_id();
        let second = fresh_id();
        assert_ne!(first, second);
        assert!(first <= second, "v7 ids sort by creation time");
        assert_eq!(first.len(), 36, "canonical hyphenated form");
    }
}
