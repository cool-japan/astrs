//! [`InputQueue`] — the per-input bounded queue (blueprint §11.2).
//!
//! Every node input gets one of these: a bounded buffer with a configured
//! `queue_size` and [`QueuePolicy`], eviction-immune to Stop-class control
//! events and to messages correlated via `request_id`/`goal_id`/
//! `goal_status` ([`MetadataView::is_evict_immune`]), and instrumented with
//! the atomic depth/delivered/dropped counters a metrics sampler polls
//! without contending with the push/pop hot path.
//!
//! # Policy semantics
//!
//! Both [`QueuePolicy`] variants share one rule — an
//! [`is_evict_immune`](MetadataView::is_evict_immune) message is *always*
//! accepted and *never* chosen as an eviction victim — but otherwise behave
//! quite differently once the queue is full, matching the blueprint's own
//! wording for each:
//!
//! - **`DropOldest`** never refuses an incoming non-immune message: at
//!   capacity, it evicts the oldest non-immune message to make room.
//!   [`InputQueue::push`] returns
//!   [`PushOutcome::EnqueuedEvicting`] in the steady state.
//! - **`Backpressure`** never evicts: it buffers freely up to ten times
//!   `queue_size` ([`QueuePolicy::overflow_multiplier`]), then refuses
//!   (drops) further non-immune arrivals, reporting
//!   [`QueueSignal::BackpressureExhausted`] — the "must-log" signal the
//!   blueprint calls for. This crate does not log it; it surfaces it as
//!   data in [`PushReport::signal`] for the caller to log or meter.
//!
//! If every currently-queued message happens to be immune (so there is
//! nothing eligible to evict) and a non-immune message arrives at capacity,
//! that new message is the one dropped —
//! [`PushOutcome::DroppedIncoming`] with
//! [`QueueSignal::ImmuneOverflow`] — under *either* policy. The same signal
//! fires if an *immune* message's own unconditional acceptance pushes the
//! queue past its effective capacity: immune messages are never dropped,
//! but they are not free either, and a client flooding correlated requests
//! should be visible in telemetry.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use astrs_wire::{InputSpec, QueuePolicy};

use crate::error::{Result, SchedulerError};
use crate::metadata_view::MetadataView;
use crate::sync_util::lock;

/// What happened to the message passed to [`InputQueue::push`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// The message was appended; the queue was under its effective
    /// capacity (or the message is immune, which is always accepted).
    Enqueued,
    /// The message was appended after evicting one older, non-immune
    /// message to make room (the `DropOldest` steady state).
    EnqueuedEvicting,
    /// The message itself was refused: the queue is at its effective
    /// capacity and either the policy never evicts (`Backpressure`) or
    /// every currently-queued message is eviction-immune.
    DroppedIncoming,
}

/// A condition worth a caller's attention: a "must-log" drop, or an
/// escalation that eviction immunity itself cannot resolve.
///
/// [`InputQueue`] never logs or emits metrics on its own (this crate has no
/// logging dependency by design — see the crate docs); a signal is how it
/// hands that responsibility to whoever owns the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueSignal {
    /// `Backpressure` filled its ten-times-`queue_size` buffer and still
    /// had to drop an incoming message.
    BackpressureExhausted {
        /// The input's configured `queue_size`.
        queue_size: u32,
        /// How many messages were buffered at the moment of the drop
        /// (approximately `10 * queue_size`).
        buffered: u32,
    },
    /// The queue could not evict anything to make room, or an
    /// unconditionally-accepted immune message pushed it past its
    /// effective capacity: every message currently queued (or now queued)
    /// is eviction-immune.
    ///
    /// This is the scenario eviction immunity cannot make disappear —
    /// only relieve less politely (by growing without bound) than an
    /// ordinary drop would. It is reported so the caller can decide: log
    /// it, meter it, or apply back-pressure somewhere upstream of this
    /// queue (e.g. slow down accepting new service requests).
    ImmuneOverflow {
        /// How many eviction-immune messages are currently queued.
        immune_count: usize,
        /// The queue's effective capacity
        /// ([`QueuePolicy::overflow_multiplier`] applied to `queue_size`).
        capacity: u32,
    },
}

/// The result of one [`InputQueue::push`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PushReport {
    /// What happened to the pushed message.
    pub outcome: PushOutcome,
    /// A condition worth logging or metering, if this push triggered one.
    pub signal: Option<QueueSignal>,
}

