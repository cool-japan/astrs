//! The per-consumer control page: cursors, heartbeats and eviction.
//!
//! Blueprint §6.2: *"consumers attach read-only. SPMC with per-consumer
//! cursors kept in a small per-consumer control page — a slot is reclaimable
//! when all attached cursors passed it (drop-token protocol done in shared
//! atomics, not messages)."*
//!
//! This module is that control page. One 64-byte entry per consumer — one
//! cache line, so two readers advancing their cursors never contend — in a
//! fixed-size table sized at segment creation, so attaching allocates
//! nothing.
//!
//! # Byte layout (64 bytes per entry)
//!
//! ```text
//! 0x00  state:AtomicU32     EMPTY | CLAIMING | ACTIVE | EVICTED
//! 0x04  token:AtomicU32     incremented on every (re)use — the drop token
//! 0x08  cursor:AtomicU64    the next sequence this consumer wants
//! 0x10  heartbeat_ns:AtomicU64
//! 0x18  pid:AtomicI64
//! 0x20  lagged_total:AtomicU64
//! 0x28  received_total:AtomicU64
//! 0x30  attach_ns:AtomicU64
//! 0x38  flags:AtomicU64
//! ```
//!
//! # The claim protocol, and why the *releaser* clears the cursor
//!
//! A recycled entry must never present a stale cursor, because the producer
//! reads cursors to decide what it may overwrite. A cursor left behind by a
//! previous occupant could be *higher* than the new occupant's, which would
//! let the producer reclaim messages the new consumer still wanted.
//!
//! So clearing is the responsibility of whoever releases the entry, not
//! whoever claims it:
//!
//! - **release** (detach or eviction): `cursor = 0` (Relaxed), then
//!   `state = EMPTY` (**Release**).
//! - **claim**: `compare_exchange(EMPTY, CLAIMING, AcqRel, …)` — the
//!   **Acquire** half synchronises with that Release, so the claimer is
//!   guaranteed to observe `cursor == 0`.
//! - The claimer then fills in pid, heartbeat and its start cursor, and
//!   finally stores `state = ACTIVE` with **Release**.
//!
//! While an entry is `CLAIMING` its cursor reads `0`, and the producer treats
//! a zero cursor as *"unknown — reclaim nothing"*. An attach can therefore
//! never cause the producer to drop a message the attaching consumer was
//! entitled to; the worst case is that the producer briefly cannot reclaim.
//!
//! # Eviction and the ABA question
//!
//! A crashed consumer's frozen cursor would wedge the ring forever, so the
//! producer's reclaim pass evicts entries that are **both** stale past
//! [`crate::SegmentConfig::consumer_stale_after`] **and** owned by a pid that
//! no longer exists. Both conditions are required: a live-but-stalled
//! consumer (a node blocked on a slow sensor) keeps its slot.
//!
//! That pairing is also what closes the ABA hole. Recycling an entry under a
//! live occupant would let the old occupant write a stale cursor into an
//! entry now owned by someone else — but eviction only happens once the
//! occupant's process has been confirmed dead, and a dead process writes
//! nothing. The [`ConsumerEntry::token`] word is still bumped on every reuse
//! and still checked by [`ConsumerRegistration`] before every write, as
//! defence in depth: it lives on the same cache line as the cursor, so the
//! check costs nothing measurable.

use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};

use crate::layout::CONSUMER_ENTRY_LEN;

/// The entry is unused and may be claimed.
pub const CONSUMER_EMPTY: u32 = 0;
/// The entry is being filled in by a claimer; its cursor reads `0`.
pub const CONSUMER_CLAIMING: u32 = 1;
/// The entry is live: its cursor is authoritative.
pub const CONSUMER_ACTIVE: u32 = 2;
/// The entry was evicted by the producer and is about to become empty.
pub const CONSUMER_EVICTED: u32 = 3;

/// Flag bit: the consumer wants a doorbell ring on every commit.
pub const CONSUMER_FLAG_DOORBELL: u64 = 1 << 0;

