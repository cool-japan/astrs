//! The queue policy matrix: every `queue_size` from 1 up to a handful,
//! crossed with both [`QueuePolicy`] variants, crossed with several
//! eviction-immunity mixes (none / all / alternating / front-loaded /
//! back-loaded). Complements the proptest invariant
//! (`tests/proptest_bounded_memory.rs`, which checks the bound holds for
//! *any* sequence) with exact, precomputable expectations for a
//! deliberately chosen set of sequences — including the *content* left in
//! the queue, not just its length, which is what actually distinguishes
//! `DropOldest` ("keeps the newest") from `Backpressure` ("keeps the
//! oldest, then refuses").

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_scheduler::{Envelope, InputQueue, PushOutcome};
use astrs_time::HlcTimestamp;
use astrs_wire::{Metadata, QueuePolicy};

/// Every immune/non-immune pattern this matrix exercises, as a predicate
/// over a push's 0-based index.
type ImmunePattern = fn(usize) -> bool;

const PATTERNS: &[(&str, ImmunePattern)] = &[
    ("none", |_| false),
    ("all", |_| true),
    ("alternating", |i| i % 2 == 0),
    ("front_loaded", |i| i < 3),
    ("back_loaded_relative", |i| i >= 3),
];

fn envelope(index: u32, immune: bool) -> Envelope<u32> {
    if immune {
        let mut meta = Metadata::new(HlcTimestamp::EPOCH);
        meta.set_request_id("r");
        Envelope::with_metadata(index, meta)
    } else {
        Envelope::new(index)
    }
}

fn drain(queue: &InputQueue<Envelope<u32>>) -> Vec<u32> {
    std::iter::from_fn(|| queue.pop())
        .map(|e| e.payload)
        .collect()
}

/// For every `capacity` in `1..=6`, both policies, and every pattern in
/// [`PATTERNS`]: push `3 * effective_capacity` messages and check the two
/// properties that must hold regardless of pattern (immune messages are
/// never dropped; the non-immune backlog never exceeds the effective
/// capacity), by inspecting the queue's own bookkeeping.
#[test]
fn matrix_never_drops_an_immune_message_and_bounds_non_immune_backlog() {
    for capacity in 1u32..=6 {
        for policy in QueuePolicy::ALL.iter().copied() {
            for &(pattern_name, pattern) in PATTERNS {
                let queue: InputQueue<Envelope<u32>> = InputQueue::new(capacity, policy)
                    .unwrap_or_else(|e| panic!("capacity {capacity} must be valid: {e}"));
                let effective_capacity = queue.effective_capacity();
                let total = (effective_capacity as usize) * 3;

                for i in 0..total {
                    let immune = pattern(i);
                    let report = queue.push(envelope(i as u32, immune));
                    if immune {
                        assert_ne!(
                            report.outcome,
                            PushOutcome::DroppedIncoming,
                            "capacity={capacity} policy={policy} pattern={pattern_name} index={i}: \
                             an immune message must never be dropped"
                        );
                    }

                    let snapshot = queue.snapshot();
                    let non_immune_backlog = snapshot.depth - snapshot.immune_count;
                    assert!(
                        non_immune_backlog <= u64::from(effective_capacity),
                        "capacity={capacity} policy={policy} pattern={pattern_name} index={i}: \
                         non-immune backlog {non_immune_backlog} exceeded {effective_capacity}"
                    );
                }
            }
        }
    }
}

/// `DropOldest` with no immune messages at all: the classic case, checked
/// for exact surviving *content* (not just length) — it must always be the
/// most recent `capacity` messages, in arrival order.
#[test]
fn drop_oldest_with_no_immune_messages_keeps_exactly_the_newest_capacity_messages() {
    for capacity in 1u32..=6 {
        let queue: InputQueue<Envelope<u32>> =
            InputQueue::new(capacity, QueuePolicy::DropOldest).unwrap();
        let total = capacity * 3;
        for i in 0..total {
            queue.push(envelope(i, false));
        }
        let survivors = drain(&queue);
        let expected: Vec<u32> = ((total - capacity)..total).collect();
        assert_eq!(survivors, expected, "capacity={capacity}");

        let snapshot = queue.snapshot();
        assert_eq!(snapshot.dropped, u64::from(total - capacity));
    }
}

/// `Backpressure` with no immune messages: the mirror image — it keeps
/// exactly the *oldest* `effective_capacity` messages (never evicting),
/// refusing everything after.
#[test]
fn backpressure_with_no_immune_messages_keeps_exactly_the_oldest_effective_capacity_messages() {
    for capacity in 1u32..=6 {
        let queue: InputQueue<Envelope<u32>> =
            InputQueue::new(capacity, QueuePolicy::Backpressure).unwrap();
        let effective_capacity = queue.effective_capacity();
        let total = effective_capacity * 3;
        for i in 0..total {
            queue.push(envelope(i, false));
        }
        let survivors = drain(&queue);
        let expected: Vec<u32> = (0..effective_capacity).collect();
        assert_eq!(survivors, expected, "capacity={capacity}");

        let snapshot = queue.snapshot();
        assert_eq!(snapshot.dropped, u64::from(total - effective_capacity));
    }
}

/// A fully immune stream, for every capacity and policy: nothing is ever
/// dropped, and the queue grows past its nominal capacity to hold every
/// one of them (the documented "immune overflow" trade — never lose data,
/// grow instead).
#[test]
fn matrix_all_immune_stream_never_drops_regardless_of_policy_or_capacity() {
    for capacity in 1u32..=6 {
        for policy in QueuePolicy::ALL.iter().copied() {
            let queue: InputQueue<Envelope<u32>> = InputQueue::new(capacity, policy).unwrap();
            let total = capacity * 4;
            for i in 0..total {
                let report = queue.push(envelope(i, true));
                assert_eq!(
                    report.outcome,
                    PushOutcome::Enqueued,
                    "capacity={capacity} policy={policy} index={i}"
                );
            }
            let survivors = drain(&queue);
            let expected: Vec<u32> = (0..total).collect();
            assert_eq!(
                survivors, expected,
                "capacity={capacity} policy={policy}: every immune message must survive, in order"
            );
        }
    }
}