/// A point-in-time reading of an [`InputQueue`]'s counters.
///
/// Cheap to take: every field is a relaxed atomic load, so a metrics
/// sampler (blueprint §13: every 2 s) never contends with the push/pop hot
/// path. The individual counters are read independently, so a snapshot
/// taken concurrently with in-flight pushes/pops is a plausible-but-not
/// necessarily-atomic-as-a-whole combination of them (`depth` might reflect
/// one more push than `delivered` has caught up to yet) — fine for a
/// monitoring gauge, not a substitute for the queue's own internal
/// consistency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueSnapshot {
    /// Messages currently queued.
    pub depth: u64,
    /// Of `depth`, how many are eviction-immune.
    pub immune_count: u64,
    /// The configured `queue_size`.
    pub capacity: u32,
    /// The effective capacity after the policy's overflow multiplier.
    pub effective_capacity: u32,
    /// Total messages ever popped via [`InputQueue::pop`].
    pub delivered: u64,
    /// Total messages ever refused or evicted.
    pub dropped: u64,
    /// Total times a push triggered [`QueueSignal::ImmuneOverflow`].
    pub immune_overflow_events: u64,
}

/// The mutex-guarded state: the messages themselves, plus the immune count
/// kept alongside them so eviction decisions never need to rescan the whole
/// queue just to answer "is everything here immune?".
struct Inner<T> {
    items: VecDeque<T>,
    immune_count: usize,
}

/// A bounded, policy-driven, eviction-immunity-aware queue for one node
/// input (blueprint §11.2).
///
/// Generic over the message type `T`; immunity-sensitive operations
/// ([`push`](InputQueue::push), [`pop`](InputQueue::pop)) additionally
/// require `T: `[`MetadataView`]. Cheap, capacity-and-policy-only queries
/// ([`len`](InputQueue::len), [`snapshot`](InputQueue::snapshot), …) do not.
///
/// Meant to be shared behind an [`std::sync::Arc`]: one producer side (fed
/// from a route/subscription) and one consumer side (typically
/// [`crate::EventMux`], which polls many of these fairly). The
/// producer and consumer never need each other's cooperation — `push` and
/// `pop` each take only the moment they need the internal lock.
///
/// # Examples
///
/// ```
/// use astrs_scheduler::{Envelope, InputQueue, PushOutcome};
/// use astrs_wire::QueuePolicy;
///
/// let queue: InputQueue<Envelope<u32>> = InputQueue::new(2, QueuePolicy::DropOldest)?;
/// queue.push(Envelope::new(1));
/// queue.push(Envelope::new(2));
/// // At capacity: the oldest non-immune message is evicted to make room.
/// let report = queue.push(Envelope::new(3));
/// assert_eq!(report.outcome, PushOutcome::EnqueuedEvicting);
/// assert_eq!(queue.pop().map(|e| e.payload), Some(2));
/// assert_eq!(queue.pop().map(|e| e.payload), Some(3));
/// # Ok::<(), astrs_scheduler::SchedulerError>(())
/// ```
pub struct InputQueue<T> {
    inner: Mutex<Inner<T>>,
    capacity: u32,
    effective_capacity: u32,
    policy: QueuePolicy,
    depth: AtomicU64,
    immune_count_gauge: AtomicU64,
    delivered: AtomicU64,
    dropped: AtomicU64,
    immune_overflow_events: AtomicU64,
}

// A hand-written `Debug` rather than `#[derive(Debug)]` on purpose: a
// derived impl would require `T: Debug` and would try to print every
// queued message. This prints the same summary `snapshot()` exposes
// instead, which is both more useful (depth/delivered/dropped at a glance)
// and available for every `T` with no bound at all.
impl<T> std::fmt::Debug for InputQueue<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = self.snapshot();
        f.debug_struct("InputQueue")
            .field("policy", &self.policy)
            .field("capacity", &self.capacity)
            .field("effective_capacity", &self.effective_capacity)
            .field("depth", &snapshot.depth)
            .field("delivered", &snapshot.delivered)
            .field("dropped", &snapshot.dropped)
            .finish()
    }
}

impl<T> InputQueue<T> {
    /// Creates a queue with the given `queue_size` and [`QueuePolicy`].
    ///
    /// # Errors
    ///
    /// [`SchedulerError::ZeroCapacity`] if `capacity` is zero.
    pub fn new(capacity: u32, policy: QueuePolicy) -> Result<Self> {
        if capacity == 0 {
            return Err(SchedulerError::ZeroCapacity);
        }
        let effective_capacity = capacity.saturating_mul(policy.overflow_multiplier());
        Ok(Self {
            inner: Mutex::new(Inner {
                items: VecDeque::new(),
                immune_count: 0,
            }),
            capacity,
            effective_capacity,
            policy,
            depth: AtomicU64::new(0),
            immune_count_gauge: AtomicU64::new(0),
            delivered: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            immune_overflow_events: AtomicU64::new(0),
        })
    }