/// One consumer's control entry, living in shared memory.
#[repr(C, align(64))]
pub struct ConsumerEntry {
    state: AtomicU32,
    token: AtomicU32,
    cursor: AtomicU64,
    heartbeat_ns: AtomicU64,
    pid: AtomicI64,
    lagged_total: AtomicU64,
    received_total: AtomicU64,
    attach_ns: AtomicU64,
    flags: AtomicU64,
}

const _: () = assert!(size_of::<ConsumerEntry>() == CONSUMER_ENTRY_LEN as usize);
const _: () = assert!(align_of::<ConsumerEntry>() == 64);
const _: () = assert!(std::mem::offset_of!(ConsumerEntry, state) == 0x00);
const _: () = assert!(std::mem::offset_of!(ConsumerEntry, token) == 0x04);
const _: () = assert!(std::mem::offset_of!(ConsumerEntry, cursor) == 0x08);
const _: () = assert!(std::mem::offset_of!(ConsumerEntry, heartbeat_ns) == 0x10);
const _: () = assert!(std::mem::offset_of!(ConsumerEntry, pid) == 0x18);
const _: () = assert!(std::mem::offset_of!(ConsumerEntry, lagged_total) == 0x20);
const _: () = assert!(std::mem::offset_of!(ConsumerEntry, received_total) == 0x28);
const _: () = assert!(std::mem::offset_of!(ConsumerEntry, attach_ns) == 0x30);
const _: () = assert!(std::mem::offset_of!(ConsumerEntry, flags) == 0x38);

impl ConsumerEntry {
    /// Reinterpret a pointer into the consumer table as an entry.
    ///
    /// # Safety
    ///
    /// `ptr` must point at a live, 64-byte-aligned consumer-table entry
    /// inside a validated mapping that outlives `'a`.
    #[must_use]
    pub unsafe fn from_ptr<'a>(ptr: *const u8) -> &'a Self {
        // SAFETY: the caller guarantees a live, aligned entry; every field is
        // an atomic, so shared access is sound under concurrent mutation.
        unsafe { &*ptr.cast::<Self>() }
    }

    /// Zero a freshly created entry.
    ///
    /// # Safety
    ///
    /// Must run before the segment is published.
    pub(crate) unsafe fn initialize(&self) {
        self.cursor.store(0, Ordering::Relaxed);
        self.heartbeat_ns.store(0, Ordering::Relaxed);
        self.pid.store(0, Ordering::Relaxed);
        self.lagged_total.store(0, Ordering::Relaxed);
        self.received_total.store(0, Ordering::Relaxed);
        self.attach_ns.store(0, Ordering::Relaxed);
        self.flags.store(0, Ordering::Relaxed);
        self.token.store(0, Ordering::Relaxed);
        self.state.store(CONSUMER_EMPTY, Ordering::Relaxed);
    }

    /// The entry's state word.
    #[must_use]
    pub fn state(&self) -> u32 {
        self.state.load(Ordering::Acquire)
    }

    /// The drop token of the current (or most recent) occupant.
    #[must_use]
    pub fn token(&self) -> u32 {
        self.token.load(Ordering::Relaxed)
    }

    /// The next sequence number this consumer wants to read.
    ///
    /// `0` means "unknown" — either the entry is empty, or a claim is in
    /// flight. The producer treats it as a hard stop on reclamation.
    #[must_use]
    pub fn cursor(&self) -> u64 {
        self.cursor.load(Ordering::Acquire)
    }

    /// The consumer's last heartbeat, in nanoseconds.
    #[must_use]
    pub fn heartbeat_ns(&self) -> u64 {
        self.heartbeat_ns.load(Ordering::Relaxed)
    }

    /// The consumer's process id.
    #[must_use]
    pub fn pid(&self) -> i64 {
        self.pid.load(Ordering::Relaxed)
    }

    /// How many messages this consumer has missed to overwrite.
    #[must_use]
    pub fn lagged_total(&self) -> u64 {
        self.lagged_total.load(Ordering::Relaxed)
    }

