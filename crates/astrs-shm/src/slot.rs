//! The per-slot control block and the pin / reclaim protocol.
//!
//! This module is the correctness heart of the crate: §23 risk #3 ("SHM plane
//! races — SPMC reclamation") is a race in exactly these forty lines of
//! atomics. Everything else is plumbing around them.
//!
//! # Byte layout (64 bytes — one cache line)
//!
//! ```text
//! 0x00  state:AtomicU8   FREE | WRITING | READY
//! 0x01  pad[3]
//! 0x04  refcount:AtomicU32   0..N readers, or RECLAIM_SENTINEL = "gate closed"
//! 0x08  seq:AtomicU64        the 1-based sequence number this slot carries
//! 0x10  len:AtomicU32        payload length
//! 0x14  meta_len:AtomicU32   metadata length
//! 0x18  commit_ns:AtomicU64  wall-clock nanoseconds at commit (telemetry)
//! 0x20  pin_total:AtomicU64        diagnostics
//! 0x28  overwrite_total:AtomicU64  diagnostics
//! 0x30  reserved0:AtomicU64  0x38 reserved1:AtomicU64
//! ```
//!
//! # The gate
//!
//! The blueprint's slot table carries "state (FREE/WRITING/READY/IN_USE
//! (refcnt))". This implementation splits that into an explicit `state` byte
//! and a `refcount` word, and makes **`refcount` the gate**: it is the single
//! memory location every transfer of ownership goes through, so there is no
//! two-location handshake to get wrong.
//!
//! `refcount == RECLAIM_SENTINEL` means *the producer owns this slot*: no
//! reader may pin it. Any other value is the number of live
//! [`crate::Sample`] guards, and implies the slot is READY.
//!
//! | `state` | `refcount` | meaning |
//! |---|---|---|
//! | `FREE` | `SENTINEL` | empty, producer-owned |
//! | `WRITING` | `SENTINEL` | producer is filling it |
//! | `READY` | `SENTINEL` | published or being reclaimed — producer-owned, transient |
//! | `READY` | `0` | published, no reader holds it |
//! | `READY` | `n > 0` | published, `n` zero-copy readers hold it |
//!
//! The load-bearing invariant is one-directional and holds at **every**
//! instant, not just at quiescence:
//!
//! > `state != READY` ⟹ `refcount == SENTINEL`
//!
//! which is precisely "no slot is readable while WRITING" and "no FREE slot
//! has a nonzero refcount". The reverse is deliberately not an invariant:
//! `(READY, SENTINEL)` is the legal transient window in which the producer
//! has taken ownership of a still-published slot but has not yet relabelled
//! it, and the window in which it has relabelled a filled slot but not yet
//! opened the gate.
//!
//! # Why this beats a two-location handshake
//!
//! The obvious alternative — consumer sets `refcount += 1` then re-reads
//! `state`; producer sets `state = RECLAIMING` then reads `refcount` — is
//! Dekker's algorithm. A store followed by a load of a *different* location
//! needs a full StoreLoad barrier on both sides (`SeqCst`, i.e. `mfence` or
//! `dmb ish`) to be correct; acquire/release is not enough, and getting that
//! wrong produces a race that only shows up under load on weakly ordered
//! hardware. Routing every ownership transfer through one location makes the
//! protocol a plain CAS loop whose correctness follows from single-location
//! coherence, which every architecture provides for free.
//!
//! # Ordering rationale, transition by transition
//!
//! **Commit** (producer, `WRITING` → `READY`):
//!
//! 1. payload bytes — plain writes into the exclusively owned data region
//! 2. `len`, `meta_len`, `seq`, `commit_ns` — Relaxed; still exclusive
//! 3. `state = READY` — Relaxed; still exclusive
//! 4. `refcount.store(0, Release)` — **the publication fence.** Everything
//!    above happens-before any reader that acquires the gate.
//!
//! **Pin** (consumer): `compare_exchange_weak(r, r + 1, AcqRel, Acquire)`.
//! The success ordering is Acquire so it synchronises-with step 4 — either
//! directly, or through the release sequence formed by other consumers'
//! successful pins, which are themselves read-modify-writes on the same
//! location. Release on the success side keeps the CAS part of that release
//! sequence, so a *third* consumer pinning after a second still synchronises
//! with the producer.
//!
//! **Unpin**: `fetch_sub(1, Release)` — so the producer's later acquiring CAS
//! sees everything the reader did (it reads only, but the ordering costs
//! nothing and makes the reasoning uniform).
//!
//! **Reclaim** (producer, `READY` → `FREE`):
//! `compare_exchange(0, SENTINEL, AcqRel, Relaxed)`. Acquire pairs with every
//! reader's releasing unpin; failure means a reader is live and the slot is
//! untouchable — which is what makes zero-copy reads sound even under
//! [`crate::OverflowPolicy::Overwrite`].

