//! Real-thread concurrency stress tests.
//!
//! Every other test in this crate drives [`InputQueue`]/[`EventMux`] from a
//! single thread — including the property test in
//! `proptest_bounded_memory.rs`, which fuzzes the *sequence* of operations
//! but still applies them one at a time, sequentially, to the same queue.
//! That proves the state-machine logic is correct for any interleaving a
//! single caller could produce, but neither type's actual documented usage
//! is single-threaded: `InputQueue`'s docs say it is "meant to be shared
//! behind an `Arc`: one producer side ... and one consumer side", and
//! `EventMux`'s say "many producer tasks feed the mux concurrently without
//! contending on its registry lock". This file is the test that a bug in
//! the locking itself — a lost update, a torn counter, a message that is
//! both delivered and counted as dropped — would actually be able to
//! surface, by driving many real OS threads (for `InputQueue`, which has no
//! async dependency of its own) or many real tokio tasks (for `EventMux`)
//! concurrently against shared state.
//!
//! The invariant every test here checks is the same one, restated per
//! scenario: **every message pushed is accounted for exactly once** — as
//! delivered, as dropped, or as still queued at the end — never zero times
//! (lost) and never twice (double-counted).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_scheduler::{Envelope, EventMux, InputQueue};
use astrs_time::HlcTimestamp;
use astrs_wire::{DataId, Metadata, PriorityLane, QueuePolicy};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;

const PRODUCER_THREADS: u32 = 8;
const PUSHES_PER_PRODUCER: u32 = 2_000;
const TOTAL_PUSHES: u32 = PRODUCER_THREADS * PUSHES_PER_PRODUCER;

fn plain(n: u32) -> Envelope<u32> {
    Envelope::new(n)
}

fn immune(n: u32) -> Envelope<u32> {
    let mut meta = Metadata::new(HlcTimestamp::EPOCH);
    meta.set_request_id("r");
    Envelope::with_metadata(n, meta)
}

/// `PRODUCER_THREADS` real OS threads push concurrently while a consumer
/// thread drains at the same time (rather than after producers finish),
/// maximizing actual lock contention between pushers and the popper.
/// Regardless of scheduling, every pushed message must be accounted for
/// exactly once between what the consumer received and the queue's own
/// `dropped` counter, with nothing left behind once every producer has
/// finished and the queue is drained to empty.
#[test]
fn drop_oldest_accounts_for_every_message_exactly_once_under_real_concurrent_access() {
    let queue: Arc<InputQueue<Envelope<u32>>> =
        Arc::new(InputQueue::new(16, QueuePolicy::DropOldest).unwrap());
    let delivered_count = Arc::new(AtomicU32::new(0));
    let producers_done = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let consumer = {
        let queue = Arc::clone(&queue);
        let delivered_count = Arc::clone(&delivered_count);
        let producers_done = Arc::clone(&producers_done);
        thread::spawn(move || {
            loop {
                while queue.pop().is_some() {
                    delivered_count.fetch_add(1, Ordering::Relaxed);
                }
                if producers_done.load(Ordering::Acquire) {
                    // `producers_done` only ever becomes `true` after the
                    // test thread has `join`-ed every producer, which
                    // (transitively, through `join`'s own happens-before
                    // guarantee and this flag's release/acquire pairing)
                    // means every push has already completed and is
                    // visible. But that does *not* mean this loop's *own*
                    // drain pass above necessarily ran after every push —
                    // a push can race in between that drain and this check.
                    // One final drain, strictly after observing the flag,
                    // is what actually closes that window: at this point no
                    // thread anywhere can push again, so this pass is
                    // guaranteed to see everything that is ever going to
                    // exist in the queue.
                    while queue.pop().is_some() {
                        delivered_count.fetch_add(1, Ordering::Relaxed);
                    }
                    break;
                }
                thread::yield_now();
            }
        })
    };

    let producers: Vec<_> = (0..PRODUCER_THREADS)
        .map(|producer_id| {
            let queue = Arc::clone(&queue);
            thread::spawn(move || {
                for n in 0..PUSHES_PER_PRODUCER {
                    // A mix of plain and (occasionally) immune messages from
                    // every producer, so eviction pressure and immunity
                    // bookkeeping both happen under real contention.
                    let value = producer_id * PUSHES_PER_PRODUCER + n;
                    if n.is_multiple_of(97) {
                        queue.push(immune(value));
                    } else {
                        queue.push(plain(value));
                    }
                }
            })
        })
        .collect();

    for producer in producers {
        producer.join().expect("producer thread must not panic");
    }
    producers_done.store(true, Ordering::Release);
    consumer.join().expect("consumer thread must not panic");

    let snapshot = queue.snapshot();
    assert_eq!(
        snapshot.depth, 0,
        "the consumer drains to empty after producers finish"
    );
    let delivered = delivered_count.load(Ordering::Relaxed);
    assert_eq!(
        u64::from(delivered),
        snapshot.delivered,
        "the queue's own counter must agree with what was actually popped"
    );
    assert_eq!(
        u64::from(delivered) + snapshot.dropped,
        u64::from(TOTAL_PUSHES),
        "every pushed message must be accounted for exactly once: delivered + dropped == total pushed"
    );
}

