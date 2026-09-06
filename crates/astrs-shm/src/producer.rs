//! The single producer: zero-copy write windows, commit, and reclamation.
//!
//! # The fast path
//!
//! ```
//! # #[cfg(unix)] {
//! use std::sync::Arc;
//! use astrs_shm::{Producer, Segment, SegmentConfig, SegmentKey};
//! use astrs_wire::DataflowId;
//!
//! let key = SegmentKey::from_parts(DataflowId::generate(), "camera", "image", 1)?;
//! let segment = Segment::create_shared(key, SegmentConfig::new(8, 4096)?)?;
//! let mut producer = Producer::new(Arc::clone(&segment))?;
//!
//! // `allocate` hands back a window pointing straight into the ring: the
//! // caller serialises into it, and no copy ever happens.
//! let mut window = producer.allocate(12)?;
//! window.as_mut_slice().copy_from_slice(b"hello robots");
//! let seq = window.commit(b"")?;
//! assert_eq!(seq, 1);
//! assert_eq!(producer.stats().published, 1);
//! # }
//! # Ok::<(), astrs_shm::ShmError>(())
//! ```
//!
//! # Reclamation, and the exhaustion policy
//!
//! The slot a sequence lands in is fixed: `index = (seq - 1) % slot_count`.
//! Before writing sequence `S` the producer must take back the slot holding
//! `S - slot_count`, which is legal only when
//!
//! - **every occupied consumer cursor has passed it** — under
//!   [`crate::OverflowPolicy::Block`], the reliable default; or
//! - the policy is [`crate::OverflowPolicy::Overwrite`], which drops the
//!   message for whoever had not read it yet; and, under **both** policies,
//! - **no reader holds a pin on it** ([`crate::slot`]'s gate). A pinned slot
//!   is never taken, which is what makes a live [`crate::Sample`] sound even
//!   while the producer is racing ahead.
//!
//! When the slot cannot be taken, [`Producer::allocate`] returns
//! [`ShmError::PoolExhausted`] **immediately**. Blueprint §6.2 is explicit
//! about this (it is the lesson of dora's PR-2366): *never sleep-retry*. The
//! caller falls back to the reliable daemon path and the segment's
//! `shm_fallback_total` counter is incremented so the condition is visible in
//! telemetry rather than as latency.
//!
//! # Crashed consumers cannot wedge the ring
//!
//! A consumer that dies leaves an ACTIVE table entry with a frozen cursor,
//! which under `Block` would stop reclamation forever. Before reporting
//! exhaustion the producer runs an eviction pass over the consumer table and
//! retries once. Eviction requires both a stale heartbeat *and* a pid that no
//! longer exists — see [`crate::consumer_table`].

use std::sync::Arc;

use crate::config::OverflowPolicy;
use crate::consumer_table::CONSUMER_ACTIVE;
use crate::doorbell::DoorbellFanout;
use crate::error::{ShmError, ShmResult};
use crate::layout::SegmentLayout;
use crate::os;
use crate::segment::Segment;
use crate::slot::SlotState;
use crate::stats::ProducerStats;

/// How many consumer-table entries the producer caches between epoch bumps.
const ACTIVE_CACHE_HINT: usize = 8;

/// The single writer of one ring.
///
/// Not `Sync`: the ring is single-producer by construction, and a second
/// concurrent writer would corrupt the sequence numbering. It *is* `Send`, so
/// a node may hand its producer to a worker thread.
#[derive(Debug)]
pub struct Producer {
    segment: Arc<Segment>,
    layout: SegmentLayout,
    policy: OverflowPolicy,
    next_seq: u64,
    stats: ProducerStats,
    fanout: DoorbellFanout,
    active: Vec<u32>,
    active_epoch: u64,
    active_valid: bool,
    stale_after_ns: u64,
}

