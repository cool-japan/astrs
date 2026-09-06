//! The crate's unified error type.

use thiserror::Error;

use astrs_wire::DataId;

/// Everything that can go wrong configuring or driving the scheduler:
/// malformed queue configuration and duplicate input registration.
///
/// Runtime facts that are not configuration mistakes — a queue at
/// capacity, a deadline miss, a coalesced timer tick — are not errors at
/// all; they are ordinary values ([`crate::PushReport`],
/// [`crate::DeadlineOutcome`], [`crate::TimerFired`]) that
/// the caller inspects and, if it chooses, turns into a log line or a
/// metric. This crate never logs on their behalf (see the crate docs).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SchedulerError {
    /// An [`InputQueue`](crate::InputQueue) was asked for a
    /// `queue_size` of zero.
    ///
    /// A zero-capacity queue cannot meaningfully apply either
    /// [`QueuePolicy`](astrs_wire::QueuePolicy): `DropOldest` would have to
    /// evict the item it just accepted before returning, and
    /// `Backpressure`'s "buffer to 10×" ceiling would also be zero. Rejected
    /// at construction rather than silently rounded up to one, so a
    /// misconfigured manifest fails loudly at `validate` time instead of
    /// quietly behaving like `queue_size: 1`.
    #[error("input queue capacity must be at least 1 (queue_size was 0)")]
    ZeroCapacity,

    /// [`EventMux::register_input`](crate::EventMux::register_input)
    /// was called twice with the same [`DataId`].
    #[error("input {0} is already registered on this event mux")]
    DuplicateInput(DataId),
}

/// Result alias used throughout this crate.
pub type Result<T> = std::result::Result<T, SchedulerError>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn zero_capacity_message_is_actionable() {
        let err = SchedulerError::ZeroCapacity;
        assert!(err.to_string().contains("queue_size"));
    }

    #[test]
    fn duplicate_input_message_names_the_id() {
        let id = DataId::new("frames").unwrap();
        let err = SchedulerError::DuplicateInput(id.clone());
        assert!(err.to_string().contains("frames"));
        assert_eq!(err, SchedulerError::DuplicateInput(id));
    }
}
