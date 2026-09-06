//! Consumers: zero-copy read views, cursors, lag reporting and blocking
//! receives.
//!
//! # Reading without copying
//!
//! ```
//! # #[cfg(unix)] {
//! use std::sync::Arc;
//! use astrs_shm::{AttachOptions, Consumer, Producer, Segment, SegmentConfig, SegmentKey};
//! use astrs_wire::DataflowId;
//!
//! let key = SegmentKey::from_parts(DataflowId::generate(), "camera", "image", 1)?;
//! let segment = Segment::create_shared(key, SegmentConfig::new(8, 4096)?)?;
//! let mut producer = Producer::new(Arc::clone(&segment))?;
//! let mut consumer = Consumer::attach(Arc::clone(&segment), AttachOptions::default())?;
//!
//! producer.send(b"frame-0", b"meta")?;
//! let sample = consumer.try_next()?;
//! assert_eq!(sample.payload(), b"frame-0");
//! assert_eq!(sample.metadata(), b"meta");
//! assert_eq!(sample.seq(), 1);
//! # }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # The receive algorithm
//!
//! Three checks, in this order, and the order is load-bearing:
//!
//! 1. **Lag first.** If the cursor is at or below the segment's
//!    `reclaimed_seq` watermark, the messages it wanted are gone. Report
//!    [`RecvError::Lagged`] with an exact count and jump the cursor to the
//!    oldest surviving sequence. Checking this before availability means a
//!    lagging consumer never pins a slot it has no right to.
//! 2. **Availability.** `cursor > write_seq` means nothing new — or, if the
//!    segment is closed, that the drain is complete.
//! 3. **Pin, then verify.** Pin the slot the cursor names and check that it
//!    really carries that sequence. A mismatch means the producer recycled
//!    the slot between steps 1 and 3; the loop re-runs and step 1 then
//!    reports the lag. The retry budget is small and bounded — every
//!    iteration observes a strictly newer watermark, so it terminates, and
//!    exhausting it simply yields [`RecvError::Empty`].
//!
//! Advancing the cursor *after* pinning is deliberate. The pin stops the
//! producer from reclaiming the slot; the cursor tells it that this consumer
//! no longer needs it. Both conditions must hold before a slot returns to the
//! pool, which is precisely the drop-token protocol of §6.2.
//!
//! # Blocking without spinning
//!
//! [`Consumer::recv`] and [`Consumer::next_blocking`] park on the consumer's
//! own doorbell descriptor ([`crate::Doorbell`]), which the producer rings on
//! every commit. The wait is sliced with a growing timeout so that a route
//! whose doorbell has not been wired yet — the window before the daemon
//! delivers the descriptor to the producer's process — still makes progress
//! with bounded latency instead of hanging. That fallback is the *only*
//! polling in the crate, and it never applies to a fully wired route.
//!
//! # Detecting a dead producer
//!
//! A consumer must not wait forever because the producer was `kill -9`'d and
//! the daemon died too, leaving nobody to set the `closed` flag. Whenever a
//! blocking wait goes a full slice without data, the consumer checks whether
//! the pid in the segment header is still alive and, if not, reports
//! [`ShmError::ProducerGone`] after draining whatever is left.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::attach::AttachOptions;
use crate::backoff::Backoff;
use crate::consumer_table::{ConsumerEntry, ConsumerRegistration, claim_entry};
use crate::doorbell::Doorbell;
use crate::error::{RecvError, ShmError, ShmResult};
use crate::layout::SegmentLayout;
use crate::os;
use crate::segment::Segment;
use crate::slot::PinFailure;
use crate::stats::ConsumerStats;

/// How many times a receive re-runs its checks after losing a race with the
/// producer before giving up and reporting [`RecvError::Empty`].
const RECV_RETRY_BUDGET: u32 = 64;

/// The first doorbell wait slice.
const MIN_WAIT_SLICE: Duration = Duration::from_micros(200);

/// The longest doorbell wait slice — the worst-case added latency for a route
/// whose doorbell has not been wired to the producer's process yet.
const MAX_WAIT_SLICE: Duration = Duration::from_millis(20);

/// How many receive attempts pass between heartbeat refreshes.
const HEARTBEAT_EVERY: u32 = 64;