    /// How many messages this consumer has received.
    #[must_use]
    pub fn received_total(&self) -> u64 {
        self.received_total.load(Ordering::Relaxed)
    }

    /// When this consumer attached, in nanoseconds.
    #[must_use]
    pub fn attach_ns(&self) -> u64 {
        self.attach_ns.load(Ordering::Relaxed)
    }

    /// Whether this consumer asked to be woken by the doorbell.
    #[must_use]
    pub fn wants_doorbell(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & CONSUMER_FLAG_DOORBELL != 0
    }

    /// Whether this entry currently constrains reclamation.
    #[must_use]
    pub fn is_occupied(&self) -> bool {
        matches!(self.state(), CONSUMER_CLAIMING | CONSUMER_ACTIVE)
    }

    /// Try to take this entry.
    ///
    /// Returns the new drop token on success.
    ///
    /// Only reachable through [`claim_entry`], whose own callers are the
    /// `#[cfg(unix)]` producer/consumer machinery and this module's own
    /// portable unit tests — dead code on a Windows build otherwise.
    #[cfg(any(unix, test))]
    fn try_claim(&self) -> Option<u32> {
        self.state
            .compare_exchange(
                CONSUMER_EMPTY,
                CONSUMER_CLAIMING,
                // Acquire: synchronises with the releasing store of EMPTY, so
                // the cleared cursor is visible. Release: the claim is itself
                // an ownership transfer.
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .ok()?;
        // Now exclusively ours: no other claimer can see EMPTY.
        let token = self.token.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        Some(token)
    }

    /// Take an entry and stop, leaving it `CONSUMER_CLAIMING` with its cursor
    /// still zero.
    ///
    /// This is the window [`claim_entry`] passes through between
    /// [`ConsumerEntry::try_claim`] and [`ConsumerEntry::activate`], and it is
    /// observable by a producer: `refresh_active` rescans the whole table
    /// whenever the consumer epoch moves, so a producer refreshing because
    /// consumer A detached can see consumer B's half-finished claim. Reaching
    /// that interleaving by racing threads is not reproducible, so the
    /// reclamation guard is tested against this state directly.
    ///
    /// Its only caller is a `producer.rs` unit test, and that whole module
    /// is `#[cfg(unix)]` — dead code on a Windows `--tests` build otherwise.
    #[cfg(all(unix, test))]
    pub(crate) fn claim_in_flight(&self) -> Option<u32> {
        self.try_claim()
    }

    /// Finish a claim: publish the consumer's identity and start cursor.
    ///
    /// # Safety
    ///
    /// Must follow a successful [`ConsumerEntry::try_claim`] on the same
    /// entry, exactly once.
    #[cfg(any(unix, test))]
    unsafe fn activate(&self, pid: i64, cursor: u64, now_ns: u64, doorbell: bool) {
        self.pid.store(pid, Ordering::Relaxed);
        self.heartbeat_ns.store(now_ns, Ordering::Relaxed);
        self.attach_ns.store(now_ns, Ordering::Relaxed);
        self.lagged_total.store(0, Ordering::Relaxed);
        self.received_total.store(0, Ordering::Relaxed);
        self.flags.store(
            if doorbell { CONSUMER_FLAG_DOORBELL } else { 0 },
            Ordering::Relaxed,
        );
        // Release: the cursor must be visible to the producer no later than
        // the ACTIVE state that makes the producer read it.
        self.cursor.store(cursor.max(1), Ordering::Release);
        self.state.store(CONSUMER_ACTIVE, Ordering::Release);
    }

    /// Release the entry, clearing the cursor first.
    ///
    /// # Safety
    ///
    /// The caller must own the entry (a matching claim, or a successful
    /// eviction CAS).
    unsafe fn release(&self) {
        self.pid.store(0, Ordering::Relaxed);
        self.heartbeat_ns.store(0, Ordering::Relaxed);
        self.flags.store(0, Ordering::Relaxed);
        // Clearing the cursor *before* publishing EMPTY is the whole point of
        // the release-side protocol; see the module docs.
        self.cursor.store(0, Ordering::Relaxed);
        self.state.store(CONSUMER_EMPTY, Ordering::Release);
    }

    /// Try to evict this entry, moving it ACTIVE → EVICTED → EMPTY.
    ///
    /// Returns `true` if this call performed the eviction. Racing evictors
    /// (the producer and the broker may both scan) see `false`.
    ///
    /// Called only from the `#[cfg(unix)]` producer and this module's own
    /// portable unit tests — dead code on a Windows build otherwise.
    #[cfg(any(unix, test))]
    pub(crate) fn try_evict(&self) -> bool {
        if self
            .state
            .compare_exchange(
                CONSUMER_ACTIVE,
                CONSUMER_EVICTED,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return false;
        }
        // Bump the token so a resurrected occupant's writes are rejected.
        self.token.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the CAS above transferred ownership of the entry to us.
        unsafe { self.release() };
        true
    }

    /// A snapshot for diagnostics.
    #[must_use]
    pub fn snapshot(&self) -> ConsumerSnapshot {
        ConsumerSnapshot {
            state: self.state(),
            token: self.token(),
            cursor: self.cursor(),
            heartbeat_ns: self.heartbeat_ns(),
            pid: self.pid(),
            lagged_total: self.lagged_total(),
            received_total: self.received_total(),
            attach_ns: self.attach_ns(),
            wants_doorbell: self.wants_doorbell(),
        }
    }
}

/// A point-in-time view of one consumer entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConsumerSnapshot {
    /// The state word.
    pub state: u32,
    /// The drop token.
    pub token: u32,
    /// The cursor (`0` = unknown).
    pub cursor: u64,
    /// The last heartbeat, in nanoseconds.
    pub heartbeat_ns: u64,
    /// The consumer's pid.
    pub pid: i64,
    /// Messages missed to overwrite.
    pub lagged_total: u64,
    /// Messages received.
    pub received_total: u64,
    /// Attach time, in nanoseconds.
    pub attach_ns: u64,
    /// Whether the consumer wants doorbell wakeups.
    pub wants_doorbell: bool,
}

