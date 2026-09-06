// `missing_docs` (workspace lint) would otherwise fire on a non-unix target:
// `#![cfg(unix)]` below makes this whole crate empty there, which strips the
// module doc comment along with everything else, so this `allow` has to sit
// ahead of that line to survive the stripping.
#![cfg_attr(not(unix), allow(missing_docs))]
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! A proptest state machine over the slot lifecycle.
//!
//! Blueprint §23 risk #3 asks for a "loom/proptest interleaving suite" before
//! any dependent crate lands. `loom` is not on the retained-crate list
//! (§18.1), so this is the proptest half: instead of exploring *thread*
//! interleavings, it explores **operation** interleavings — arbitrary
//! sequences of publish / receive / hold / drop / attach / detach against a
//! real segment, with a model that says exactly what each one must produce.
//!
//! That trade is a real one and worth naming: a single-threaded driver cannot
//! find a missing barrier. What it *can* find, and what the thread-based
//! [`torture`](../torture.rs) suite cannot check exhaustively, is a wrong
//! *predicate* — a reclamation rule that is too permissive for some ordering
//! of attaches and holds, an off-by-one in the lag arithmetic, a cursor that
//! advances without being accounted for. The two suites are complementary and
//! neither subsumes the other.
//!
//! # The invariants
//!
//! Checked after **every** operation, on every slot and every consumer:
//!
//! 1. `state != READY` ⟹ the gate is closed. This is
//!    [`SlotSnapshot::violation`]'s first rule: "no slot is readable while
//!    WRITING", and "no FREE slot has a nonzero refcount", stated over the
//!    `(state, refcount)` **pair** rather than over `state` alone — which is
//!    the only way to state it that has teeth.
//! 2. A READY slot carries a nonzero sequence number.
//! 3. Per-slot sequence numbers are monotone non-decreasing.
//! 4. `write_seq` and `reclaimed_seq` are monotone, and
//!    `reclaimed_seq <= write_seq`.
//! 5. A slot with a live [`astrs_shm::Sample`] on it is pinned and READY.
//! 6. For every consumer: `received + lagged == cursor - start_cursor`.
//! 7. Under [`OverflowPolicy::Block`], `Lagged` never occurs — a direct
//!    cross-check of the reclamation predicate, since the producer must
//!    refuse to recycle any slot a cursor has not passed.
//! 8. Delivered payloads carry the sequence number they claim.

use std::collections::BTreeMap;
use std::sync::Arc;

use astrs_shm::{
    AttachOptions, Consumer, OverflowPolicy, Producer, RECLAIM_SENTINEL, RecvError, Sample,
    Segment, SegmentConfig, SegmentKey, ShmError, SlotState,
};
use astrs_wire::DataflowId;
use proptest::prelude::*;

/// One step the driver can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// Publish a message of this many bytes.
    Publish(u16),
    /// Try to receive on consumer `index`.
    Receive(u8),
    /// Receive and *keep* the sample pinned.
    ReceiveAndHold(u8),
    /// Drop the oldest held sample of consumer `index`.
    ReleaseHeld(u8),
    /// Attach another consumer.
    Attach,
    /// Detach consumer `index`.
    Detach(u8),
    /// Ask the producer to close the segment.
    Close,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        // Weighted towards publish/receive: those are the operations whose
        // interleavings the reclamation predicate actually turns on.
        8 => (0u16..600).prop_map(Op::Publish),
        8 => (0u8..4).prop_map(Op::Receive),
        3 => (0u8..4).prop_map(Op::ReceiveAndHold),
        3 => (0u8..4).prop_map(Op::ReleaseHeld),
        2 => Just(Op::Attach),
        2 => (0u8..4).prop_map(Op::Detach),
        1 => Just(Op::Close),
    ]
}

/// What the model expects of one consumer.
#[derive(Debug)]
struct ModelConsumer {
    consumer: Consumer,
    start_cursor: u64,
    received: u64,
    lagged: u64,
    last_delivered: u64,
    held: Vec<Sample>,
}

