//! Property test: bounded memory under eviction immunity.
//!
//! The naive statement of this invariant — "total buffered never exceeds
//! `queue_size × overflow_multiplier`" — is false by this crate's own
//! design: an eviction-immune message is *always* accepted, even past the
//! ceiling, because dropping it would wedge a client forever (blueprint
//! §11.2). The property actually worth proving is the one that would make
//! "just drop immune messages too" a visible regression rather than a
//! silent one:
//!
//! 1. **Non-immune backlog is strictly bounded.** At every point in any
//!    operation sequence, the number of *non-immune* messages currently
//!    queued never exceeds the effective capacity — unbounded memory
//!    growth is only possible by flooding the queue with eviction-immune
//!    messages, which is a distinct, already-surfaced condition
//!    ([`astrs_scheduler::QueueSignal::ImmuneOverflow`]), not a leak.
//! 2. **An eviction-immune push is never reported as dropped.** Across the
//!    whole sequence, [`astrs_scheduler::PushOutcome::DroppedIncoming`]
//!    never occurs for a message flagged immune.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_scheduler::{Envelope, InputQueue, PushOutcome};
use astrs_time::HlcTimestamp;
use astrs_wire::{Metadata, QueuePolicy};
use proptest::prelude::*;

#[derive(Debug, Clone, Copy)]
enum Op {
    Push { immune: bool },
    Pop,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        any::<bool>().prop_map(|immune| Op::Push { immune }),
        Just(Op::Pop),
    ]
}

fn envelope(immune: bool) -> Envelope<u32> {
    if immune {
        let mut meta = Metadata::new(HlcTimestamp::EPOCH);
        meta.set_request_id("r");
        Envelope::with_metadata(0, meta)
    } else {
        Envelope::new(0)
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// For any capacity, either policy, and any sequence of pushes (each
    /// independently immune or not) interleaved with pops, the two
    /// properties above hold after every single operation — not just at
    /// the end of the sequence, so a bug that transiently overshoots and
    /// then "recovers" is still caught.
    #[test]
    fn non_immune_backlog_is_bounded_and_immune_is_never_dropped(
        capacity in 1u32..12,
        use_backpressure in any::<bool>(),
        ops in prop::collection::vec(op_strategy(), 0..300),
    ) {
        let policy = if use_backpressure {
            QueuePolicy::Backpressure
        } else {
            QueuePolicy::DropOldest
        };
        let queue: InputQueue<Envelope<u32>> = InputQueue::new(capacity, policy)
            .expect("capacity is always >= 1 in this test");

        for op in ops {
            match op {
                Op::Push { immune } => {
                    let report = queue.push(envelope(immune));
                    if immune {
                        prop_assert_ne!(
                            report.outcome,
                            PushOutcome::DroppedIncoming,
                            "an eviction-immune message must never be the one dropped"
                        );
                    }
                }
                Op::Pop => {
                    let _ = queue.pop();
                }
            }

            let snapshot = queue.snapshot();
            let non_immune_backlog = snapshot.depth - snapshot.immune_count;
            prop_assert!(
                non_immune_backlog <= u64::from(snapshot.effective_capacity),
                "non-immune backlog {non_immune_backlog} exceeded effective capacity {}",
                snapshot.effective_capacity
            );
        }
    }

    /// A stream of *only* immune pushes still keeps every single one (the
    /// most direct exercise of "never evicted, never dropped"), and the
    /// resulting depth matches the push count exactly — no silent loss.
    #[test]
    fn an_all_immune_stream_keeps_every_message(
        capacity in 1u32..6,
        use_backpressure in any::<bool>(),
        push_count in 0usize..500,
    ) {
        let policy = if use_backpressure {
            QueuePolicy::Backpressure
        } else {
            QueuePolicy::DropOldest
        };
        let queue: InputQueue<Envelope<u32>> = InputQueue::new(capacity, policy)
            .expect("capacity is always >= 1 in this test");

        for _ in 0..push_count {
            let report = queue.push(envelope(true));
            prop_assert_eq!(report.outcome, PushOutcome::Enqueued);
        }

        prop_assert_eq!(queue.snapshot().depth, push_count as u64);
        prop_assert_eq!(queue.snapshot().immune_count, push_count as u64);
    }
}