impl Producer {
    /// Take the producer role for a segment.
    ///
    /// # Errors
    ///
    /// [`ShmError::InvalidConfig`] if this mapping already has a producer, or
    /// [`ShmError::Closed`] if the segment is already closed.
    pub fn new(segment: Arc<Segment>) -> ShmResult<Self> {
        segment.claim_producer()?;
        if segment.header().is_closed() {
            segment.release_producer();
            return Err(ShmError::Closed {
                name: segment_label(&segment),
            });
        }
        let layout = *segment.layout();
        let policy = segment.header().overflow_policy();
        let next_seq = segment.header().write_seq_relaxed() + 1;
        let stale_after_ns = u64::try_from(crate::config::DEFAULT_CONSUMER_STALE_AFTER.as_nanos())
            .unwrap_or(u64::MAX);
        Ok(Self {
            segment,
            layout,
            policy,
            next_seq,
            stats: ProducerStats::default(),
            fanout: DoorbellFanout::new(),
            active: Vec::with_capacity(ACTIVE_CACHE_HINT),
            active_epoch: 0,
            active_valid: false,
            stale_after_ns,
        })
    }

    /// The segment this producer writes into.
    #[must_use]
    pub fn segment(&self) -> &Arc<Segment> {
        &self.segment
    }

    /// The ring's slot count.
    #[must_use]
    pub const fn slot_count(&self) -> u32 {
        self.layout.slot_count()
    }

    /// The largest payload a single message may carry.
    #[must_use]
    pub const fn payload_capacity(&self) -> usize {
        self.layout.payload_capacity() as usize
    }

    /// The largest metadata blob a single message may carry.
    #[must_use]
    pub const fn meta_capacity(&self) -> usize {
        self.layout.meta_capacity() as usize
    }

    /// The overflow policy in force.
    #[must_use]
    pub const fn overflow_policy(&self) -> OverflowPolicy {
        self.policy
    }

    /// The sequence number the next commit will publish.
    #[must_use]
    pub const fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// The producer's counters.
    #[must_use]
    pub const fn stats(&self) -> &ProducerStats {
        &self.stats
    }

    /// Override how long a consumer heartbeat may go unrefreshed before its
    /// entry becomes evictable.
    pub fn set_consumer_stale_after(&mut self, stale_after: std::time::Duration) {
        self.stale_after_ns = u64::try_from(stale_after.as_nanos()).unwrap_or(u64::MAX);
    }