impl ConsumerSnapshot {
    /// Whether this entry constrains the producer's reclamation.
    #[must_use]
    pub const fn is_occupied(&self) -> bool {
        matches!(self.state, CONSUMER_CLAIMING | CONSUMER_ACTIVE)
    }

    /// The human-readable state name.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[cfg(unix)] {
    /// use std::sync::Arc;
    /// use astrs_shm::{AttachOptions, Consumer, Segment, SegmentConfig, SegmentKey};
    /// use astrs_wire::DataflowId;
    ///
    /// let key = SegmentKey::from_parts(DataflowId::generate(), "n", "o", 1)?;
    /// let segment = Segment::create_shared(key, SegmentConfig::new(2, 128)?)?;
    /// assert_eq!(segment.consumer_entry(0).snapshot().state_name(), "empty");
    ///
    /// let consumer = Consumer::attach(Arc::clone(&segment), AttachOptions::default())?;
    /// let snapshot = segment.consumer_entry(consumer.index()).snapshot();
    /// assert_eq!(snapshot.state_name(), "active");
    /// assert!(snapshot.is_occupied());
    /// # }
    /// # Ok::<(), astrs_shm::ShmError>(())
    /// ```
    #[must_use]
    pub const fn state_name(&self) -> &'static str {
        match self.state {
            CONSUMER_CLAIMING => "claiming",
            CONSUMER_ACTIVE => "active",
            CONSUMER_EVICTED => "evicted",
            _ => "empty",
        }
    }
}

/// A live claim on one consumer-table entry.
///
/// Owned by a [`crate::Consumer`]; dropping the consumer releases the entry.
/// Every mutating method re-checks the drop token, so a registration whose
/// entry was recycled underneath it becomes inert rather than corrupting the
/// new occupant's state.
#[derive(Debug)]
pub struct ConsumerRegistration {
    index: u32,
    token: u32,
}