use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use crate::layout::SLOT_ENTRY_LEN;

/// The `refcount` value that means "the producer owns this slot".
///
/// `u32::MAX` rather than a separate flag word so the gate stays a single
/// location. A ring can therefore hold `u32::MAX - 1` concurrent readers of
/// one slot, four billion more than the [`crate::config::MAX_MAX_CONSUMERS`]
/// the consumer table admits.
pub const RECLAIM_SENTINEL: u32 = u32::MAX;

/// The highest refcount a pin may produce before it refuses.
const MAX_REFCOUNT: u32 = RECLAIM_SENTINEL - 1;

/// The lifecycle state of one slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum SlotState {
    /// Empty and owned by the producer.
    Free = 0,
    /// The producer is filling it; no reader may observe it.
    Writing = 1,
    /// Published. Readable when the gate is open.
    Ready = 2,
}

impl SlotState {
    /// Decode a raw state byte.
    ///
    /// An unrecognised byte decodes to [`SlotState::Free`] rather than
    /// panicking: the byte comes from shared memory a peer could corrupt, and
    /// the gate — not this field — is what authorises access.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::SlotState;
    ///
    /// assert_eq!(SlotState::from_raw(2), SlotState::Ready);
    /// assert_eq!(SlotState::from_raw(200), SlotState::Free);
    /// ```
    #[must_use]
    pub const fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::Writing,
            2 => Self::Ready,
            _ => Self::Free,
        }
    }

    /// The name used in diagnostics.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::SlotState;
    ///
    /// assert_eq!(SlotState::Writing.as_str(), "writing");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Writing => "writing",
            Self::Ready => "ready",
        }
    }

    /// Whether a reader may, in principle, pin a slot in this state.
    #[must_use]
    pub const fn is_readable(self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// One slot's control block, living in shared memory.
#[repr(C, align(64))]
pub struct SlotHeader {
    state: AtomicU8,
    _pad: [u8; 3],
    refcount: AtomicU32,
    seq: AtomicU64,
    len: AtomicU32,
    meta_len: AtomicU32,
    commit_ns: AtomicU64,
    pin_total: AtomicU64,
    overwrite_total: AtomicU64,
    _reserved0: AtomicU64,
    _reserved1: AtomicU64,
}

const _: () = assert!(size_of::<SlotHeader>() == SLOT_ENTRY_LEN as usize);
const _: () = assert!(align_of::<SlotHeader>() == 64);
const _: () = assert!(std::mem::offset_of!(SlotHeader, state) == 0x00);
const _: () = assert!(std::mem::offset_of!(SlotHeader, refcount) == 0x04);
const _: () = assert!(std::mem::offset_of!(SlotHeader, seq) == 0x08);
const _: () = assert!(std::mem::offset_of!(SlotHeader, len) == 0x10);
const _: () = assert!(std::mem::offset_of!(SlotHeader, meta_len) == 0x14);
const _: () = assert!(std::mem::offset_of!(SlotHeader, commit_ns) == 0x18);

/// Why a pin attempt did not produce a readable slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PinFailure {
    /// The producer owns the slot: it is FREE, WRITING, or mid-reclaim.
    Closed,
    /// The refcount is saturated. Unreachable with any legal consumer table
    /// size, but the protocol is total rather than "impossible in practice".
    Saturated,
}

/// A successful pin: the slot's published facts, captured atomically with
/// respect to the producer.
///
/// Reading these fields *after* the gate has been acquired is what makes them
/// safe: the producer cannot have begun to overwrite the slot, because doing
/// so requires closing a gate this pin is holding open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedFacts {
    /// The sequence number the slot carries.
    pub seq: u64,
    /// The payload length in bytes.
    pub len: u32,
    /// The metadata length in bytes.
    pub meta_len: u32,
    /// The wall-clock nanoseconds recorded at commit.
    pub commit_ns: u64,
}