    /// Open a zero-copy write window of `len` bytes.
    ///
    /// # Errors
    ///
    /// - [`ShmError::Closed`] — the segment was closed.
    /// - [`ShmError::PayloadTooLarge`] — `len` exceeds
    ///   [`Producer::payload_capacity`].
    /// - [`ShmError::PoolExhausted`] — no slot could be reclaimed. **Do not
    ///   retry in a loop**; fall back to the daemon path (§6.2).
    pub fn allocate(&mut self, len: usize) -> ShmResult<SampleMut<'_>> {
        self.try_allocate(len)
    }

    /// The explicit spelling of [`Producer::allocate`].
    ///
    /// Both are non-blocking and both refuse rather than wait; the pair
    /// exists because "try_" reads better at a call site that has a fallback
    /// ready, and the blueprint names the exhaustion policy against
    /// `try_allocate`.
    ///
    /// # Errors
    ///
    /// As [`Producer::allocate`].
    pub fn try_allocate(&mut self, len: usize) -> ShmResult<SampleMut<'_>> {
        if self.segment.header().is_closed() {
            return Err(ShmError::Closed {
                name: segment_label(&self.segment),
            });
        }
        if len > self.payload_capacity() {
            return Err(ShmError::PayloadTooLarge {
                requested: len,
                capacity: self.payload_capacity(),
            });
        }

        let seq = self.next_seq;
        let index = self.layout.slot_index_for_seq(seq);

        if !self.acquire_slot(index, seq) {
            // One eviction pass, then one retry: a crashed consumer must not
            // wedge the ring, but a *live* slow consumer must still produce
            // backpressure rather than silent loss.
            let evicted = self.evict_stale_consumers();
            if evicted == 0 || !self.acquire_slot(index, seq) {
                self.stats.exhausted += 1;
                self.segment.header().record_fallback();
                return Err(ShmError::PoolExhausted {
                    slot_count: self.layout.slot_count(),
                });
            }
        }

        let slot = self.segment.slot(index);
        // SAFETY: `acquire_slot` returned true, so the slot's gate is closed
        // and the producer owns it exclusively.
        unsafe { slot.begin_write() };

        Ok(SampleMut {
            producer: self,
            index,
            seq,
            len,
            finished: false,
        })
    }

    /// Allocate, copy `payload` in, and commit — the §6.2 "copied once into a
    /// slot" path for payloads that are already in a heap buffer.
    ///
    /// # Errors
    ///
    /// As [`Producer::allocate`], plus [`ShmError::MetadataTooLarge`].
    pub fn send(&mut self, payload: &[u8], meta: &[u8]) -> ShmResult<u64> {
        let mut window = self.try_allocate(payload.len())?;
        window.as_mut_slice().copy_from_slice(payload);
        window.commit(meta)
    }

    /// Commit a write window, publishing it to consumers.
    ///
    /// The associated-function spelling of [`SampleMut::commit`], matching
    /// the blueprint's `commit(SampleMut, meta_bytes)` shape.
    ///
    /// # Errors
    ///
    /// [`ShmError::MetadataTooLarge`] if `meta` exceeds
    /// [`Producer::meta_capacity`]; the window is aborted in that case and
    /// the slot returns to the pool.
    pub fn commit(sample: SampleMut<'_>, meta: &[u8]) -> ShmResult<u64> {
        sample.commit(meta)
    }

    /// Mark the segment closed: no further allocations succeed, and
    /// consumers drain and then observe [`crate::RecvError::Closed`].
    pub fn close(&mut self) {
        self.segment.mark_closed();
        self.ring_doorbells();
    }

    /// Refresh the producer heartbeat word without publishing anything.
    ///
    /// A producer that is alive but idle (waiting on a sensor) keeps this
    /// moving so a supervisor can distinguish "quiet" from "wedged".
    pub fn heartbeat(&self) {
        self.segment.header().touch_producer(crate::now_ns());
    }

    /// Ring every registered consumer doorbell.
    ///
    /// Called automatically on commit and on close; exposed for a producer
    /// that wants to wake readers for its own reasons (a shutdown barrier).
    pub fn ring_doorbells(&mut self) -> usize {
        let rung = self.fanout.ring_all(self.segment.doorbells());
        self.stats.doorbell_rings += rung as u64;
        rung
    }

    /// Try to take exclusive ownership of the slot sequence `seq` will use.
    ///
    /// Returns `false` when the slot is unreclaimable — either a consumer
    /// still needs it (under `Block`) or a reader has it pinned (under both
    /// policies).
    fn acquire_slot(&mut self, index: u32, seq: u64) -> bool {
        // Deliberately re-borrows `self.segment` at each step rather than
        // holding a `&SlotHeader` across `all_cursors_passed`, which needs
        // `&mut self` to refresh its consumer cache.
        match self.segment.slot(index).state() {
            // Never written, or already reclaimed: the gate is closed and the
            // slot is ours.
            SlotState::Free => true,
            // A previous `allocate` opened a window that was neither
            // committed nor dropped. `SampleMut`'s `Drop` makes this
            // unreachable; treating it as available would risk two live
            // windows on one slot, so refuse instead.
            SlotState::Writing => false,
            SlotState::Ready => {
                let resident = self.segment.slot(index).seq();
                let passed = self.all_cursors_passed(resident);
                if !passed && !self.policy.may_overwrite() {
                    return false;
                }
                let slot = self.segment.slot(index);
                if !slot.try_close_gate() {
                    // A reader holds it. Under either policy this is a hard
                    // stop: zero-copy soundness outranks the overflow policy.
                    return false;
                }
                if !passed {
                    slot.record_overwrite();
                    self.stats.overwritten += 1;
                }
                // SAFETY: the gate close transferred ownership to us.
                unsafe { slot.set_free() };
                // Everything up to and including `resident` has left the
                // ring. Published before the write begins so a consumer that
                // observes the slot changing can immediately compute its lag.
                self.segment.header().set_reclaimed_seq(resident);
                debug_assert!(seq > resident, "ring order must be monotone");
                true
            }
        }
    }

    /// Whether every occupied consumer cursor is strictly past `seq`.
    ///
    /// A cursor of `0` means "a claim is in flight, the real value is not yet
    /// published" and is treated as *not* passed — the conservative
    /// direction, since guessing high would let the producer drop a message
    /// an attaching consumer was entitled to.
    fn all_cursors_passed(&mut self, seq: u64) -> bool {
        self.refresh_active();
        for index in &self.active {
            let entry = self.segment.consumer_entry(*index);
            if !entry.is_occupied() {
                continue;
            }
            let cursor = entry.cursor();
            if cursor == 0 || cursor <= seq {
                return false;
            }
        }
        true
    }

    /// Rebuild the cached list of occupied consumer entries when the
    /// segment's consumer epoch has moved.
    fn refresh_active(&mut self) {
        let epoch = self.segment.header().consumer_epoch();
        if self.active_valid && epoch == self.active_epoch {
            return;
        }
        self.active.clear();
        for (index, entry) in self.segment.consumer_entries() {
            if entry.is_occupied() {
                self.active.push(index);
            }
        }
        self.active_epoch = epoch;
        self.active_valid = true;
    }

    /// Evict consumer entries whose owning process is gone.
    ///
    /// Returns how many entries were freed.
    fn evict_stale_consumers(&mut self) -> u32 {
        let now = crate::now_ns();
        let mut evicted = 0;
        for (index, entry) in self.segment.consumer_entries() {
            if entry.state() != CONSUMER_ACTIVE {
                continue;
            }
            let heartbeat = entry.heartbeat_ns();
            if now.saturating_sub(heartbeat) < self.stale_after_ns {
                continue;
            }
            let pid = entry.pid();
            if pid != 0 && os::process_alive(pid) {
                continue;
            }
            if entry.try_evict() {
                self.segment.doorbells().unregister(index);
                evicted += 1;
            }
        }
        if evicted > 0 {
            self.stats.evicted_consumers += u64::from(evicted);
            self.active_valid = false;
        }
        evicted
    }
}