impl ConsumerRegistration {
    /// The table index this registration owns.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }

    /// The drop token proving ownership.
    #[must_use]
    pub const fn token(&self) -> u32 {
        self.token
    }

    /// Whether the entry is still ours.
    ///
    /// A `false` here means the producer evicted us after confirming our
    /// process was dead — which, from inside a running process, can only
    /// happen if the pid was reused or the clock jumped. The consumer treats
    /// it as a detach.
    #[must_use]
    pub fn is_valid(&self, entry: &ConsumerEntry) -> bool {
        entry.token.load(Ordering::Relaxed) == self.token
            && entry.state.load(Ordering::Relaxed) == CONSUMER_ACTIVE
    }

    /// Publish a new cursor.
    ///
    /// **Release**: the producer reads cursors to decide what it may
    /// overwrite, and must never see a cursor advance before the reads that
    /// justified it retired.
    pub fn set_cursor(&self, entry: &ConsumerEntry, cursor: u64) {
        if entry.token.load(Ordering::Relaxed) != self.token {
            return;
        }
        entry.cursor.store(cursor, Ordering::Release);
    }

    /// Refresh the heartbeat word.
    pub fn touch(&self, entry: &ConsumerEntry, now_ns: u64) {
        if entry.token.load(Ordering::Relaxed) != self.token {
            return;
        }
        entry.heartbeat_ns.fetch_max(now_ns, Ordering::Relaxed);
    }

    /// Record `count` received messages.
    pub fn record_received(&self, entry: &ConsumerEntry, count: u64) {
        entry.received_total.fetch_add(count, Ordering::Relaxed);
    }

    /// Record `count` messages lost to overwrite.
    pub fn record_lagged(&self, entry: &ConsumerEntry, count: u64) {
        entry.lagged_total.fetch_add(count, Ordering::Relaxed);
    }

    /// Release the entry.
    ///
    /// Idempotent with respect to eviction: if the entry was already
    /// recycled, this does nothing.
    pub fn release(&self, entry: &ConsumerEntry) {
        if entry.token.load(Ordering::Relaxed) != self.token {
            return;
        }
        if entry
            .state
            .compare_exchange(
                CONSUMER_ACTIVE,
                CONSUMER_EVICTED,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return;
        }
        // SAFETY: the CAS transferred ownership of the entry back to us.
        unsafe { entry.release() };
    }
}