    /// Creates a queue from a manifest-resolved [`InputSpec`], reusing its
    /// `queue_size` and `queue_policy` fields directly.
    ///
    /// # Errors
    ///
    /// [`SchedulerError::ZeroCapacity`] if `spec.queue_size` is zero.
    pub fn from_spec(spec: &InputSpec) -> Result<Self> {
        Self::new(spec.queue_size, spec.queue_policy)
    }

    /// The configured `queue_size`.
    #[must_use]
    pub const fn capacity(&self) -> u32 {
        self.capacity
    }

    /// The effective capacity: `capacity` scaled by the policy's
    /// [`overflow_multiplier`](QueuePolicy::overflow_multiplier) (1× for
    /// `DropOldest`, 10× for `Backpressure`).
    #[must_use]
    pub const fn effective_capacity(&self) -> u32 {
        self.effective_capacity
    }

    /// The configured [`QueuePolicy`].
    #[must_use]
    pub const fn policy(&self) -> QueuePolicy {
        self.policy
    }

    /// The number of messages currently queued.
    ///
    /// Takes the internal lock for an exact reading; see
    /// [`InputQueue::snapshot`] for a lock-free approximate one suited to a
    /// metrics-sampling loop.
    #[must_use]
    pub fn len(&self) -> usize {
        lock(&self.inner).items.len()
    }

    /// Whether the queue is currently empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A lock-free, point-in-time reading of this queue's counters.
    #[must_use]
    pub fn snapshot(&self) -> QueueSnapshot {
        QueueSnapshot {
            depth: self.depth.load(Ordering::Relaxed),
            immune_count: self.immune_count_gauge.load(Ordering::Relaxed),
            capacity: self.capacity,
            effective_capacity: self.effective_capacity,
            delivered: self.delivered.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            immune_overflow_events: self.immune_overflow_events.load(Ordering::Relaxed),
        }
    }

    /// Mirrors the locked state into the lock-free gauges. Called with the
    /// lock already held, right before it is released.
    fn sync_gauges(&self, inner: &Inner<T>) {
        self.depth
            .store(inner.items.len() as u64, Ordering::Relaxed);
        self.immune_count_gauge
            .store(inner.immune_count as u64, Ordering::Relaxed);
    }
}

impl<T: MetadataView> InputQueue<T> {
    /// Pushes a message, applying the queue's policy and eviction-immunity
    /// rule (see the module docs for the exact semantics).
    pub fn push(&self, item: T) -> PushReport {
        let immune = item.is_evict_immune();
        let mut inner = lock(&self.inner);
        let at_ceiling = inner.items.len() as u32 >= self.effective_capacity;

        if !at_ceiling {
            inner.items.push_back(item);
            if immune {
                inner.immune_count += 1;
            }
            self.sync_gauges(&inner);
            return PushReport {
                outcome: PushOutcome::Enqueued,
                signal: None,
            };
        }

        if immune {
            // Immune messages are always accepted, even over the nominal
            // ceiling — that is what "never evicted" has to mean once the
            // queue is already saturated with them.
            inner.items.push_back(item);
            inner.immune_count += 1;
            let overflowed = inner.items.len() as u32 > self.effective_capacity;
            let immune_count = inner.immune_count;
            self.sync_gauges(&inner);
            drop(inner);
            let signal = if overflowed {
                self.immune_overflow_events.fetch_add(1, Ordering::Relaxed);
                Some(QueueSignal::ImmuneOverflow {
                    immune_count,
                    capacity: self.effective_capacity,
                })
            } else {
                None
            };
            return PushReport {
                outcome: PushOutcome::Enqueued,
                signal,
            };
        }

        // The incoming message is not immune, and the queue is at or over
        // its effective ceiling.
        match self.policy {
            QueuePolicy::DropOldest => {
                // `immune_count == items.len()` means every queued message
                // is immune, so no non-immune victim can possibly exist —
                // skip the O(n) scan below entirely rather than walking the
                // whole queue only to confirm what the counter already
                // proves. This is the queue's own pathological worst case
                // (a client flooding correlated requests until the queue is
                // wall-to-wall immune), so it is exactly the path most
                // worth keeping O(1).
                if inner.immune_count >= inner.items.len() {
                    // Every queued message is immune: there is nothing
                    // eligible to evict, so the new (non-immune) arrival is
                    // the one sacrificed instead.
                    let immune_count = inner.immune_count;
                    drop(inner);
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    self.immune_overflow_events.fetch_add(1, Ordering::Relaxed);
                    return PushReport {
                        outcome: PushOutcome::DroppedIncoming,
                        signal: Some(QueueSignal::ImmuneOverflow {
                            immune_count,
                            capacity: self.effective_capacity,
                        }),
                    };
                }
                // `immune_count < items.len()` guarantees at least one
                // non-immune message is present, so `position` below always
                // finds a victim; the `unwrap_or` fallback exists only to
                // keep this line panic-free if that invariant were ever
                // violated, not because the `None` case is expected.
                let victim = inner
                    .items
                    .iter()
                    .position(|existing| !existing.is_evict_immune())
                    .unwrap_or(0);
                let _ = inner.items.remove(victim);
                inner.items.push_back(item);
                self.dropped.fetch_add(1, Ordering::Relaxed);
                self.sync_gauges(&inner);
                PushReport {
                    outcome: PushOutcome::EnqueuedEvicting,
                    signal: None,
                }
            }
            QueuePolicy::Backpressure => {
                let buffered = inner.items.len() as u32;
                drop(inner);
                self.dropped.fetch_add(1, Ordering::Relaxed);
                PushReport {
                    outcome: PushOutcome::DroppedIncoming,
                    signal: Some(QueueSignal::BackpressureExhausted {
                        queue_size: self.capacity,
                        buffered,
                    }),
                }
            }
            // `QueuePolicy` is `#[non_exhaustive]` upstream. A future
            // variant this build does not understand is handled the
            // conservative way: refuse to grow further, but never guess at
            // what would be safe to evict.
            _ => {
                drop(inner);
                self.dropped.fetch_add(1, Ordering::Relaxed);
                PushReport {
                    outcome: PushOutcome::DroppedIncoming,
                    signal: None,
                }
            }
        }
    }