impl SlotHeader {
    /// Reinterpret a pointer into the slot table as a slot header.
    ///
    /// # Safety
    ///
    /// `ptr` must point at a live, 64-byte-aligned slot-table entry inside a
    /// validated mapping, and the mapping must outlive `'a`.
    #[must_use]
    pub unsafe fn from_ptr<'a>(ptr: *const u8) -> &'a Self {
        // SAFETY: the caller guarantees a live, aligned slot entry; every
        // field is an atomic, so shared access is sound under concurrent
        // mutation by peers.
        unsafe { &*ptr.cast::<Self>() }
    }

    /// Put a freshly created slot into `(FREE, SENTINEL)`.
    ///
    /// # Safety
    ///
    /// Must run before the segment is published — i.e. before any other
    /// process can map it.
    pub(crate) unsafe fn initialize(&self) {
        self.seq.store(0, Ordering::Relaxed);
        self.len.store(0, Ordering::Relaxed);
        self.meta_len.store(0, Ordering::Relaxed);
        self.commit_ns.store(0, Ordering::Relaxed);
        self.pin_total.store(0, Ordering::Relaxed);
        self.overwrite_total.store(0, Ordering::Relaxed);
        self.state.store(SlotState::Free as u8, Ordering::Relaxed);
        self.refcount.store(RECLAIM_SENTINEL, Ordering::Relaxed);
    }

    /// The slot's current state.
    ///
    /// Relaxed: the state byte is advisory for everyone but the producer, and
    /// the producer is its only writer.
    #[must_use]
    pub fn state(&self) -> SlotState {
        SlotState::from_raw(self.state.load(Ordering::Relaxed))
    }

    /// The raw gate value.
    ///
    /// [`RECLAIM_SENTINEL`] means producer-owned; anything else is a live
    /// reader count.
    #[must_use]
    pub fn refcount(&self) -> u32 {
        self.refcount.load(Ordering::Acquire)
    }

    /// The number of live readers, or `None` when the producer owns the slot.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::{SegmentConfig, Segment, SegmentKey};
    /// # use astrs_wire::DataflowId;
    /// # #[cfg(unix)] {
    /// let key = SegmentKey::from_parts(DataflowId::generate(), "n", "o", 1)?;
    /// let segment = Segment::create(key, SegmentConfig::new(2, 128)?)?;
    /// // A fresh ring is entirely producer-owned.
    /// assert_eq!(segment.slot(0).readers(), None);
    /// # }
    /// # Ok::<(), astrs_shm::ShmError>(())
    /// ```
    #[must_use]
    pub fn readers(&self) -> Option<u32> {
        match self.refcount() {
            RECLAIM_SENTINEL => None,
            count => Some(count),
        }
    }