struct Model {
    segment: Arc<Segment>,
    producer: Producer,
    consumers: Vec<ModelConsumer>,
    policy: OverflowPolicy,
    slot_count: u32,
    /// The highest sequence each slot has ever carried, for the monotonicity
    /// check.
    slot_high_water: BTreeMap<u32, u64>,
    last_write_seq: u64,
    last_reclaimed_seq: u64,
    closed: bool,
    published: u64,
}

impl Model {
    fn new(slots: u32, payload: u32, policy: OverflowPolicy) -> Self {
        let key =
            SegmentKey::from_parts(DataflowId::generate(), "model", "out", 1).expect("valid ids");
        let config = SegmentConfig::new(slots, payload)
            .expect("valid geometry")
            .with_overflow(policy)
            .with_max_consumers(8)
            .expect("valid consumer table");
        let segment = Segment::create_shared(key, config).expect("segment");
        let producer = Producer::new(Arc::clone(&segment)).expect("producer");
        Self {
            segment,
            producer,
            consumers: Vec::new(),
            policy,
            slot_count: slots,
            slot_high_water: BTreeMap::new(),
            last_write_seq: 0,
            last_reclaimed_seq: 0,
            closed: false,
            published: 0,
        }
    }

    fn apply(&mut self, op: Op) -> Result<(), TestCaseError> {
        match op {
            Op::Publish(len) => self.publish(len as usize)?,
            Op::Receive(index) => self.receive(index as usize, false)?,
            Op::ReceiveAndHold(index) => self.receive(index as usize, true)?,
            Op::ReleaseHeld(index) => self.release_held(index as usize),
            Op::Attach => self.attach()?,
            Op::Detach(index) => self.detach(index as usize),
            Op::Close => {
                self.producer.close();
                self.closed = true;
            }
        }
        self.check_invariants()
    }

    fn publish(&mut self, len: usize) -> Result<(), TestCaseError> {
        let capacity = self.producer.payload_capacity();
        let len = len.min(capacity);
        // The outcome is computed and the borrow released before any
        // invariant is consulted, so the checks below can see the whole model.
        let outcome = match self.producer.try_allocate(len) {
            Ok(mut window) => {
                let seq = window.seq();
                stamp(window.as_mut_slice(), seq);
                Ok(window.commit(b"m").expect("commit"))
            }
            Err(err) => Err(err),
        };
        match outcome {
            Ok(published) => {
                prop_assert_eq!(published, self.published + 1, "sequences must be dense");
                self.published = published;
                Ok(())
            }
            Err(ShmError::PoolExhausted { .. }) => {
                // Legal, and the whole point of the `Block` policy. Assert
                // the reason is real: some slot must be either unread by a
                // cursor or pinned by a live sample.
                prop_assert!(
                    self.some_slot_is_unreclaimable(),
                    "the pool reported exhaustion with nothing holding it"
                );
                Ok(())
            }
            Err(ShmError::Closed { .. }) => {
                prop_assert!(self.closed, "only a closed segment may refuse with Closed");
                Ok(())
            }
            Err(other) => Err(TestCaseError::fail(format!(
                "unexpected publish error: {other}"
            ))),
        }
    }

    /// Whether at least one slot is legitimately unreclaimable right now.
    fn some_slot_is_unreclaimable(&self) -> bool {
        if self.consumers.is_empty() {
            // With no consumers the only possible holder is a live sample,
            // which requires a consumer — so this would be a real bug.
            return false;
        }
        (0..self.slot_count).any(|index| {
            let slot = self.segment.slot(index);
            let snapshot = slot.snapshot();
            let pinned = snapshot.readers().is_some_and(|readers| readers > 0);
            let unread = self
                .consumers
                .iter()
                .any(|model| model.consumer.cursor() <= snapshot.seq);
            pinned || unread
        })
    }