/// The `Backpressure` counterpart: with a capacity generous enough that no
/// legitimate drop should occur (`effective_capacity` comfortably above
/// `TOTAL_PUSHES`), concurrent producers must never lose a message —
/// `dropped` stays exactly zero and every single push is eventually
/// delivered.
#[test]
fn backpressure_drops_nothing_under_concurrent_access_when_capacity_is_not_exceeded() {
    // effective_capacity = queue_size * 10; comfortably above TOTAL_PUSHES.
    let queue_size = (TOTAL_PUSHES / 5) + 1;
    let queue: Arc<InputQueue<Envelope<u32>>> =
        Arc::new(InputQueue::new(queue_size, QueuePolicy::Backpressure).unwrap());

    let producers: Vec<_> = (0..PRODUCER_THREADS)
        .map(|producer_id| {
            let queue = Arc::clone(&queue);
            thread::spawn(move || {
                for n in 0..PUSHES_PER_PRODUCER {
                    let value = producer_id * PUSHES_PER_PRODUCER + n;
                    queue.push(plain(value));
                }
            })
        })
        .collect();
    for producer in producers {
        producer.join().expect("producer thread must not panic");
    }

    let snapshot = queue.snapshot();
    assert_eq!(
        snapshot.dropped, 0,
        "capacity was never exceeded, so nothing should be dropped"
    );
    assert_eq!(snapshot.depth, u64::from(TOTAL_PUSHES));

    let mut received: Vec<u32> = std::iter::from_fn(|| queue.pop())
        .map(|e| e.payload)
        .collect();
    assert_eq!(received.len(), TOTAL_PUSHES as usize);
    received.sort_unstable();
    received.dedup();
    assert_eq!(
        received.len(),
        TOTAL_PUSHES as usize,
        "every value must appear exactly once -- no duplicate delivery under concurrent pushes"
    );
}