/// One attached reader of a ring.
#[derive(Debug)]
pub struct Consumer {
    segment: Arc<Segment>,
    layout: SegmentLayout,
    registration: ConsumerRegistration,
    cursor: u64,
    doorbell: Option<Doorbell>,
    stats: ConsumerStats,
    detached: bool,
    /// Counts receive attempts so the heartbeat is refreshed periodically
    /// rather than on every poll — a tight `try_next` loop must not become a
    /// clock-read loop.
    attempts: u32,
}

impl Consumer {
    /// Attach to a segment.
    ///
    /// # Errors
    ///
    /// - [`ShmError::Closed`] — the segment is closed and no longer accepts
    ///   readers.
    /// - [`ShmError::ConsumerTableFull`] — the fan-out exceeds
    ///   [`crate::SegmentConfig::max_consumers`].
    /// - [`ShmError::StaleGeneration`] / [`ShmError::KeyMismatch`] — when
    ///   [`AttachOptions::expecting`] was used.
    /// - [`ShmError::Os`] — the doorbell descriptor could not be created.
    pub fn attach(segment: Arc<Segment>, options: AttachOptions) -> ShmResult<Self> {
        if let Some(expected) = options.expected() {
            segment.verify_identity(expected)?;
        }
        let header = segment.header();
        if header.is_closed() {
            return Err(ShmError::Closed {
                name: segment
                    .name()
                    .map(|name| name.as_str().to_owned())
                    .unwrap_or_else(|| "<anonymous>".to_owned()),
            });
        }

        let layout = *segment.layout();
        let cursor = options
            .start()
            .resolve(header.reclaimed_seq(), header.write_seq());

        let doorbell = if options.doorbell() {
            Some(Doorbell::new()?)
        } else {
            None
        };

        let now = crate::now_ns();
        let registration = claim_entry(
            segment.consumer_entries(),
            os::current_pid(),
            cursor,
            now,
            doorbell.is_some(),
        )
        .ok_or(ShmError::ConsumerTableFull {
            max_consumers: layout.max_consumers(),
        })?;
        header.consumer_attached();

        if let Some(doorbell) = &doorbell {
            // In-process wiring: the producer sharing this mapping picks the
            // ringer up on its next commit. A cross-process producer is wired
            // by the daemon, which sends this same descriptor over
            // `SCM_RIGHTS`.
            segment.doorbells().register(
                registration.index(),
                registration.token(),
                doorbell.ringer()?,
            );
        }

        let mut consumer = Self {
            segment,
            layout,
            registration,
            cursor,
            doorbell,
            stats: ConsumerStats::default(),
            detached: false,
            attempts: 0,
        };
        // The producer may have reclaimed past our chosen start between the
        // read above and the claim; re-clamp so the first receive reports a
        // clean lag instead of pinning a recycled slot.
        consumer.clamp_to_oldest();
        Ok(consumer)
    }

    /// The segment being read.
    #[must_use]
    pub fn segment(&self) -> &Arc<Segment> {
        &self.segment
    }