    fn receive(&mut self, index: usize, hold: bool) -> Result<(), TestCaseError> {
        if self.consumers.is_empty() {
            return Ok(());
        }
        let index = index % self.consumers.len();
        let policy = self.policy;
        let model = &mut self.consumers[index];
        match model.consumer.try_next() {
            Ok(sample) => {
                prop_assert!(
                    sample.seq() > model.last_delivered,
                    "delivered sequences must strictly increase"
                );
                verify_stamp(sample.payload(), sample.seq())?;
                prop_assert_eq!(
                    sample.payload_address() % 128,
                    0,
                    "payload bases stay 128-byte aligned"
                );
                model.last_delivered = sample.seq();
                model.received += 1;
                if hold {
                    model.held.push(sample);
                }
                Ok(())
            }
            Err(RecvError::Lagged(missed)) => {
                prop_assert!(
                    policy.may_overwrite(),
                    "a Block-policy ring must never report lag: the producer may \
                     not recycle a slot any cursor has yet to pass"
                );
                prop_assert!(missed > 0, "a lag report must name a nonzero gap");
                model.lagged += missed;
                Ok(())
            }
            Err(RecvError::Empty) => Ok(()),
            Err(RecvError::Closed) => {
                prop_assert!(self.closed, "only a closed segment reports Closed");
                Ok(())
            }
            Err(other) => Err(TestCaseError::fail(format!(
                "unexpected receive error: {other}"
            ))),
        }
    }

    fn release_held(&mut self, index: usize) {
        if self.consumers.is_empty() {
            return;
        }
        let index = index % self.consumers.len();
        let model = &mut self.consumers[index];
        if !model.held.is_empty() {
            model.held.remove(0);
        }
    }

    fn attach(&mut self) -> Result<(), TestCaseError> {
        match Consumer::attach(
            Arc::clone(&self.segment),
            AttachOptions::default().with_doorbell(false),
        ) {
            Ok(consumer) => {
                let start_cursor = consumer.cursor();
                self.consumers.push(ModelConsumer {
                    consumer,
                    start_cursor,
                    received: 0,
                    lagged: 0,
                    last_delivered: start_cursor.saturating_sub(1),
                    held: Vec::new(),
                });
                Ok(())
            }
            Err(ShmError::ConsumerTableFull { .. } | ShmError::Closed { .. }) => Ok(()),
            Err(other) => Err(TestCaseError::fail(format!(
                "unexpected attach error: {other}"
            ))),
        }
    }

    fn detach(&mut self, index: usize) {
        if self.consumers.is_empty() {
            return;
        }
        let index = index % self.consumers.len();
        self.consumers.remove(index);
    }

    fn check_invariants(&mut self) -> Result<(), TestCaseError> {
        // (1)(2) per-slot state/gate consistency, and (3) sequence
        // monotonicity per slot.
        for index in 0..self.slot_count {
            let snapshot = self.segment.slot(index).snapshot();
            prop_assert_eq!(
                snapshot.violation(),
                None,
                "slot {} violated the state/gate invariant: {:?}",
                index,
                snapshot
            );
            if !matches!(snapshot.state, SlotState::Free) || snapshot.seq != 0 {
                let high = self.slot_high_water.entry(index).or_insert(0);
                prop_assert!(
                    snapshot.seq >= *high,
                    "slot {} went backwards: {} after {}",
                    index,
                    snapshot.seq,
                    *high
                );
                *high = snapshot.seq;
            }
            // A FREE or WRITING slot must present the sentinel gate.
            if !snapshot.state.is_readable() {
                prop_assert_eq!(
                    snapshot.refcount,
                    RECLAIM_SENTINEL,
                    "slot {} is {} with an open gate",
                    index,
                    snapshot.state.as_str()
                );
            }
        }

        // (4) header watermarks are monotone and ordered.
        let write_seq = self.segment.header().write_seq();
        let reclaimed = self.segment.header().reclaimed_seq();
        prop_assert!(write_seq >= self.last_write_seq, "write_seq went backwards");
        prop_assert!(
            reclaimed >= self.last_reclaimed_seq,
            "reclaimed_seq went backwards"
        );
        prop_assert!(
            reclaimed <= write_seq,
            "reclaimed_seq {reclaimed} overtook write_seq {write_seq}"
        );
        self.last_write_seq = write_seq;
        self.last_reclaimed_seq = reclaimed;
        prop_assert_eq!(write_seq, self.published, "write_seq must track commits");

        // (5) every held sample keeps its slot pinned and READY.
        for model in &self.consumers {
            for sample in &model.held {
                let snapshot = self.segment.slot(sample.slot_index()).snapshot();
                prop_assert_eq!(
                    snapshot.state,
                    SlotState::Ready,
                    "a held sample's slot is {}",
                    snapshot.state.as_str()
                );
                prop_assert!(
                    snapshot.readers().is_some_and(|readers| readers > 0),
                    "a held sample's slot is not pinned"
                );
                prop_assert_eq!(
                    snapshot.seq,
                    sample.seq(),
                    "a held sample's slot was reused underneath it"
                );
                verify_stamp(sample.payload(), sample.seq())?;
            }
        }

        // (6) the accounting identity, per consumer.
        for (index, model) in self.consumers.iter().enumerate() {
            let cursor = model.consumer.cursor();
            prop_assert_eq!(
                model.received + model.lagged,
                cursor - model.start_cursor,
                "consumer {} lost track: received {} + lagged {} != cursor {} - start {}",
                index,
                model.received,
                model.lagged,
                cursor,
                model.start_cursor
            );
            let stats = model.consumer.stats();
            prop_assert_eq!(stats.received, model.received);
            prop_assert_eq!(stats.lagged, model.lagged);
        }

        Ok(())
    }
}