/// All messages immune, pushed concurrently from every producer thread,
/// with no consumer draining at all: the queue must grow to hold every
/// single one, under either policy, exactly as the single-threaded
/// `matrix_all_immune_stream_never_drops_regardless_of_policy_or_capacity`
/// test in `policy_matrix.rs` proves sequentially -- this is the same
/// property proven under real concurrent contention on the queue's mutex
/// instead.
#[test]
fn concurrent_producers_never_cause_an_immune_message_to_be_dropped() {
    for policy in [QueuePolicy::DropOldest, QueuePolicy::Backpressure] {
        let queue: Arc<InputQueue<Envelope<u32>>> = Arc::new(InputQueue::new(4, policy).unwrap());
        let producers: Vec<_> = (0..PRODUCER_THREADS)
            .map(|producer_id| {
                let queue = Arc::clone(&queue);
                thread::spawn(move || {
                    for n in 0..PUSHES_PER_PRODUCER {
                        let value = producer_id * PUSHES_PER_PRODUCER + n;
                        let report = queue.push(immune(value));
                        assert_ne!(
                            report.outcome,
                            astrs_scheduler::PushOutcome::DroppedIncoming,
                            "policy={policy}: an immune message must never be dropped"
                        );
                    }
                })
            })
            .collect();
        for producer in producers {
            producer.join().expect("producer thread must not panic");
        }

        let snapshot = queue.snapshot();
        assert_eq!(snapshot.dropped, 0, "policy={policy}");
        assert_eq!(snapshot.depth, u64::from(TOTAL_PUSHES), "policy={policy}");
        assert_eq!(
            snapshot.immune_count,
            u64::from(TOTAL_PUSHES),
            "policy={policy}"
        );
    }
}

/// `EventMux`'s async side: several tokio tasks push into a mix of control
/// and data inputs concurrently while one task calls `recv()` in a loop.
/// Every message pushed by every producer must be received exactly once,
/// with no loss and no duplication, despite the producers never
/// synchronizing with each other or with the consumer beyond the mux's own
/// internal locking.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mux_concurrent_producers_and_a_single_consumer_deliver_every_message_exactly_once() {
    const INPUTS: usize = 6;
    const PUSHES_PER_INPUT: u32 = 3_000;
    const TOTAL: u32 = INPUTS as u32 * PUSHES_PER_INPUT;

    let mux: Arc<EventMux<Envelope<(usize, u32)>>> = Arc::new(EventMux::new());
    // `Backpressure`'s 10x multiplier means `queue_size` here only needs to
    // clear `PUSHES_PER_INPUT / 10`; sized well past that on purpose, so
    // this test's only concern is "no loss, no duplication under real
    // concurrency", never "did the consumer drain fast enough to avoid a
    // legitimate capacity drop" — that scenario is already covered by the
    // single-threaded backpressure tests in `queue.rs` and
    // `policy_matrix.rs`.
    let queue_size = (PUSHES_PER_INPUT / 10) + 100;
    let mut handles = Vec::new();
    for i in 0..INPUTS {
        let lane = if i % 2 == 0 {
            PriorityLane::Control
        } else {
            PriorityLane::Data
        };
        let handle = mux
            .register_input(
                DataId::new(format!("in{i}")).unwrap(),
                queue_size,
                QueuePolicy::Backpressure,
                lane,
            )
            .unwrap();
        handles.push(handle);
    }

    let producers: Vec<_> = handles
        .into_iter()
        .enumerate()
        .map(|(i, handle)| {
            tokio::spawn(async move {
                for n in 0..PUSHES_PER_INPUT {
                    handle.push(Envelope::new((i, n)));
                    if n.is_multiple_of(37) {
                        tokio::task::yield_now().await;
                    }
                }
            })
        })
        .collect();

    let consumer = {
        let mux = Arc::clone(&mux);
        tokio::spawn(async move {
            let mut received: Vec<(usize, u32)> = Vec::with_capacity(TOTAL as usize);
            while received.len() < TOTAL as usize {
                let (_, event) = mux.recv().await;
                received.push(event.payload);
            }
            received
        })
    };

    for producer in producers {
        producer.await.expect("producer task must not panic");
    }
    let mut received = consumer.await.expect("consumer task must not panic");

    assert_eq!(received.len(), TOTAL as usize);
    received.sort_unstable();
    let mut expected: Vec<(usize, u32)> = (0..INPUTS)
        .flat_map(|i| (0..PUSHES_PER_INPUT).map(move |n| (i, n)))
        .collect();
    expected.sort_unstable();
    assert_eq!(
        received, expected,
        "every (input, sequence) pair must arrive exactly once"
    );
}