    /// The sequence number the slot carries; `0` means never published.
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }

    /// The payload length recorded at the last commit.
    #[must_use]
    pub fn len(&self) -> u32 {
        self.len.load(Ordering::Relaxed)
    }

    /// Whether the last commit stored an empty payload.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The metadata length recorded at the last commit.
    #[must_use]
    pub fn meta_len(&self) -> u32 {
        self.meta_len.load(Ordering::Relaxed)
    }

    /// How many times this slot has been pinned since creation.
    #[must_use]
    pub fn pin_total(&self) -> u64 {
        self.pin_total.load(Ordering::Relaxed)
    }

    /// How many times this slot has been force-reclaimed under
    /// [`crate::OverflowPolicy::Overwrite`].
    #[must_use]
    pub fn overwrite_total(&self) -> u64 {
        self.overwrite_total.load(Ordering::Relaxed)
    }

    // -- consumer side ----------------------------------------------------

    /// Acquire a read pin.
    ///
    /// On success the slot cannot be reclaimed until the matching
    /// `SlotHeader::unpin` (crate-private), and the returned facts are the ones that were
    /// published for the slot's current occupant.
    ///
    /// # Errors
    ///
    /// [`PinFailure::Closed`] when the producer owns the slot,
    /// [`PinFailure::Saturated`] when the reader count is at its ceiling.
    pub fn pin(&self) -> Result<PinnedFacts, PinFailure> {
        let mut current = self.refcount.load(Ordering::Acquire);
        loop {
            if current == RECLAIM_SENTINEL {
                return Err(PinFailure::Closed);
            }
            if current >= MAX_REFCOUNT {
                return Err(PinFailure::Saturated);
            }
            match self.refcount.compare_exchange_weak(
                current,
                current + 1,
                // Success: AcqRel. Acquire so the payload writes that
                // preceded the producer's releasing gate-open are visible;
                // Release so this RMW extends the release sequence for the
                // next consumer that pins on top of us.
                Ordering::AcqRel,
                // Failure: Acquire, so a retry sees the freshest value and
                // still synchronises if the value we lost to was a gate-open.
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
        self.pin_total.fetch_add(1, Ordering::Relaxed);
        // Safe to read now: the gate is held open, so the producer cannot be
        // mutating any of these.
        Ok(PinnedFacts {
            seq: self.seq.load(Ordering::Relaxed),
            len: self.len.load(Ordering::Relaxed),
            meta_len: self.meta_len.load(Ordering::Relaxed),
            commit_ns: self.commit_ns.load(Ordering::Relaxed),
        })
    }

    /// Release a read pin.
    ///
    /// # Safety
    ///
    /// Must be called exactly once for each successful [`SlotHeader::pin`],
    /// and never otherwise — an unmatched call would let the producer
    /// overwrite bytes a live [`crate::Sample`] still points at. The only
    /// caller is `Sample::drop`.
    ///
    /// `Sample::drop` and this module's own portable unit tests are the
    /// only callers; the former is reachable only through the
    /// `#[cfg(unix)]` consumer/producer machinery, so this is dead code on
    /// a Windows build otherwise.
    #[cfg(any(unix, test))]
    pub(crate) unsafe fn unpin(&self) {
        // Release: everything this reader did happens-before the producer's
        // acquiring reclaim CAS.
        let previous = self.refcount.fetch_sub(1, Ordering::Release);
        debug_assert!(
            previous != 0 && previous != RECLAIM_SENTINEL,
            "unpin without a matching pin (refcount was {previous})"
        );
    }

    // -- producer side ----------------------------------------------------

    /// Try to take exclusive ownership of a READY slot.
    ///
    /// Succeeds only when no reader holds it. This is the operation that
    /// makes zero-copy sound: a pinned slot simply cannot be reclaimed, under
    /// either overflow policy.
    ///
    /// Returns `false` when a reader is live.
    ///
    /// Called only from the `#[cfg(unix)]` producer and this module's own
    /// portable unit tests — dead code on a Windows build otherwise.
    #[cfg(any(unix, test))]
    #[must_use]
    pub(crate) fn try_close_gate(&self) -> bool {
        self.refcount
            .compare_exchange(0, RECLAIM_SENTINEL, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    /// Mark an exclusively owned slot free.
    ///
    /// # Safety
    ///
    /// The caller must hold exclusive ownership — either from a successful
    /// [`SlotHeader::try_close_gate`] or from the slot never having been
    /// published.
    ///
    /// Called only from the `#[cfg(unix)]` producer and this module's own
    /// portable unit tests — dead code on a Windows build otherwise.
    #[cfg(any(unix, test))]
    pub(crate) unsafe fn set_free(&self) {
        self.state.store(SlotState::Free as u8, Ordering::Relaxed);
    }

    /// Mark an exclusively owned slot as being written.
    ///
    /// # Safety
    ///
    /// As [`SlotHeader::set_free`].
    ///
    /// Called only from the `#[cfg(unix)]` producer and this module's own
    /// portable unit tests — dead code on a Windows build otherwise.
    #[cfg(any(unix, test))]
    pub(crate) unsafe fn begin_write(&self) {
        self.state
            .store(SlotState::Writing as u8, Ordering::Relaxed);
    }

    /// Publish an exclusively owned slot.
    ///
    /// Performs steps 2–4 of the commit protocol documented at the top of
    /// this module: record the facts, relabel the slot, then open the gate
    /// with a Release store.
    ///
    /// # Safety
    ///
    /// The caller must hold exclusive ownership and must already have written
    /// `len` payload bytes and `meta_len` metadata bytes into the slot's data
    /// region.
    ///
    /// Called only from the `#[cfg(unix)]` producer and this module's own
    /// portable unit tests — dead code on a Windows build otherwise.
    #[cfg(any(unix, test))]
    pub(crate) unsafe fn publish(&self, seq: u64, len: u32, meta_len: u32, commit_ns: u64) {
        self.len.store(len, Ordering::Relaxed);
        self.meta_len.store(meta_len, Ordering::Relaxed);
        self.commit_ns.store(commit_ns, Ordering::Relaxed);
        self.seq.store(seq, Ordering::Relaxed);
        self.state.store(SlotState::Ready as u8, Ordering::Relaxed);
        // Release: the publication fence. Pairs with the acquiring CAS in
        // `pin`.
        self.refcount.store(0, Ordering::Release);
    }

    /// Abandon a write in progress, returning the slot to FREE and reopening
    /// nothing — the slot stays producer-owned and reusable.
    ///
    /// # Safety
    ///
    /// The caller must hold exclusive ownership.
    ///
    /// Called only from the `#[cfg(unix)]` producer and this module's own
    /// portable unit tests — dead code on a Windows build otherwise.
    #[cfg(any(unix, test))]
    pub(crate) unsafe fn abort_write(&self) {
        self.state.store(SlotState::Free as u8, Ordering::Relaxed);
    }

    /// Record that this slot was force-reclaimed under the overwrite policy.
    ///
    /// Called only from the `#[cfg(unix)]` producer and this module's own
    /// portable unit tests — dead code on a Windows build otherwise.
    #[cfg(any(unix, test))]
    pub(crate) fn record_overwrite(&self) {
        self.overwrite_total.fetch_add(1, Ordering::Relaxed);
    }

    /// A diagnostic snapshot of the slot.
    ///
    /// The two halves are read gate-first so the reported pair can never
    /// claim a readable slot that was actually producer-owned at the instant
    /// the gate was sampled.
    #[must_use]
    pub fn snapshot(&self) -> SlotSnapshot {
        let refcount = self.refcount.load(Ordering::Acquire);
        SlotSnapshot {
            state: self.state(),
            refcount,
            seq: self.seq.load(Ordering::Relaxed),
            len: self.len.load(Ordering::Relaxed),
            meta_len: self.meta_len.load(Ordering::Relaxed),
        }
    }
}

/// A point-in-time view of a slot, for tests and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SlotSnapshot {
    /// The lifecycle state.
    pub state: SlotState,
    /// The raw gate value ([`RECLAIM_SENTINEL`] = producer-owned).
    pub refcount: u32,
    /// The sequence number carried.
    pub seq: u64,
    /// The payload length.
    pub len: u32,
    /// The metadata length.
    pub meta_len: u32,
}

impl SlotSnapshot {
    /// Whether the producer exclusively owns the slot.
    #[must_use]
    pub const fn is_producer_owned(&self) -> bool {
        self.refcount == RECLAIM_SENTINEL
    }

    /// The number of live readers, or `None` when producer-owned.
    #[must_use]
    pub const fn readers(&self) -> Option<u32> {
        if self.is_producer_owned() {
            None
        } else {
            Some(self.refcount)
        }
    }

    /// Check the module's load-bearing invariant on this snapshot.
    ///
    /// Returns the name of the violated rule, or `None` when the snapshot is
    /// consistent. Used by the proptest state machine and the torture suite.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(unix)] {
    /// use astrs_shm::{Segment, SegmentConfig, SegmentKey};
    /// use astrs_wire::DataflowId;
    ///
    /// let key = SegmentKey::from_parts(DataflowId::generate(), "n", "o", 1)?;
    /// let segment = Segment::create(key, SegmentConfig::new(2, 128)?)?;
    /// // A freshly created ring is entirely consistent.
    /// assert_eq!(segment.slot(0).snapshot().violation(), None);
    /// # }
    /// # Ok::<(), astrs_shm::ShmError>(())
    /// ```
    #[must_use]
    pub const fn violation(&self) -> Option<&'static str> {
        if !self.state.is_readable() && self.refcount != RECLAIM_SENTINEL {
            return Some("a non-READY slot has an open gate");
        }
        if matches!(self.state, SlotState::Ready) && self.seq == 0 {
            return Some("a READY slot carries no sequence number");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::thread;

    /// A standalone, correctly aligned slot for unit testing the protocol
    /// without a mapping.
    #[repr(C, align(64))]
    struct SlotBox([u8; SLOT_ENTRY_LEN as usize]);

    struct Scratch {
        storage: Box<SlotBox>,
    }

    impl Scratch {
        fn new() -> Self {
            let scratch = Self {
                storage: Box::new(SlotBox([0; SLOT_ENTRY_LEN as usize])),
            };
            // SAFETY: freshly allocated, exclusively owned, correctly aligned.
            unsafe { scratch.slot().initialize() };
            scratch
        }

        fn slot(&self) -> &SlotHeader {
            // SAFETY: the box is 64-byte aligned, exactly one entry long, and
            // outlives the borrow.
            unsafe { SlotHeader::from_ptr(self.storage.0.as_ptr()) }
        }
    }

    #[test]
    fn a_fresh_slot_is_free_and_producer_owned() {
        let scratch = Scratch::new();
        let slot = scratch.slot();
        assert_eq!(slot.state(), SlotState::Free);
        assert_eq!(slot.refcount(), RECLAIM_SENTINEL);
        assert_eq!(slot.readers(), None);
        assert_eq!(slot.seq(), 0);
        assert!(slot.is_empty());
        assert_eq!(slot.snapshot().violation(), None);
    }

    #[test]
    fn a_free_slot_cannot_be_pinned() {
        let scratch = Scratch::new();
        assert_eq!(scratch.slot().pin(), Err(PinFailure::Closed));
    }

    #[test]
    fn the_full_commit_cycle() {
        let scratch = Scratch::new();
        let slot = scratch.slot();

        // SAFETY: single-threaded scratch slot; the producer owns it.
        unsafe { slot.begin_write() };
        assert_eq!(slot.state(), SlotState::Writing);
        assert_eq!(
            slot.pin(),
            Err(PinFailure::Closed),
            "WRITING is not readable"
        );
        assert_eq!(slot.snapshot().violation(), None);

        // SAFETY: as above.
        unsafe { slot.publish(1, 42, 7, 1234) };
        assert_eq!(slot.state(), SlotState::Ready);
        assert_eq!(slot.readers(), Some(0));
        assert_eq!(slot.snapshot().violation(), None);

        let facts = slot.pin().unwrap();
        assert_eq!(
            facts,
            PinnedFacts {
                seq: 1,
                len: 42,
                meta_len: 7,
                commit_ns: 1234
            }
        );
        assert_eq!(slot.readers(), Some(1));
        assert!(
            !slot.try_close_gate(),
            "a pinned slot must not be reclaimable"
        );

        // SAFETY: exactly one matching pin.
        unsafe { slot.unpin() };
        assert_eq!(slot.readers(), Some(0));
        assert!(slot.try_close_gate());
        assert_eq!(slot.refcount(), RECLAIM_SENTINEL);
        // SAFETY: exclusive after a successful gate close.
        unsafe { slot.set_free() };
        assert_eq!(slot.state(), SlotState::Free);
        assert_eq!(slot.snapshot().violation(), None);
    }

    #[test]
    fn many_readers_share_one_slot() {
        let scratch = Scratch::new();
        let slot = scratch.slot();
        // SAFETY: single-threaded scratch slot.
        unsafe {
            slot.begin_write();
            slot.publish(9, 1, 0, 0);
        }
        for expected in 0..16 {
            assert_eq!(slot.readers(), Some(expected));
            assert_eq!(slot.pin().unwrap().seq, 9);
        }
        assert_eq!(slot.readers(), Some(16));
        assert!(!slot.try_close_gate());
        for _ in 0..16 {
            // SAFETY: matched pins.
            unsafe { slot.unpin() };
        }
        assert!(slot.try_close_gate());
        assert_eq!(slot.pin_total(), 16);
    }

    #[test]
    fn abort_returns_a_slot_without_publishing_it() {
        let scratch = Scratch::new();
        let slot = scratch.slot();
        // SAFETY: single-threaded scratch slot.
        unsafe {
            slot.begin_write();
            slot.abort_write();
        }
        assert_eq!(slot.state(), SlotState::Free);
        assert_eq!(slot.refcount(), RECLAIM_SENTINEL);
        assert_eq!(slot.pin(), Err(PinFailure::Closed));
        assert_eq!(slot.seq(), 0);
    }

    #[test]
    fn overwrite_accounting() {
        let scratch = Scratch::new();
        let slot = scratch.slot();
        assert_eq!(slot.overwrite_total(), 0);
        slot.record_overwrite();
        slot.record_overwrite();
        assert_eq!(slot.overwrite_total(), 2);
    }

    #[test]
    fn slot_state_decoding_is_total() {
        assert_eq!(SlotState::from_raw(0), SlotState::Free);
        assert_eq!(SlotState::from_raw(1), SlotState::Writing);
        assert_eq!(SlotState::from_raw(2), SlotState::Ready);
        for raw in 3u8..=255 {
            assert_eq!(SlotState::from_raw(raw), SlotState::Free);
        }
        assert!(SlotState::Ready.is_readable());
        assert!(!SlotState::Writing.is_readable());
        assert!(!SlotState::Free.is_readable());
        assert_eq!(SlotState::Free.as_str(), "free");
        assert_eq!(SlotState::Ready.as_str(), "ready");
    }

    #[test]
    fn snapshot_violation_detection() {
        let base = SlotSnapshot {
            state: SlotState::Ready,
            refcount: 0,
            seq: 1,
            len: 0,
            meta_len: 0,
        };
        assert_eq!(base.violation(), None);
        assert_eq!(base.readers(), Some(0));
        assert!(!base.is_producer_owned());

        let owned = SlotSnapshot {
            refcount: RECLAIM_SENTINEL,
            ..base
        };
        assert_eq!(owned.violation(), None);
        assert!(owned.is_producer_owned());
        assert_eq!(owned.readers(), None);

        let writing_but_open = SlotSnapshot {
            state: SlotState::Writing,
            refcount: 0,
            ..base
        };
        assert_eq!(
            writing_but_open.violation(),
            Some("a non-READY slot has an open gate")
        );

        let ready_without_seq = SlotSnapshot { seq: 0, ..base };
        assert_eq!(
            ready_without_seq.violation(),
            Some("a READY slot carries no sequence number")
        );
    }

    /// The protocol's central claim, exercised concurrently: while any thread
    /// holds a pin, no thread can close the gate; and a slot is never
    /// simultaneously pinned and producer-owned.
    #[test]
    fn concurrent_pins_and_reclaims_never_overlap() {
        let scratch = Arc::new(Scratch::new());
        // SAFETY: nothing else touches the slot yet.
        unsafe {
            scratch.slot().begin_write();
            scratch.slot().publish(1, 8, 0, 0);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let overlap = Arc::new(AtomicBool::new(false));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let scratch = Arc::clone(&scratch);
            let stop = Arc::clone(&stop);
            let overlap = Arc::clone(&overlap);
            handles.push(thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if scratch.slot().pin().is_ok() {
                        // While pinned, the gate must be open and the state
                        // must be READY — the producer cannot have taken it.
                        if scratch.slot().refcount() == RECLAIM_SENTINEL
                            || scratch.slot().state() != SlotState::Ready
                        {
                            overlap.store(true, Ordering::Relaxed);
                        }
                        // SAFETY: matched with the successful pin above.
                        unsafe { scratch.slot().unpin() };
                    }
                }
            }));
        }

        // The producer: repeatedly reclaim and republish.
        let producer = {
            let scratch = Arc::clone(&scratch);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut seq = 2u64;
                let mut reclaims = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    if scratch.slot().try_close_gate() {
                        // SAFETY: exclusive after a successful gate close.
                        unsafe {
                            scratch.slot().set_free();
                            scratch.slot().begin_write();
                            scratch.slot().publish(seq, 8, 0, 0);
                        }
                        seq += 1;
                        reclaims += 1;
                    }
                }
                reclaims
            })
        };

        thread::sleep(std::time::Duration::from_millis(150));
        stop.store(true, Ordering::Relaxed);
        for handle in handles {
            handle.join().expect("reader thread panicked");
        }
        let reclaims = producer.join().expect("producer thread panicked");

        assert!(
            !overlap.load(Ordering::Relaxed),
            "a pin overlapped a reclaim"
        );
        assert!(reclaims > 0, "the producer never made progress");
        assert_eq!(scratch.slot().snapshot().violation(), None);
    }
}