    /// The consumer-table index this reader occupies.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.registration.index()
    }

    /// The drop token proving ownership of the table entry.
    #[must_use]
    pub const fn token(&self) -> u32 {
        self.registration.token()
    }

    /// The next sequence number this consumer wants.
    #[must_use]
    pub const fn cursor(&self) -> u64 {
        self.cursor
    }

    /// The consumer's counters.
    #[must_use]
    pub const fn stats(&self) -> &ConsumerStats {
        &self.stats
    }

    /// The doorbell, when one was created.
    ///
    /// The daemon takes [`Doorbell::ringer`] from here and ships it to the
    /// producer's process.
    #[must_use]
    pub const fn doorbell(&self) -> Option<&Doorbell> {
        self.doorbell.as_ref()
    }

    /// How far behind the producer this consumer is, in messages.
    #[must_use]
    pub fn backlog(&self) -> u64 {
        self.segment
            .header()
            .write_seq()
            .saturating_sub(self.cursor.saturating_sub(1))
    }

    /// Refresh the heartbeat word so the producer does not consider this
    /// consumer a candidate for eviction.
    ///
    /// Every receive attempt does this; an event loop that goes quiet for
    /// longer than [`crate::SegmentConfig::consumer_stale_after`] should call
    /// it explicitly. (Eviction also requires the pid to be dead, so a live
    /// but silent consumer is safe either way — this simply keeps the
    /// diagnostics honest.)
    pub fn heartbeat(&self) {
        self.registration.touch(self.entry(), crate::now_ns());
    }

    /// Take the next message without blocking.
    ///
    /// # Errors
    ///
    /// - [`RecvError::Empty`] — nothing new.
    /// - [`RecvError::Lagged`] — messages were overwritten; the count is
    ///   exact and the cursor has been advanced to the oldest survivor.
    /// - [`RecvError::Closed`] — the segment is closed and fully drained.
    pub fn try_next(&mut self) -> Result<Sample, RecvError> {
        self.periodic_heartbeat();
        let header = self.segment.header();

        for _ in 0..RECV_RETRY_BUDGET {
            let reclaimed = header.reclaimed_seq();
            if self.cursor <= reclaimed {
                let missed = reclaimed + 1 - self.cursor;
                self.cursor = reclaimed + 1;
                self.publish_cursor();
                self.stats.lagged += missed;
                self.registration.record_lagged(self.entry(), missed);
                return Err(RecvError::Lagged(missed));
            }

            let write_seq = header.write_seq();
            if self.cursor > write_seq {
                self.stats.empty_polls += 1;
                return if header.is_closed() {
                    Err(RecvError::Closed)
                } else {
                    Err(RecvError::Empty)
                };
            }

            let index = self.layout.slot_index_for_seq(self.cursor);
            let slot = self.segment.slot(index);
            match slot.pin() {
                Ok(facts) if facts.seq == self.cursor => {
                    self.cursor += 1;
                    self.publish_cursor();
                    self.stats.received += 1;
                    self.stats.payload_bytes += u64::from(facts.len);
                    self.registration.record_received(self.entry(), 1);
                    return Ok(Sample {
                        segment: Arc::clone(&self.segment),
                        index,
                        seq: facts.seq,
                        len: facts.len as usize,
                        meta_len: facts.meta_len as usize,
                        commit_ns: facts.commit_ns,
                    });
                }
                Ok(_) => {
                    // The slot was recycled under us. Release and re-run:
                    // the lag check at the top of the loop now has a fresher
                    // watermark and will report the gap.
                    // SAFETY: matched with the successful pin above.
                    unsafe { slot.unpin() };
                    self.stats.race_retries += 1;
                }
                Err(PinFailure::Closed) => {
                    // The producer owns the slot right now — it is either
                    // filling our sequence or reclaiming the previous one.
                    self.stats.race_retries += 1;
                }
                Err(PinFailure::Saturated) => {
                    self.stats.empty_polls += 1;
                    return Err(RecvError::Empty);
                }
            }
            std::hint::spin_loop();
        }

        self.stats.empty_polls += 1;
        Err(RecvError::Empty)
    }

    /// Block until a message arrives, the segment closes, or the producer
    /// dies.
    ///
    /// # Errors
    ///
    /// As [`Consumer::try_next`], plus [`ShmError::ProducerGone`] wrapped in
    /// [`RecvError::Shm`] when the producing process has vanished without the
    /// segment being closed.
    pub fn recv(&mut self) -> Result<Sample, RecvError> {
        self.wait_for_message(None)
    }

    /// Block for at most `timeout`.
    ///
    /// # Errors
    ///
    /// As [`Consumer::recv`], plus [`RecvError::Empty`] on expiry.
    pub fn next_blocking(&mut self, timeout: Duration) -> Result<Sample, RecvError> {
        self.wait_for_message(Some(timeout))
    }

    fn wait_for_message(&mut self, timeout: Option<Duration>) -> Result<Sample, RecvError> {
        let deadline = timeout.map(|budget| Instant::now() + budget);
        let mut slice = MIN_WAIT_SLICE;
        // Only used on the no-doorbell path; see below.
        let mut backoff = Backoff::new();
        loop {
            match self.try_next() {
                Err(RecvError::Empty) => {}
                other => return other,
            }

            let remaining = match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(RecvError::Empty);
                    }
                    Some(deadline - now)
                }
                None => None,
            };
            let wait = remaining.map_or(slice, |left| left.min(slice));

            self.stats.doorbell_waits += 1;
            let rang = match &self.doorbell {
                Some(doorbell) => doorbell.wait(Some(wait)).map_err(RecvError::Shm)?,
                None => {
                    // No doorbell was requested. Walk the bounded spin →
                    // yield → sleep schedule rather than sleeping a fixed
                    // slice: a message that is already one commit away is
                    // then picked up in nanoseconds, and a genuinely quiet
                    // ring still costs nothing.
                    if let Some(sleep) = backoff.step_without_sleeping() {
                        std::thread::sleep(sleep.min(wait));
                    }
                    false
                }
            };
            if rang {
                if let Some(doorbell) = &self.doorbell {
                    doorbell.drain().map_err(RecvError::Shm)?;
                }
                slice = MIN_WAIT_SLICE;
                backoff.reset();
            } else {
                slice = (slice * 2).min(MAX_WAIT_SLICE);
                self.check_producer_liveness()?;
            }
        }
    }

    /// Await the next message on a tokio runtime.
    ///
    /// # Errors
    ///
    /// As [`Consumer::recv`].
    pub async fn recv_async(&mut self) -> Result<Sample, RecvError> {
        loop {
            match self.try_next() {
                Err(RecvError::Empty) => {}
                other => return other,
            }
            match &self.doorbell {
                Some(doorbell) => {
                    // A bounded slice keeps a not-yet-wired route progressing,
                    // exactly as in the blocking path.
                    match tokio::time::timeout(MAX_WAIT_SLICE, doorbell.wait_async()).await {
                        Ok(result) => {
                            result.map_err(RecvError::Shm)?;
                            doorbell.drain().map_err(RecvError::Shm)?;
                        }
                        Err(_) => self.check_producer_liveness()?,
                    }
                }
                None => {
                    tokio::time::sleep(MIN_WAIT_SLICE).await;
                    self.check_producer_liveness()?;
                }
            }
        }
    }

    /// Report [`ShmError::ProducerGone`] if the ring is quiet and the
    /// producing process no longer exists.
    fn check_producer_liveness(&self) -> Result<(), RecvError> {
        let header = self.segment.header();
        if header.is_closed() {
            return Ok(());
        }
        if self.cursor <= header.write_seq() {
            // There is still data to drain; liveness does not matter yet.
            return Ok(());
        }
        if self.segment.producer_alive() {
            return Ok(());
        }
        // Mark the segment closed so every other reader — and the broker —
        // converges on the same conclusion without repeating the syscall.
        header.mark_closed();
        Err(RecvError::Shm(ShmError::ProducerGone {
            pid: header.producer_pid(),
        }))
    }

    /// Move the cursor explicitly.
    ///
    /// Clamped into the resident window: below the oldest survivor it snaps
    /// forward (and the next receive reports the lag), above `write_seq + 1`
    /// it snaps back.
    pub fn seek(&mut self, seq: u64) {
        let header = self.segment.header();
        let oldest = header.reclaimed_seq() + 1;
        let newest = header.write_seq() + 1;
        self.cursor = seq.max(1).max(oldest).min(newest.max(oldest));
        self.publish_cursor();
    }

    /// Detach: release the consumer-table entry and unregister the doorbell.
    ///
    /// Idempotent; also run by [`Drop`].
    pub fn detach(&mut self) {
        if self.detached {
            return;
        }
        self.detached = true;
        self.segment
            .doorbells()
            .unregister(self.registration.index());
        self.registration.release(self.entry());
        self.segment.header().consumer_detached();
    }

    /// Refresh the heartbeat every [`HEARTBEAT_EVERY`] attempts.
    ///
    /// [`crate::now_ns`] is a `vDSO` read, not a syscall, but a consumer
    /// polling a quiet ring in a tight loop would still make it the dominant
    /// cost of `try_next`. Sampling it periodically keeps the heartbeat
    /// orders of magnitude fresher than the staleness budget
    /// ([`crate::DEFAULT_CONSUMER_STALE_AFTER`], 10 s) while leaving the poll
    /// path to pure atomics.
    fn periodic_heartbeat(&mut self) {
        if self.attempts == 0 {
            self.heartbeat();
        }
        self.attempts = (self.attempts + 1) % HEARTBEAT_EVERY;
    }

    fn entry(&self) -> &ConsumerEntry {
        self.segment.consumer_entry(self.registration.index())
    }

    fn publish_cursor(&self) {
        self.registration.set_cursor(self.entry(), self.cursor);
    }

    fn clamp_to_oldest(&mut self) {
        let oldest = self.segment.header().reclaimed_seq() + 1;
        if self.cursor < oldest {
            self.cursor = oldest;
            self.publish_cursor();
        }
    }
}