impl Drop for Producer {
    fn drop(&mut self) {
        self.segment.release_producer();
    }
}

/// A zero-copy write window into one slot.
///
/// Borrows the producer for its lifetime, so the compiler — not a runtime
/// flag — enforces that at most one window is open at a time.
///
/// Dropping an uncommitted window returns the slot to the pool; the sequence
/// number is *not* consumed, so an aborted write leaves no gap for consumers
/// to wait on.
#[derive(Debug)]
pub struct SampleMut<'a> {
    producer: &'a mut Producer,
    index: u32,
    seq: u64,
    len: usize,
    finished: bool,
}

impl SampleMut<'_> {
    /// The sequence number this window will publish, if committed.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// The ring slot this window occupies.
    #[must_use]
    pub const fn slot_index(&self) -> u32 {
        self.index
    }

    /// The window's length in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the window is zero bytes long.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The writable payload bytes.
    ///
    /// The slice base is 128-byte aligned (blueprint §6.1), so an Arrow IPC
    /// message serialised straight into it is directly usable as a SIMD
    /// source by every consumer.
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: the slot's gate is closed and its state is WRITING, so no
        // consumer can pin it; the length was checked against the slot's
        // payload capacity in `try_allocate`; the mapping outlives `self`
        // because `SampleMut` borrows the producer, which holds the `Arc`.
        unsafe {
            std::slice::from_raw_parts_mut(self.producer.segment.payload_ptr(self.index), self.len)
        }
    }

    /// The payload bytes, read back.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: as `as_mut_slice`.
        unsafe {
            std::slice::from_raw_parts(self.producer.segment.payload_ptr(self.index), self.len)
        }
    }

    /// Shrink the window before committing.
    ///
    /// A serialiser that reserved an upper bound and wrote less trims here
    /// rather than committing padding.
    ///
    /// # Errors
    ///
    /// [`ShmError::PayloadTooLarge`] if `len` is larger than the window that
    /// was allocated — growing would reach past what was reserved.
    pub fn truncate(&mut self, len: usize) -> ShmResult<()> {
        if len > self.len {
            return Err(ShmError::PayloadTooLarge {
                requested: len,
                capacity: self.len,
            });
        }
        self.len = len;
        Ok(())
    }

    /// Publish the window, with `meta` as its metadata blob.
    ///
    /// Returns the sequence number published.
    ///
    /// # Errors
    ///
    /// [`ShmError::MetadataTooLarge`] if `meta` does not fit the slot's
    /// metadata region; the window is aborted and the slot returns to the
    /// pool.
    pub fn commit(mut self, meta: &[u8]) -> ShmResult<u64> {
        let capacity = self.producer.meta_capacity();
        if meta.len() > capacity {
            self.abort_in_place();
            return Err(ShmError::MetadataTooLarge {
                requested: meta.len(),
                capacity,
            });
        }
        if !meta.is_empty() {
            // SAFETY: exclusive ownership of the slot; the length was just
            // checked against the metadata region's capacity.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    meta.as_ptr(),
                    self.producer.segment.meta_ptr(self.index),
                    meta.len(),
                );
            }
        }

        let seq = self.seq;
        let len = u32::try_from(self.len).unwrap_or(u32::MAX);
        let meta_len = u32::try_from(meta.len()).unwrap_or(u32::MAX);
        let now = crate::now_ns();
        let slot = self.producer.segment.slot(self.index);
        // SAFETY: exclusive ownership; the payload and metadata bytes are
        // already written. `publish` performs the release fence.
        unsafe { slot.publish(seq, len, meta_len, now) };

        let header = self.producer.segment.header();
        // Release, after the slot's own release: a consumer that observes
        // `write_seq >= seq` is guaranteed to be able to pin the slot.
        header.publish_seq(seq);
        header.touch_producer(now);

        self.producer.next_seq = seq + 1;
        self.producer.stats.published += 1;
        self.producer.stats.payload_bytes += self.len as u64;
        self.producer.stats.meta_bytes += meta.len() as u64;
        self.finished = true;
        self.producer.ring_doorbells();
        Ok(seq)
    }

    /// Abandon the window without publishing.
    ///
    /// The slot returns to the pool and the sequence number is reused by the
    /// next allocation.
    pub fn abort(mut self) {
        self.abort_in_place();
    }

    fn abort_in_place(&mut self) {
        if self.finished {
            return;
        }
        let slot = self.producer.segment.slot(self.index);
        // SAFETY: the producer owns the slot exclusively for as long as this
        // window is alive.
        unsafe { slot.abort_write() };
        self.producer.stats.aborted += 1;
        self.finished = true;
    }
}