/// Claim a free entry in a consumer table.
///
/// Walks the table until a `CONSUMER_EMPTY` entry is taken. Concurrent
/// claimers cannot collide: only one CAS on a given entry can succeed, and a
/// loser simply carries on to the next index.
///
/// Called only from the `#[cfg(unix)]` producer/consumer machinery and this
/// module's own portable unit tests — dead code on a Windows build
/// otherwise (the plane's Windows side has no consumer-table registration
/// yet; see `crate::os::windows`'s module docs).
#[cfg(any(unix, test))]
pub(crate) fn claim_entry<'a>(
    entries: impl Iterator<Item = (u32, &'a ConsumerEntry)>,
    pid: i64,
    cursor: u64,
    now_ns: u64,
    doorbell: bool,
) -> Option<ConsumerRegistration> {
    for (index, entry) in entries {
        if let Some(token) = entry.try_claim() {
            // SAFETY: `try_claim` succeeded, so the entry is exclusively ours
            // and is in the CLAIMING state.
            unsafe { entry.activate(pid, cursor, now_ns, doorbell) };
            return Some(ConsumerRegistration { index, token });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[repr(C, align(64))]
    struct EntryBox([u8; CONSUMER_ENTRY_LEN as usize]);

    struct Scratch {
        storage: Box<EntryBox>,
    }

    impl Scratch {
        fn new() -> Self {
            let scratch = Self {
                storage: Box::new(EntryBox([0xff; CONSUMER_ENTRY_LEN as usize])),
            };
            // SAFETY: freshly allocated, exclusively owned, 64-byte aligned.
            unsafe { scratch.entry().initialize() };
            scratch
        }

        fn entry(&self) -> &ConsumerEntry {
            // SAFETY: the box is 64-byte aligned, one entry long, and
            // outlives the borrow.
            unsafe { ConsumerEntry::from_ptr(self.storage.0.as_ptr()) }
        }
    }

    fn claim(entry: &'static ConsumerEntry, cursor: u64) -> Option<ConsumerRegistration> {
        claim_entry(std::iter::once((0u32, entry)), 42, cursor, 100, true)
    }

    /// Leak a scratch entry so it satisfies the `'static` bound the claim
    /// helper takes (matching the real, mapping-lifetime call site).
    fn leaked_entry() -> &'static ConsumerEntry {
        let scratch = Box::leak(Box::new(Scratch::new()));
        scratch.entry()
    }

    #[test]
    fn a_fresh_entry_is_empty_with_a_zero_cursor() {
        let scratch = Scratch::new();
        let entry = scratch.entry();
        assert_eq!(entry.state(), CONSUMER_EMPTY);
        assert_eq!(entry.cursor(), 0);
        assert_eq!(entry.pid(), 0);
        assert_eq!(entry.token(), 0);
        assert!(!entry.is_occupied());
        assert!(!entry.wants_doorbell());
    }

    #[test]
    fn claim_activate_release_cycle() {
        let entry = leaked_entry();
        let registration = claim(entry, 5).expect("the only entry must be claimable");
        assert_eq!(registration.index(), 0);
        assert_eq!(registration.token(), 1);
        assert_eq!(entry.state(), CONSUMER_ACTIVE);
        assert_eq!(entry.cursor(), 5);
        assert_eq!(entry.pid(), 42);
        assert_eq!(entry.heartbeat_ns(), 100);
        assert!(entry.wants_doorbell());
        assert!(entry.is_occupied());
        assert!(registration.is_valid(entry));

        registration.set_cursor(entry, 9);
        assert_eq!(entry.cursor(), 9);
        registration.record_received(entry, 3);
        registration.record_lagged(entry, 2);
        registration.touch(entry, 500);
        assert_eq!(entry.received_total(), 3);
        assert_eq!(entry.lagged_total(), 2);
        assert_eq!(entry.heartbeat_ns(), 500);
        // A heartbeat never moves backwards.
        registration.touch(entry, 100);
        assert_eq!(entry.heartbeat_ns(), 500);

        registration.release(entry);
        assert_eq!(entry.state(), CONSUMER_EMPTY);
        assert_eq!(
            entry.cursor(),
            0,
            "the releaser must clear the cursor for the next claimer"
        );
        assert!(!registration.is_valid(entry));
        // Releasing twice is a no-op.
        registration.release(entry);
        assert_eq!(entry.state(), CONSUMER_EMPTY);
    }

    #[test]
    fn a_claimed_entry_cannot_be_claimed_again() {
        let entry = leaked_entry();
        let first = claim(entry, 1).expect("first claim");
        assert!(claim(entry, 1).is_none(), "double claim must fail");
        first.release(entry);
        let second = claim(entry, 1).expect("re-claim after release");
        assert_eq!(second.token(), 2, "the drop token must advance on reuse");
    }

    #[test]
    fn a_zero_start_cursor_is_raised_to_one() {
        let entry = leaked_entry();
        let _registration = claim(entry, 0).expect("claim");
        assert_eq!(
            entry.cursor(),
            1,
            "sequence numbers are 1-based; a 0 cursor would read as 'unknown'"
        );
    }

    #[test]
    fn eviction_recycles_the_entry_and_invalidates_the_registration() {
        let entry = leaked_entry();
        let registration = claim(entry, 7).expect("claim");
        assert!(entry.try_evict());
        assert!(!entry.try_evict(), "eviction is not repeatable");
        assert_eq!(entry.state(), CONSUMER_EMPTY);
        assert_eq!(entry.cursor(), 0);
        assert!(!registration.is_valid(entry));

        // The stale registration must not be able to write into the recycled
        // entry — this is the drop-token guard.
        registration.set_cursor(entry, 999);
        assert_eq!(entry.cursor(), 0);
        registration.touch(entry, 999);
        assert_eq!(entry.heartbeat_ns(), 0);
        registration.release(entry);
        assert_eq!(entry.state(), CONSUMER_EMPTY);
    }

    #[test]
    fn claim_scans_past_occupied_entries() {
        let first = leaked_entry();
        let second = leaked_entry();
        let occupied = claim_entry([(0u32, first), (1u32, second)].into_iter(), 1, 1, 0, false)
            .expect("first claim");
        assert_eq!(occupied.index(), 0);

        let next = claim_entry([(0u32, first), (1u32, second)].into_iter(), 2, 1, 0, false)
            .expect("second claim lands on the free entry");
        assert_eq!(next.index(), 1);

        assert!(
            claim_entry([(0u32, first), (1u32, second)].into_iter(), 3, 1, 0, false).is_none(),
            "a full table must refuse"
        );
    }

    #[test]
    fn snapshots_report_the_entry_faithfully() {
        let entry = leaked_entry();
        let registration = claim(entry, 3).expect("claim");
        registration.record_received(entry, 11);
        let snapshot = entry.snapshot();
        assert_eq!(snapshot.state_name(), "active");
        assert!(snapshot.is_occupied());
        assert_eq!(snapshot.cursor, 3);
        assert_eq!(snapshot.received_total, 11);
        assert_eq!(snapshot.pid, 42);
        assert!(snapshot.wants_doorbell);

        registration.release(entry);
        let snapshot = entry.snapshot();
        assert_eq!(snapshot.state_name(), "empty");
        assert!(!snapshot.is_occupied());
    }

    #[test]
    fn state_names_cover_every_encoding() {
        let base = ConsumerSnapshot {
            state: CONSUMER_EMPTY,
            token: 0,
            cursor: 0,
            heartbeat_ns: 0,
            pid: 0,
            lagged_total: 0,
            received_total: 0,
            attach_ns: 0,
            wants_doorbell: false,
        };
        assert_eq!(base.state_name(), "empty");
        assert_eq!(
            ConsumerSnapshot {
                state: CONSUMER_CLAIMING,
                ..base
            }
            .state_name(),
            "claiming"
        );
        assert_eq!(
            ConsumerSnapshot {
                state: CONSUMER_ACTIVE,
                ..base
            }
            .state_name(),
            "active"
        );
        assert_eq!(
            ConsumerSnapshot {
                state: CONSUMER_EVICTED,
                ..base
            }
            .state_name(),
            "evicted"
        );
        assert_eq!(ConsumerSnapshot { state: 99, ..base }.state_name(), "empty");
    }

    #[test]
    fn concurrent_claims_never_hand_out_the_same_entry() {
        let entries: Vec<&'static ConsumerEntry> = (0..8).map(|_| leaked_entry()).collect();
        let entries: &'static [&'static ConsumerEntry] = Box::leak(entries.into_boxed_slice());

        let mut handles = Vec::new();
        for worker in 0..8 {
            handles.push(std::thread::spawn(move || {
                claim_entry(
                    entries
                        .iter()
                        .copied()
                        .enumerate()
                        .map(|(index, entry)| (u32::try_from(index).unwrap_or(u32::MAX), entry)),
                    i64::from(worker),
                    1,
                    0,
                    false,
                )
                .map(|registration| registration.index())
            }));
        }

        let mut claimed: Vec<u32> = handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .expect("worker panicked")
                    .expect("a free entry")
            })
            .collect();
        claimed.sort_unstable();
        claimed.dedup();
        assert_eq!(claimed.len(), 8, "every claim must get a distinct entry");
    }
}