impl Drop for Consumer {
    fn drop(&mut self) {
        self.detach();
    }
}

/// A zero-copy read view of one message.
///
/// Holds a pin on its slot: the producer cannot reclaim or overwrite the
/// bytes this points at until the sample is dropped. Keeping a sample alive
/// therefore applies real backpressure — under
/// [`crate::OverflowPolicy::Block`] the producer will report
/// [`ShmError::PoolExhausted`] rather than take the slot back.
///
/// # Trust boundary
///
/// [`Sample::payload`] hands out a `&[u8]` into memory another process can
/// physically write. The slot protocol makes that safe against every
/// *protocol-abiding* peer, and a segment is only ever shared with processes
/// of the same dataflow, brokered by the daemon (§6.3, §16) — that is the
/// boundary, stated plainly. A peer that ignores the protocol can corrupt the
/// bytes under a live sample, exactly as a peer that ignores an mmap'd file
/// format can; [`Sample::to_vec`] is the escape hatch for callers that would
/// rather pay a copy than rely on it.
#[derive(Debug)]
pub struct Sample {
    segment: Arc<Segment>,
    index: u32,
    seq: u64,
    len: usize,
    meta_len: usize,
    commit_ns: u64,
}

impl Sample {
    /// The message's sequence number.
    #[must_use]
    pub const fn seq(&self) -> u64 {
        self.seq
    }