impl Drop for SampleMut<'_> {
    fn drop(&mut self) {
        self.abort_in_place();
    }
}

fn segment_label(segment: &Segment) -> String {
    segment
        .name()
        .map(|name| name.as_str().to_owned())
        .or_else(|| segment.key().map(crate::SegmentKey::canonical))
        .unwrap_or_else(|| "<anonymous>".to_owned())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::SegmentConfig;
    use crate::key::SegmentKey;
    use astrs_wire::DataflowId;

    fn segment(slots: u32, payload: u32, policy: OverflowPolicy) -> Arc<Segment> {
        let key = SegmentKey::from_parts(DataflowId::generate(), "producer", "out", 1).unwrap();
        let config = SegmentConfig::new(slots, payload)
            .unwrap()
            .with_overflow(policy);
        Segment::create_shared(key, config).unwrap()
    }

    #[test]
    fn commit_publishes_sequences_starting_at_one() {
        let segment = segment(4, 256, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        assert_eq!(producer.next_seq(), 1);

        for expected in 1..=4u64 {
            let mut window = producer.allocate(8).unwrap();
            assert_eq!(window.seq(), expected);
            window
                .as_mut_slice()
                .copy_from_slice(&expected.to_le_bytes());
            assert_eq!(window.commit(b"m").unwrap(), expected);
        }
        assert_eq!(segment.header().write_seq(), 4);
        assert_eq!(producer.stats().published, 4);
        assert_eq!(producer.stats().payload_bytes, 32);
        assert_eq!(producer.stats().meta_bytes, 4);

        for index in 0..4u32 {
            let slot = segment.slot(index);
            assert_eq!(slot.state(), SlotState::Ready);
            assert_eq!(slot.seq(), u64::from(index) + 1);
            assert_eq!(slot.len(), 8);
            assert_eq!(slot.meta_len(), 1);
            assert_eq!(slot.snapshot().violation(), None);
        }
    }

    #[test]
    fn a_full_ring_with_no_consumers_recycles_freely() {
        let segment = segment(2, 128, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        for _ in 0..100 {
            producer.send(b"x", b"").unwrap();
        }
        assert_eq!(producer.stats().published, 100);
        assert_eq!(producer.stats().exhausted, 0);
        assert_eq!(segment.header().reclaimed_seq(), 98);
        assert_eq!(segment.view().resident(), 2);
    }

    #[test]
    fn payload_and_metadata_limits_are_enforced() {
        let segment = segment(2, 128, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        assert!(matches!(
            producer.allocate(129),
            Err(ShmError::PayloadTooLarge {
                requested: 129,
                capacity: 128
            })
        ));

        let oversized = vec![0u8; producer.meta_capacity() + 1];
        let window = producer.allocate(4).unwrap();
        assert!(matches!(
            window.commit(&oversized),
            Err(ShmError::MetadataTooLarge { .. })
        ));
        // The failed commit must have returned the slot, not leaked it.
        assert_eq!(segment.slot(0).state(), SlotState::Free);
        assert_eq!(producer.next_seq(), 1);
        assert_eq!(producer.stats().aborted, 1);
    }

    #[test]
    fn an_aborted_window_reuses_its_sequence_number() {
        let segment = segment(4, 128, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        let window = producer.allocate(4).unwrap();
        assert_eq!(window.seq(), 1);
        window.abort();
        assert_eq!(producer.next_seq(), 1);
        assert_eq!(segment.slot(0).state(), SlotState::Free);

        // Dropping without committing does the same.
        {
            let _dropped = producer.allocate(4).unwrap();
        }
        assert_eq!(producer.next_seq(), 1);
        assert_eq!(producer.stats().aborted, 2);
        assert_eq!(producer.send(b"ok", b"").unwrap(), 1);
    }

    #[test]
    fn truncation_shrinks_but_never_grows_a_window() {
        let segment = segment(2, 256, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        let mut window = producer.allocate(64).unwrap();
        assert_eq!(window.len(), 64);
        assert!(!window.is_empty());
        window.as_mut_slice().fill(0xcd);
        window.truncate(10).unwrap();
        assert_eq!(window.len(), 10);
        assert!(window.truncate(11).is_err());
        assert_eq!(window.as_slice().len(), 10);
        window.commit(b"").unwrap();
        assert_eq!(segment.slot(0).len(), 10);
    }

    #[test]
    fn write_windows_are_128_byte_aligned() {
        let segment = segment(5, 700, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        for _ in 0..10 {
            let mut window = producer.allocate(1).unwrap();
            let address = window.as_mut_slice().as_ptr() as usize;
            assert_eq!(address % 128, 0, "payload bases must be 128-byte aligned");
            window.commit(b"").unwrap();
        }
    }

    #[test]
    fn only_one_producer_per_mapping() {
        let segment = segment(2, 128, OverflowPolicy::Block);
        let first = Producer::new(Arc::clone(&segment)).unwrap();
        assert!(Producer::new(Arc::clone(&segment)).is_err());
        drop(first);
        // The role is released on drop.
        Producer::new(Arc::clone(&segment)).unwrap();
    }

    #[test]
    fn a_closed_segment_refuses_producers_and_allocations() {
        let segment = segment(2, 128, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        producer.send(b"a", b"").unwrap();
        producer.close();
        assert!(matches!(producer.allocate(1), Err(ShmError::Closed { .. })));
        drop(producer);
        assert!(matches!(
            Producer::new(Arc::clone(&segment)),
            Err(ShmError::Closed { .. })
        ));
    }

    #[test]
    fn a_pinned_slot_is_never_overwritten_even_under_the_overwrite_policy() {
        let segment = segment(2, 128, OverflowPolicy::Overwrite);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        producer.send(b"one", b"").unwrap();
        producer.send(b"two", b"").unwrap();

        // Pin slot 0 (sequence 1) as a reader would.
        let facts = segment.slot(0).pin().unwrap();
        assert_eq!(facts.seq, 1);

        // Sequence 3 wants slot 0 and cannot have it.
        assert!(matches!(
            producer.allocate(1),
            Err(ShmError::PoolExhausted { slot_count: 2 })
        ));
        assert_eq!(segment.header().fallback_total(), 1);

        // SAFETY: matched with the pin above.
        unsafe { segment.slot(0).unpin() };
        assert_eq!(producer.send(b"three", b"").unwrap(), 3);
        assert_eq!(producer.stats().published, 3);
        // No consumer was registered, so nothing was actually lost: the
        // overwrite counter tracks *readers* left behind, not slot reuse.
        assert_eq!(producer.stats().overwritten, 0);
    }

    #[test]
    fn a_consumer_mid_claim_blocks_reclamation_until_it_publishes_a_cursor() {
        {
            let ring = segment(2, 128, OverflowPolicy::Block);
            let mut producer = Producer::new(Arc::clone(&ring)).unwrap();
            producer.send(b"one", b"").unwrap();
            producer.send(b"two", b"").unwrap();

            // A consumer is mid-attach: the entry is taken, but the cursor it
            // is entitled to has not been published yet. `activate` stores
            // `cursor.max(1)`, so a zero cursor on an occupied entry means
            // exactly this window and nothing else.
            let entry = ring.consumer_entry(0);
            assert!(entry.claim_in_flight().is_some());
            assert_eq!(entry.cursor(), 0);
            assert!(entry.is_occupied());
            ring.header().bump_consumer_epoch();

            // Guessing "passed" here would let the producer drop sequence 1
            // before the attaching consumer ever got to read it, which under
            // `Block` is silent loss rather than a reportable lag.
            assert!(matches!(
                producer.allocate(1),
                Err(ShmError::PoolExhausted { slot_count: 2 })
            ));
        }

        // Positive control: the same ring, but with the cursor actually
        // published past sequence 1, reclaims slot 0 without complaint. This
        // is what proves the assertion above pins on the *unpublished
        // cursor* and not merely on the entry being occupied.
        let other = segment(2, 128, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&other)).unwrap();
        producer.send(b"one", b"").unwrap();
        producer.send(b"two", b"").unwrap();
        let registration = crate::consumer_table::claim_entry(
            other.consumer_entries(),
            crate::os::current_pid(),
            2,
            crate::now_ns(),
            false,
        )
        .unwrap();
        other.header().consumer_attached();
        assert_eq!(
            other.consumer_entry(registration.index()).cursor(),
            2,
            "an activated entry never carries a zero cursor"
        );
        assert_eq!(producer.send(b"three", b"").unwrap(), 3);
    }

    #[test]
    fn the_producer_heartbeat_advances() {
        let segment = segment(2, 128, OverflowPolicy::Block);
        let producer = Producer::new(Arc::clone(&segment)).unwrap();
        let before = segment.header().producer_heartbeat();
        std::thread::sleep(std::time::Duration::from_millis(2));
        producer.heartbeat();
        assert!(segment.header().producer_heartbeat() > before);
    }

    #[test]
    fn commit_rings_registered_doorbells() {
        let segment = segment(4, 128, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        let doorbell = crate::Doorbell::new().unwrap();
        segment
            .doorbells()
            .register(0, 1, doorbell.ringer().unwrap());
        producer.send(b"ding", b"").unwrap();
        assert!(
            doorbell
                .wait(Some(std::time::Duration::from_millis(500)))
                .unwrap()
        );
        assert!(producer.stats().doorbell_rings >= 1);
    }

    #[test]
    fn the_associated_commit_spelling_matches_the_method() {
        let segment = segment(2, 128, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        let mut window = producer.allocate(3).unwrap();
        window.as_mut_slice().copy_from_slice(b"abc");
        assert_eq!(Producer::commit(window, b"meta").unwrap(), 1);
        assert_eq!(segment.slot(0).meta_len(), 4);
    }

    #[test]
    fn capacities_are_reported_from_the_layout() {
        let segment = segment(3, 640, OverflowPolicy::Block);
        let producer = Producer::new(Arc::clone(&segment)).unwrap();
        assert_eq!(producer.slot_count(), 3);
        assert_eq!(producer.payload_capacity(), 640);
        assert_eq!(
            producer.meta_capacity(),
            crate::config::DEFAULT_META_CAPACITY as usize
        );
        assert_eq!(producer.overflow_policy(), OverflowPolicy::Block);
        assert!(Arc::ptr_eq(producer.segment(), &segment));
    }
}