fn stamp(buffer: &mut [u8], seq: u64) {
    let bytes = seq.to_le_bytes();
    for (index, byte) in buffer.iter_mut().enumerate() {
        *byte = bytes[index % bytes.len()];
    }
}

fn verify_stamp(payload: &[u8], seq: u64) -> Result<(), TestCaseError> {
    let bytes = seq.to_le_bytes();
    for (index, byte) in payload.iter().enumerate() {
        prop_assert_eq!(
            *byte,
            bytes[index % bytes.len()],
            "payload for sequence {} is corrupt at byte {}",
            seq,
            index
        );
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 96,
        max_shrink_iters: 4096,
        ..ProptestConfig::default()
    })]

    /// Arbitrary operation interleavings against a `Block`-policy ring.
    ///
    /// The strongest of the two: nothing may ever be lost, so a
    /// [`RecvError::Lagged`] anywhere is a failure and the accounting
    /// identity reduces to "the cursor only ever advances by deliveries".
    #[test]
    fn block_policy_never_loses_a_message(
        slots in 1u32..=6,
        payload in 64u32..=1024,
        ops in proptest::collection::vec(op_strategy(), 1..220),
    ) {
        let mut model = Model::new(slots, payload, OverflowPolicy::Block);
        for op in ops {
            model.apply(op)?;
        }
        for consumer in &model.consumers {
            prop_assert_eq!(consumer.lagged, 0, "Block policy must not lose messages");
        }
    }

    /// The same interleavings against an `Overwrite`-policy ring.
    ///
    /// Loss is legal here; the identity `received + lagged == cursor - start`
    /// is what must hold, and it is checked after every single operation.
    #[test]
    fn overwrite_policy_accounts_for_every_message(
        slots in 1u32..=6,
        payload in 64u32..=1024,
        ops in proptest::collection::vec(op_strategy(), 1..220),
    ) {
        let mut model = Model::new(slots, payload, OverflowPolicy::Overwrite);
        for op in ops {
            model.apply(op)?;
        }
    }

    /// A single-slot ring is the tightest possible schedule: every publish
    /// contends with every read for the same slot.
    #[test]
    fn a_single_slot_ring_upholds_every_invariant(
        ops in proptest::collection::vec(op_strategy(), 1..160),
        overwrite in any::<bool>(),
    ) {
        let policy = if overwrite {
            OverflowPolicy::Overwrite
        } else {
            OverflowPolicy::Block
        };
        let mut model = Model::new(1, 128, policy);
        for op in ops {
            model.apply(op)?;
        }
    }

    /// Publishing with no consumers at all must recycle freely and never
    /// report exhaustion — the "nobody is listening" fast path.
    #[test]
    fn an_unobserved_ring_never_exhausts(
        slots in 1u32..=8,
        messages in 1u32..=200,
    ) {
        let mut model = Model::new(slots, 256, OverflowPolicy::Block);
        for _ in 0..messages {
            model.apply(Op::Publish(64))?;
        }
        prop_assert_eq!(model.producer.stats().exhausted, 0);
        prop_assert_eq!(model.producer.stats().published, u64::from(messages));
    }
}