    /// Pops the oldest queued message, if any.
    pub fn pop(&self) -> Option<T> {
        let mut inner = lock(&self.inner);
        let item = inner.items.pop_front()?;
        if item.is_evict_immune() {
            inner.immune_count = inner.immune_count.saturating_sub(1);
        }
        self.sync_gauges(&inner);
        drop(inner);
        self.delivered.fetch_add(1, Ordering::Relaxed);
        Some(item)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::metadata_view::Envelope;

    type Q = InputQueue<Envelope<u32>>;

    fn plain(n: u32) -> Envelope<u32> {
        Envelope::new(n)
    }

    fn immune(n: u32) -> Envelope<u32> {
        let mut meta = astrs_wire::Metadata::new(astrs_time::HlcTimestamp::EPOCH);
        meta.set_request_id("r");
        Envelope::with_metadata(n, meta)
    }

    #[test]
    fn zero_capacity_is_rejected() {
        assert_eq!(
            Q::new(0, QueuePolicy::DropOldest).unwrap_err(),
            SchedulerError::ZeroCapacity
        );
    }

    #[test]
    fn from_spec_reuses_queue_size_and_policy() {
        let spec = InputSpec::new(
            astrs_wire::DataId::new("frames").unwrap(),
            astrs_wire::PortRef::new(
                astrs_wire::NodeId::new("camera").unwrap(),
                astrs_wire::DataId::new("out").unwrap(),
            ),
        )
        .with_queue(3, QueuePolicy::Backpressure);
        let queue: Q = InputQueue::from_spec(&spec).unwrap();
        assert_eq!(queue.capacity(), 3);
        assert_eq!(queue.policy(), QueuePolicy::Backpressure);
        assert_eq!(queue.effective_capacity(), 30);
    }

    #[test]
    fn under_capacity_pushes_are_plain_enqueues() {
        let queue = Q::new(2, QueuePolicy::DropOldest).unwrap();
        assert_eq!(queue.push(plain(1)).outcome, PushOutcome::Enqueued);
        assert_eq!(queue.push(plain(2)).outcome, PushOutcome::Enqueued);
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn drop_oldest_evicts_the_front_non_immune_message() {
        let queue = Q::new(2, QueuePolicy::DropOldest).unwrap();
        queue.push(plain(1));
        queue.push(plain(2));
        let report = queue.push(plain(3));
        assert_eq!(report.outcome, PushOutcome::EnqueuedEvicting);
        assert!(report.signal.is_none());
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.pop().unwrap().payload, 2);
        assert_eq!(queue.pop().unwrap().payload, 3);
    }

    #[test]
    fn drop_oldest_skips_immune_messages_when_choosing_a_victim() {
        let queue = Q::new(2, QueuePolicy::DropOldest).unwrap();
        queue.push(immune(1));
        queue.push(plain(2));
        let report = queue.push(plain(3));
        assert_eq!(report.outcome, PushOutcome::EnqueuedEvicting);
        // The immune message survives; the non-immune one was evicted.
        let remaining: Vec<u32> = std::iter::from_fn(|| queue.pop())
            .map(|e| e.payload)
            .collect();
        assert_eq!(remaining, vec![1, 3]);
    }

    #[test]
    fn drop_oldest_drops_incoming_when_everything_queued_is_immune() {
        let queue = Q::new(2, QueuePolicy::DropOldest).unwrap();
        queue.push(immune(1));
        queue.push(immune(2));
        let report = queue.push(plain(3));
        assert_eq!(report.outcome, PushOutcome::DroppedIncoming);
        assert!(matches!(
            report.signal,
            Some(QueueSignal::ImmuneOverflow {
                immune_count: 2,
                ..
            })
        ));
        assert_eq!(
            queue.len(),
            2,
            "the incoming message was refused, not appended"
        );
    }

    #[test]
    fn immune_incoming_is_always_accepted_even_over_capacity() {
        let queue = Q::new(1, QueuePolicy::DropOldest).unwrap();
        queue.push(immune(1));
        let report = queue.push(immune(2));
        assert_eq!(report.outcome, PushOutcome::Enqueued);
        assert!(matches!(
            report.signal,
            Some(QueueSignal::ImmuneOverflow {
                immune_count: 2,
                capacity: 1
            })
        ));
        assert_eq!(queue.len(), 2, "grew past the nominal capacity of 1");
    }

    #[test]
    fn backpressure_buffers_without_evicting_up_to_ten_times_queue_size() {
        let queue = Q::new(2, QueuePolicy::Backpressure).unwrap();
        for n in 0..20 {
            let report = queue.push(plain(n));
            assert_eq!(
                report.outcome,
                PushOutcome::Enqueued,
                "message {n} should buffer"
            );
        }
        assert_eq!(queue.len(), 20);
        assert_eq!(queue.effective_capacity(), 20);
    }

    #[test]
    fn backpressure_drops_once_the_ten_times_buffer_is_exhausted() {
        let queue = Q::new(2, QueuePolicy::Backpressure).unwrap();
        for n in 0..20 {
            queue.push(plain(n));
        }
        let report = queue.push(plain(999));
        assert_eq!(report.outcome, PushOutcome::DroppedIncoming);
        assert!(matches!(
            report.signal,
            Some(QueueSignal::BackpressureExhausted {
                queue_size: 2,
                buffered: 20
            })
        ));
        assert_eq!(
            queue.len(),
            20,
            "the queue never shrinks to accept a new message"
        );
    }

    #[test]
    fn backpressure_never_evicts_a_non_immune_message_to_make_room() {
        let queue = Q::new(1, QueuePolicy::Backpressure).unwrap();
        for n in 0..10 {
            queue.push(plain(n));
        }
        // At the 10x ceiling now; the oldest message must still be there.
        assert_eq!(queue.len(), 10);
        let first = queue.pop().unwrap();
        assert_eq!(
            first.payload, 0,
            "backpressure preserves arrival order, never evicts"
        );
    }

    #[test]
    fn pop_on_empty_queue_returns_none() {
        let queue = Q::new(4, QueuePolicy::DropOldest).unwrap();
        assert!(queue.pop().is_none());
    }

    #[test]
    fn delivered_and_dropped_counters_are_accurate() {
        let queue = Q::new(1, QueuePolicy::DropOldest).unwrap();
        queue.push(plain(1)); // enqueued
        queue.push(plain(2)); // evicts 1
        let _ = queue.pop(); // delivered += 1
        let snap = queue.snapshot();
        assert_eq!(snap.delivered, 1);
        assert_eq!(snap.dropped, 1);
        assert_eq!(snap.depth, 0);
    }

    #[test]
    fn snapshot_tracks_immune_count() {
        let queue = Q::new(4, QueuePolicy::DropOldest).unwrap();
        queue.push(plain(1));
        queue.push(immune(2));
        let snap = queue.snapshot();
        assert_eq!(snap.depth, 2);
        assert_eq!(snap.immune_count, 1);
        let _ = queue.pop(); // pops the plain one (FIFO)
        assert_eq!(queue.snapshot().immune_count, 1);
        let _ = queue.pop(); // pops the immune one
        assert_eq!(queue.snapshot().immune_count, 0);
    }

    #[test]
    fn is_empty_reflects_length() {
        let queue = Q::new(1, QueuePolicy::DropOldest).unwrap();
        assert!(queue.is_empty());
        queue.push(plain(1));
        assert!(!queue.is_empty());
    }
}