    /// The ring slot the message occupies.
    #[must_use]
    pub const fn slot_index(&self) -> u32 {
        self.index
    }

    /// Wall-clock nanoseconds recorded when the producer committed.
    #[must_use]
    pub const fn commit_ns(&self) -> u64 {
        self.commit_ns
    }

    /// The payload length in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the payload is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The payload bytes, mapped in place.
    ///
    /// 128-byte aligned (§6.1), so an Arrow IPC message read from here is
    /// directly usable as a SIMD source.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        // SAFETY: this sample holds a pin on the slot, so the producer cannot
        // reclaim or rewrite it; `len` was published under that pin and is
        // bounded by the slot's payload capacity; the `Arc<Segment>` keeps
        // the mapping alive for at least `&self`.
        unsafe { std::slice::from_raw_parts(self.segment.payload_ptr(self.index), self.len) }
    }

    /// The metadata bytes that rode beside the payload.
    #[must_use]
    pub fn metadata(&self) -> &[u8] {
        // SAFETY: as `payload`, for the metadata region.
        unsafe { std::slice::from_raw_parts(self.segment.meta_ptr(self.index), self.meta_len) }
    }

    /// Copy the payload out of shared memory.
    ///
    /// The escape hatch for callers that would rather not depend on peer
    /// discipline, and the path a recording (§14) takes when it must outlive
    /// the ring.
    #[must_use]
    pub fn to_vec(&self) -> Vec<u8> {
        self.payload().to_vec()
    }

    /// Copy the metadata out of shared memory.
    #[must_use]
    pub fn metadata_to_vec(&self) -> Vec<u8> {
        self.metadata().to_vec()
    }

    /// The 128-byte-aligned base address of the payload, for callers that
    /// want to assert the alignment contract themselves.
    #[must_use]
    pub fn payload_address(&self) -> usize {
        self.payload().as_ptr() as usize
    }

    /// The first byte of the mapping this sample points into.
    ///
    /// With [`Sample::mapping_len`] and [`Sample::layout_payload_address`],
    /// this is what makes "zero copy" checkable rather than merely claimed: a
    /// payload address that lies inside the mapping *and* at the offset the
    /// layout puts its slot at cannot be a copy, and the two facts are derived
    /// from the segment's geometry rather than from the pointer being checked.
    #[must_use]
    pub fn mapping_base(&self) -> usize {
        self.segment.mapping_base()
    }

    /// How many bytes of the ring are mapped.
    #[must_use]
    pub fn mapping_len(&self) -> usize {
        self.segment.mapped_len()
    }

    /// Where the segment's layout puts this sample's payload.
    ///
    /// Equal to [`Sample::payload_address`] for a sample read in place. They
    /// are computed by different routes — one from the slot pointer, one from
    /// the mapping base plus a layout offset — so comparing them is a real
    /// check and not a restatement.
    #[must_use]
    pub fn layout_payload_address(&self) -> usize {
        self.segment.mapping_base() + self.segment.layout().payload_offset(self.index) as usize
    }
}

impl AsRef<[u8]> for Sample {
    fn as_ref(&self) -> &[u8] {
        self.payload()
    }
}

impl Drop for Sample {
    fn drop(&mut self) {
        // SAFETY: constructed only from a successful `SlotHeader::pin`, and
        // dropped exactly once.
        unsafe { self.segment.slot(self.index).unpin() };
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::attach::StartPosition;
    use crate::config::{OverflowPolicy, SegmentConfig};
    use crate::key::SegmentKey;
    use crate::producer::Producer;
    use astrs_wire::DataflowId;

    fn ring(slots: u32, payload: u32, policy: OverflowPolicy) -> Arc<Segment> {
        let key = SegmentKey::from_parts(DataflowId::generate(), "ring", "out", 1).unwrap();
        let config = SegmentConfig::new(slots, payload)
            .unwrap()
            .with_overflow(policy);
        Segment::create_shared(key, config).unwrap()
    }

    #[test]
    fn a_consumer_reads_everything_a_producer_writes() {
        let segment = ring(8, 256, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        let mut consumer =
            Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();
        assert_eq!(consumer.cursor(), 1);

        for value in 0..8u8 {
            producer.send(&[value; 4], &[value]).unwrap();
        }
        for value in 0..8u8 {
            let sample = consumer.try_next().unwrap();
            assert_eq!(sample.seq(), u64::from(value) + 1);
            assert_eq!(sample.payload(), &[value; 4]);
            assert_eq!(sample.metadata(), &[value]);
            assert!(!sample.is_empty());
            assert_eq!(sample.len(), 4);
            assert_eq!(sample.payload_address() % 128, 0);
        }
        assert!(matches!(consumer.try_next(), Err(RecvError::Empty)));
        assert_eq!(consumer.stats().received, 8);
        assert_eq!(consumer.stats().lagged, 0);
    }

    #[test]
    fn a_live_sample_blocks_reclamation_under_the_block_policy() {
        let segment = ring(2, 128, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        let mut consumer =
            Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();

        producer.send(b"a", b"").unwrap();
        producer.send(b"b", b"").unwrap();
        let held = consumer.try_next().unwrap();
        assert_eq!(held.payload(), b"a");

        // The consumer's cursor has passed sequence 1, but the sample pins it.
        assert!(matches!(
            producer.allocate(1),
            Err(ShmError::PoolExhausted { .. })
        ));
        drop(held);
        assert_eq!(producer.send(b"c", b"").unwrap(), 3);
    }

    #[test]
    fn an_unread_message_blocks_reclamation_under_the_block_policy() {
        let segment = ring(2, 128, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        let mut consumer =
            Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();

        producer.send(b"a", b"").unwrap();
        producer.send(b"b", b"").unwrap();
        assert!(matches!(
            producer.allocate(1),
            Err(ShmError::PoolExhausted { .. })
        ));
        // Draining one message frees exactly one slot.
        drop(consumer.try_next().unwrap());
        assert_eq!(producer.send(b"c", b"").unwrap(), 3);
        assert!(matches!(
            producer.allocate(1),
            Err(ShmError::PoolExhausted { .. })
        ));
        drop(consumer);
        // With no consumers left the ring recycles freely again.
        assert_eq!(producer.send(b"d", b"").unwrap(), 4);
    }

    #[test]
    fn overwrite_reports_an_exact_lag_and_then_resumes() {
        let segment = ring(4, 128, OverflowPolicy::Overwrite);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        let mut consumer =
            Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();

        for value in 0..10u8 {
            producer.send(&[value], b"").unwrap();
        }
        // Sequences 1..=6 were overwritten; 7..=10 survive.
        let error = consumer.try_next().unwrap_err();
        assert_eq!(error.lagged(), Some(6), "{error}");
        assert_eq!(consumer.cursor(), 7);

        for expected in 7..=10u64 {
            let sample = consumer.try_next().unwrap();
            assert_eq!(sample.seq(), expected);
        }
        assert!(matches!(consumer.try_next(), Err(RecvError::Empty)));
        assert_eq!(consumer.stats().lagged, 6);
        assert_eq!(consumer.stats().received, 4);
        assert_eq!(
            consumer.stats().lagged + consumer.stats().received,
            10,
            "every message is either delivered or accounted as lost"
        );
    }

    #[test]
    fn start_positions_select_where_reading_begins() {
        let segment = ring(8, 128, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        for value in 0..4u8 {
            producer.send(&[value], b"").unwrap();
        }

        let mut latest = Consumer::attach(
            Arc::clone(&segment),
            AttachOptions::new().with_start(StartPosition::Latest),
        )
        .unwrap();
        assert_eq!(latest.cursor(), 5);
        assert!(matches!(latest.try_next(), Err(RecvError::Empty)));

        let mut oldest = Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();
        assert_eq!(oldest.cursor(), 1);
        assert_eq!(oldest.try_next().unwrap().seq(), 1);

        let mut chosen = Consumer::attach(
            Arc::clone(&segment),
            AttachOptions::new().with_start(StartPosition::Sequence(3)),
        )
        .unwrap();
        assert_eq!(chosen.cursor(), 3);
        assert_eq!(chosen.try_next().unwrap().seq(), 3);

        // A sequence that has already gone snaps forward.
        let clamped = Consumer::attach(
            Arc::clone(&segment),
            AttachOptions::new().with_start(StartPosition::Sequence(0)),
        )
        .unwrap();
        assert_eq!(clamped.cursor(), 1);
    }

    #[test]
    fn seek_clamps_into_the_resident_window() {
        let segment = ring(4, 128, OverflowPolicy::Overwrite);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        let mut consumer =
            Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();
        for value in 0..8u8 {
            producer.send(&[value], b"").unwrap();
        }
        consumer.seek(1);
        assert_eq!(consumer.cursor(), 5, "below the oldest survivor snaps up");
        consumer.seek(1000);
        assert_eq!(consumer.cursor(), 9, "above write_seq + 1 snaps down");
        consumer.seek(6);
        assert_eq!(consumer.cursor(), 6);
        assert_eq!(consumer.try_next().unwrap().seq(), 6);
    }

    #[test]
    fn the_consumer_table_bounds_the_fan_out() {
        let key = SegmentKey::from_parts(DataflowId::generate(), "fan", "out", 1).unwrap();
        let config = SegmentConfig::new(2, 128)
            .unwrap()
            .with_max_consumers(2)
            .unwrap();
        let segment = Segment::create_shared(key, config).unwrap();

        let first = Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();
        let second = Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();
        assert_ne!(first.index(), second.index());
        assert!(matches!(
            Consumer::attach(Arc::clone(&segment), AttachOptions::default()),
            Err(ShmError::ConsumerTableFull { max_consumers: 2 })
        ));
        assert_eq!(segment.header().attached_consumers(), 2);

        drop(second);
        assert_eq!(segment.header().attached_consumers(), 1);
        // The freed entry is reusable.
        let third = Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();
        assert!(third.token() > 1);
    }

    #[test]
    fn detaching_is_idempotent_and_releases_the_entry() {
        let segment = ring(2, 128, OverflowPolicy::Block);
        let mut consumer =
            Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();
        let index = consumer.index();
        assert!(segment.consumer_entry(index).is_occupied());
        consumer.detach();
        consumer.detach();
        assert!(!segment.consumer_entry(index).is_occupied());
        assert_eq!(segment.header().attached_consumers(), 0);
    }

    #[test]
    fn a_closed_and_drained_segment_reports_closed() {
        let segment = ring(4, 128, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        let mut consumer =
            Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();
        producer.send(b"tail", b"").unwrap();
        producer.close();

        // The tail message is still delivered — closing drains, it does not
        // discard.
        assert_eq!(consumer.try_next().unwrap().payload(), b"tail");
        assert!(matches!(consumer.try_next(), Err(RecvError::Closed)));
        assert!(matches!(
            Consumer::attach(Arc::clone(&segment), AttachOptions::default()),
            Err(ShmError::Closed { .. })
        ));
    }

    #[test]
    fn blocking_receive_wakes_on_the_doorbell() {
        let segment = ring(8, 128, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        let mut consumer =
            Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();

        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            producer.send(b"late", b"").unwrap();
            producer
        });
        let sample = consumer.recv().unwrap();
        assert_eq!(sample.payload(), b"late");
        assert!(consumer.stats().doorbell_waits >= 1);
        let _producer = writer.join().expect("writer thread");
    }

    #[test]
    fn a_blocking_receive_honours_its_timeout() {
        let segment = ring(4, 128, OverflowPolicy::Block);
        let _producer = Producer::new(Arc::clone(&segment)).unwrap();
        let mut consumer =
            Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();
        let started = Instant::now();
        assert!(matches!(
            consumer.next_blocking(Duration::from_millis(60)),
            Err(RecvError::Empty)
        ));
        assert!(started.elapsed() >= Duration::from_millis(40));
    }

    #[test]
    fn a_consumer_without_a_doorbell_still_receives() {
        let segment = ring(4, 128, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        let mut consumer = Consumer::attach(
            Arc::clone(&segment),
            AttachOptions::new().with_doorbell(false),
        )
        .unwrap();
        assert!(consumer.doorbell().is_none());
        producer.send(b"poll", b"").unwrap();
        assert_eq!(
            consumer
                .next_blocking(Duration::from_millis(500))
                .unwrap()
                .payload(),
            b"poll"
        );
    }

    #[test]
    fn backlog_reports_the_distance_to_the_producer() {
        let segment = ring(8, 128, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        let mut consumer =
            Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();
        assert_eq!(consumer.backlog(), 0);
        producer.send(b"1", b"").unwrap();
        producer.send(b"2", b"").unwrap();
        assert_eq!(consumer.backlog(), 2);
        drop(consumer.try_next().unwrap());
        assert_eq!(consumer.backlog(), 1);
    }

    #[test]
    fn attach_verifies_the_expected_identity() {
        let key = SegmentKey::from_parts(DataflowId::generate(), "verify", "out", 5).unwrap();
        let segment =
            Segment::create_shared(key.clone(), SegmentConfig::new(2, 128).unwrap()).unwrap();
        Consumer::attach(
            Arc::clone(&segment),
            AttachOptions::new().expecting(key.clone()),
        )
        .unwrap();
        assert!(matches!(
            Consumer::attach(
                Arc::clone(&segment),
                AttachOptions::new().expecting(key.with_generation(4)),
            ),
            Err(ShmError::StaleGeneration { .. })
        ));
    }

    #[test]
    fn samples_expose_their_bytes_by_reference_and_by_copy() {
        let segment = ring(4, 128, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        let mut consumer =
            Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();
        producer.send(b"payload", b"meta").unwrap();
        let sample = consumer.try_next().unwrap();
        assert_eq!(sample.as_ref(), b"payload");
        assert_eq!(sample.to_vec(), b"payload".to_vec());
        assert_eq!(sample.metadata_to_vec(), b"meta".to_vec());
        assert!(sample.commit_ns() > 0);
        assert_eq!(sample.slot_index(), 0);
    }

    #[test]
    fn attach_options_are_inspectable() {
        let options = AttachOptions::default();
        assert_eq!(options.start(), StartPosition::Oldest);
        assert!(options.doorbell());
        let options = options
            .with_start(StartPosition::Latest)
            .with_doorbell(false);
        assert_eq!(options.start(), StartPosition::Latest);
        assert!(!options.doorbell());
    }

    #[tokio::test]
    async fn the_async_receive_resolves_when_a_message_lands() {
        let segment = ring(8, 128, OverflowPolicy::Block);
        let mut producer = Producer::new(Arc::clone(&segment)).unwrap();
        let mut consumer =
            Consumer::attach(Arc::clone(&segment), AttachOptions::default()).unwrap();

        let handle = tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(30));
            producer.send(b"async", b"").map(|seq| (producer, seq))
        });
        let sample = tokio::time::timeout(Duration::from_secs(5), consumer.recv_async())
            .await
            .expect("no timeout")
            .expect("a message");
        assert_eq!(sample.payload(), b"async");
        let _ = handle.await.expect("writer task").expect("send");
    }
}
