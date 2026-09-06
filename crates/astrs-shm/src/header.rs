//! The 128-byte segment header (blueprint §6.2).
//!
//! # Byte layout
//!
//! ```text
//!        ┌─ cold half: written once at creation, read-only for the segment's life ─┐
//! 0x00   magic:AtomicU32   "ASHM" in host byte order — also the publication flag
//! 0x04   layout_ver:AtomicU16      0x06  header_len:AtomicU16 (== 128)
//! 0x08   key_digest_lo:AtomicU64   0x10  key_digest_hi:AtomicU64
//! 0x18   generation:AtomicU64
//! 0x20   slot_count:AtomicU32      0x24  payload_capacity:AtomicU32
//! 0x28   meta_capacity:AtomicU32   0x2c  max_consumers:AtomicU32
//! 0x30   slot_table_off:AtomicU32  0x34  consumer_table_off:AtomicU32
//! 0x38   data_off:AtomicU32        0x3c  flags:AtomicU32 (overflow policy, backing)
//!        ├─────────────── cache-line boundary (0x40) ───────────────┤
//!        └─ hot half: the atomics the data plane actually hammers ──┘
//! 0x40   write_seq:AtomicU64        ← the one word every consumer polls
//! 0x48   closed:AtomicU8  0x49 pad[3]  0x4c attached_consumers:AtomicU32
//! 0x50   fallback_total:AtomicU64   ← `shm_fallback_total` metric (§6.2)
//! 0x58   producer_pid:AtomicI64
//! 0x60   producer_heartbeat:AtomicU64
//! 0x68   consumer_epoch:AtomicU64   ← bumped on attach/detach/eviction
//! 0x70   reclaimed_seq:AtomicU64    ← oldest sequence still resident
//! 0x78   total_len:AtomicU64
//! ```
//!
//! # Why every field is an atomic
//!
//! Even the fields that are logically immutable after creation are declared
//! atomic and read with [`Ordering::Relaxed`]. A segment is mapped by
//! processes AstRS does not control the code of once the mapping is brokered
//! out; a plain `u32` read racing with a peer's write would be undefined
//! behaviour in the Rust abstract machine even though the hardware would
//! simply return a torn-or-not value. Relaxed atomic loads compile to exactly
//! the same instruction on x86-64 and AArch64, so the discipline is free.
//!
//! # Publication protocol
//!
//! The creator writes every field *except* the magic, then stores the magic
//! with [`Ordering::Release`]. Every reader loads the magic with
//! [`Ordering::Acquire`] before touching anything else. That single pair is
//! what makes a mapping either fully initialised or visibly not-a-segment:
//! a reader that maps a freshly `ftruncate`d object sees a zero magic and
//! gets [`crate::ShmError::BadMagic`] rather than a header of zeroes it might
//! mistake for a legal geometry.
//!
//! # Byte order
//!
//! Native. Segments never leave the host — the cross-host plane is
//! `astrs-transport` (§6.4) — so byte-swapping on every field access would
//! buy nothing and cost a load-store on the hot path. The magic is stored
//! with [`u32::from_ne_bytes`] so a hexdump still reads `ASHM`.

use std::sync::atomic::{AtomicI64, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};

use crate::config::{Backing, OverflowPolicy, SegmentConfig};
use crate::key::SegmentKey;
use crate::layout::{HEADER_LEN, HeaderFields, LAYOUT_VERSION, SEGMENT_MAGIC, SegmentLayout};

/// The magic word as stored in the header, in host byte order.
///
/// # Examples
///
/// ```
/// use astrs_shm::MAGIC_WORD;
///
/// assert_eq!(MAGIC_WORD.to_ne_bytes(), *b"ASHM");
/// ```
pub const MAGIC_WORD: u32 = u32::from_ne_bytes(SEGMENT_MAGIC);

/// Bit 0 of the header `flags` word: the overflow policy is `Overwrite`.
const FLAG_OVERWRITE: u32 = 1 << 0;
/// Bits 1–2 of the header `flags` word: the OS backing.
const FLAG_BACKING_SHIFT: u32 = 1;
const FLAG_BACKING_MASK: u32 = 0b11 << FLAG_BACKING_SHIFT;
const BACKING_MEMFD: u32 = 0;
const BACKING_NAMED: u32 = 1;

/// The mapped segment header.
///
/// Never constructed by value — it is only ever reached through a pointer
/// into a mapping, via [`SegmentHeader::from_ptr`].
#[repr(C, align(128))]
pub struct SegmentHeader {
    // --- cold half -------------------------------------------------------
    magic: AtomicU32,
    layout_ver: AtomicU16,
    header_len: AtomicU16,
    key_digest_lo: AtomicU64,
    key_digest_hi: AtomicU64,
    generation: AtomicU64,
    slot_count: AtomicU32,
    payload_capacity: AtomicU32,
    meta_capacity: AtomicU32,
    max_consumers: AtomicU32,
    slot_table_offset: AtomicU32,
    consumer_table_offset: AtomicU32,
    data_offset: AtomicU32,
    flags: AtomicU32,
    // --- hot half --------------------------------------------------------
    write_seq: AtomicU64,
    closed: AtomicU8,
    _pad: [u8; 3],
    attached_consumers: AtomicU32,
    fallback_total: AtomicU64,
    producer_pid: AtomicI64,
    producer_heartbeat: AtomicU64,
    consumer_epoch: AtomicU64,
    reclaimed_seq: AtomicU64,
    total_len: AtomicU64,
}

// The layout is normative: any change to it is a `LAYOUT_VERSION` bump, and
// these assertions are what make that impossible to forget.
const _: () = assert!(size_of::<SegmentHeader>() == HEADER_LEN as usize);
const _: () = assert!(align_of::<SegmentHeader>() == 128);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, magic) == 0x00);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, layout_ver) == 0x04);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, header_len) == 0x06);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, key_digest_lo) == 0x08);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, key_digest_hi) == 0x10);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, generation) == 0x18);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, slot_count) == 0x20);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, payload_capacity) == 0x24);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, meta_capacity) == 0x28);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, max_consumers) == 0x2c);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, slot_table_offset) == 0x30);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, consumer_table_offset) == 0x34);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, data_offset) == 0x38);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, flags) == 0x3c);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, write_seq) == 0x40);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, closed) == 0x48);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, attached_consumers) == 0x4c);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, fallback_total) == 0x50);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, producer_pid) == 0x58);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, producer_heartbeat) == 0x60);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, consumer_epoch) == 0x68);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, reclaimed_seq) == 0x70);
const _: () = assert!(std::mem::offset_of!(SegmentHeader, total_len) == 0x78);

impl SegmentHeader {
    /// Reinterpret the start of a mapping as a header.
    ///
    /// # Safety
    ///
    /// `ptr` must be the base of a mapping of at least [`HEADER_LEN`] bytes,
    /// aligned to 128 (every `mmap` result is page-aligned, so this holds for
    /// any mapping this crate produces), and must stay mapped for `'a`.
    #[must_use]
    pub unsafe fn from_ptr<'a>(ptr: *const u8) -> &'a Self {
        // SAFETY: the caller guarantees the pointer names a live mapping of
        // at least `HEADER_LEN` bytes with 128-byte alignment, and every
        // field of `Self` is an atomic, so shared access is sound even while
        // peers mutate the same bytes.
        unsafe { &*ptr.cast::<Self>() }
    }

    /// Write every cold field, then publish the magic.
    ///
    /// # Safety
    ///
    /// Must be called exactly once, by the creating process, before the
    /// segment's descriptor or name is made reachable by any other process.
    pub(crate) unsafe fn initialize(
        &self,
        key: &SegmentKey,
        layout: &SegmentLayout,
        config: &SegmentConfig,
        producer_pid: i64,
        created_ns: u64,
    ) {
        let digest = key.digest();
        self.key_digest_lo
            .store((digest & u128::from(u64::MAX)) as u64, Ordering::Relaxed);
        self.key_digest_hi
            .store((digest >> 64) as u64, Ordering::Relaxed);
        self.generation.store(key.generation(), Ordering::Relaxed);
        self.layout_ver.store(LAYOUT_VERSION, Ordering::Relaxed);
        self.header_len.store(HEADER_LEN as u16, Ordering::Relaxed);
        self.slot_count
            .store(layout.slot_count(), Ordering::Relaxed);
        self.payload_capacity
            .store(layout.payload_capacity(), Ordering::Relaxed);
        self.meta_capacity
            .store(layout.meta_capacity(), Ordering::Relaxed);
        self.max_consumers
            .store(layout.max_consumers(), Ordering::Relaxed);
        self.slot_table_offset.store(
            truncate_offset(layout.slot_table_offset()),
            Ordering::Relaxed,
        );
        self.consumer_table_offset.store(
            truncate_offset(layout.consumer_table_offset()),
            Ordering::Relaxed,
        );
        self.data_offset
            .store(truncate_offset(layout.data_offset()), Ordering::Relaxed);
        self.flags.store(encode_flags(config), Ordering::Relaxed);
        self.total_len.store(layout.total_len(), Ordering::Relaxed);

        self.write_seq.store(0, Ordering::Relaxed);
        self.closed.store(0, Ordering::Relaxed);
        self.attached_consumers.store(0, Ordering::Relaxed);
        self.fallback_total.store(0, Ordering::Relaxed);
        self.producer_pid.store(producer_pid, Ordering::Relaxed);
        self.producer_heartbeat.store(created_ns, Ordering::Relaxed);
        self.consumer_epoch.store(0, Ordering::Relaxed);
        self.reclaimed_seq.store(0, Ordering::Relaxed);

        // Release: everything above must be visible to any thread or process
        // that observes the magic. This is the segment's publication fence.
        self.magic.store(MAGIC_WORD, Ordering::Release);
    }

    /// Snapshot the fields the layout validator needs.
    ///
    /// Loads the magic with [`Ordering::Acquire`] first, so a snapshot that
    /// reports the right magic is guaranteed to have observed a fully
    /// initialised header.
    #[must_use]
    pub fn fields(&self) -> HeaderFields {
        let magic = self.magic.load(Ordering::Acquire);
        HeaderFields {
            magic: magic.to_ne_bytes(),
            layout_ver: self.layout_ver.load(Ordering::Relaxed),
            header_len: self.header_len.load(Ordering::Relaxed),
            slot_count: self.slot_count.load(Ordering::Relaxed),
            payload_capacity: self.payload_capacity.load(Ordering::Relaxed),
            meta_capacity: self.meta_capacity.load(Ordering::Relaxed),
            max_consumers: self.max_consumers.load(Ordering::Relaxed),
            slot_table_offset: u64::from(self.slot_table_offset.load(Ordering::Relaxed)),
            consumer_table_offset: u64::from(self.consumer_table_offset.load(Ordering::Relaxed)),
            data_offset: u64::from(self.data_offset.load(Ordering::Relaxed)),
            total_len: self.total_len.load(Ordering::Relaxed),
        }
    }

    /// The producer's incarnation counter.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// The 128-bit digest of the segment key.
    #[must_use]
    pub fn key_digest(&self) -> u128 {
        let lo = u128::from(self.key_digest_lo.load(Ordering::Relaxed));
        let hi = u128::from(self.key_digest_hi.load(Ordering::Relaxed));
        (hi << 64) | lo
    }

    /// The pid of the producing process.
    #[must_use]
    pub fn producer_pid(&self) -> i64 {
        self.producer_pid.load(Ordering::Relaxed)
    }

    /// Overwrite the recorded producer pid.
    ///
    /// Used when a segment is created by the broker on behalf of a node that
    /// has not been spawned yet: the broker stamps its own pid at creation
    /// and the daemon corrects it once the child exists.
    pub fn set_producer_pid(&self, pid: i64) {
        self.producer_pid.store(pid, Ordering::Relaxed);
    }

    /// The last published sequence number; `0` before the first commit.
    ///
    /// **Acquire.** This is the consumer's gate: observing `write_seq >= n`
    /// must imply observing every byte the producer wrote for sequences up to
    /// `n`. The producer's matching [`SegmentHeader::publish_seq`] uses
    /// Release.
    #[must_use]
    pub fn write_seq(&self) -> u64 {
        self.write_seq.load(Ordering::Acquire)
    }

    /// The producer's private view of `write_seq`.
    ///
    /// The producer is the only writer, so it can use a Relaxed load: no
    /// other thread can change the value between its load and its store.
    #[must_use]
    pub fn write_seq_relaxed(&self) -> u64 {
        self.write_seq.load(Ordering::Relaxed)
    }

    /// Publish a sequence number.
    ///
    /// **Release.** Pairs with [`SegmentHeader::write_seq`].
    pub fn publish_seq(&self, seq: u64) {
        self.write_seq.store(seq, Ordering::Release);
    }

    /// The oldest sequence number still resident in the ring.
    #[must_use]
    pub fn reclaimed_seq(&self) -> u64 {
        self.reclaimed_seq.load(Ordering::Relaxed)
    }

    /// Record that sequences below `seq` are gone.
    pub fn set_reclaimed_seq(&self, seq: u64) {
        self.reclaimed_seq.fetch_max(seq, Ordering::Relaxed);
    }

    /// Whether the segment has been marked closed.
    ///
    /// **Acquire.** A consumer that observes `closed` must also observe every
    /// message the producer committed before closing, so the drain-then-detach
    /// sequence in §6.2 cannot lose a tail message.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire) != 0
    }

    /// Mark the segment closed.
    ///
    /// **Release**, pairing with [`SegmentHeader::is_closed`]. Idempotent:
    /// the broker calls it on producer death, the producer calls it on a
    /// graceful shutdown, and both may happen.
    pub fn mark_closed(&self) {
        self.closed.store(1, Ordering::Release);
    }

    /// The number of consumers currently registered in the consumer table.
    #[must_use]
    pub fn attached_consumers(&self) -> u32 {
        self.attached_consumers.load(Ordering::Acquire)
    }

    /// Increment the attached-consumer count, returning the previous value.
    pub fn consumer_attached(&self) -> u32 {
        self.consumer_epoch.fetch_add(1, Ordering::Release);
        self.attached_consumers.fetch_add(1, Ordering::AcqRel)
    }

    /// Decrement the attached-consumer count, returning the previous value.
    ///
    /// Saturating: a double detach (a crash recovery path racing an orderly
    /// detach) must not wrap the counter to four billion and make the segment
    /// look permanently busy.
    pub fn consumer_detached(&self) -> u32 {
        self.consumer_epoch.fetch_add(1, Ordering::Release);
        let mut current = self.attached_consumers.load(Ordering::Relaxed);
        loop {
            if current == 0 {
                return 0;
            }
            match self.attached_consumers.compare_exchange_weak(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(previous) => return previous,
                Err(observed) => current = observed,
            }
        }
    }

    /// A counter bumped on every attach, detach and eviction.
    ///
    /// The producer caches the set of active consumer cursors between
    /// reclaim passes; this epoch tells it when the cache is stale, so the
    /// common case costs one Relaxed load instead of a full table scan.
    #[must_use]
    pub fn consumer_epoch(&self) -> u64 {
        self.consumer_epoch.load(Ordering::Acquire)
    }

    /// Bump the consumer epoch — used by the eviction path.
    pub fn bump_consumer_epoch(&self) {
        self.consumer_epoch.fetch_add(1, Ordering::Release);
    }

    /// The `shm_fallback_total` metric (blueprint §6.2).
    #[must_use]
    pub fn fallback_total(&self) -> u64 {
        self.fallback_total.load(Ordering::Relaxed)
    }

    /// Record one fall-back to the reliable daemon path.
    pub fn record_fallback(&self) -> u64 {
        self.fallback_total.fetch_add(1, Ordering::Relaxed)
    }

    /// The producer's heartbeat word, in nanoseconds.
    #[must_use]
    pub fn producer_heartbeat(&self) -> u64 {
        self.producer_heartbeat.load(Ordering::Relaxed)
    }

    /// Refresh the producer heartbeat.
    pub fn touch_producer(&self, now_ns: u64) {
        self.producer_heartbeat.fetch_max(now_ns, Ordering::Relaxed);
    }

    /// The overflow policy chosen at creation.
    #[must_use]
    pub fn overflow_policy(&self) -> OverflowPolicy {
        if self.flags.load(Ordering::Relaxed) & FLAG_OVERWRITE == 0 {
            OverflowPolicy::Block
        } else {
            OverflowPolicy::Overwrite
        }
    }

    /// The OS backing recorded at creation.
    #[must_use]
    pub fn backing(&self) -> Backing {
        let raw = (self.flags.load(Ordering::Relaxed) & FLAG_BACKING_MASK) >> FLAG_BACKING_SHIFT;
        match raw {
            BACKING_NAMED => Backing::Named,
            // `Auto` is resolved at creation, so anything else was written by
            // a build that resolved to memfd.
            _ => Backing::Memfd,
        }
    }

    /// The total segment length recorded at creation.
    #[must_use]
    pub fn total_len(&self) -> u64 {
        self.total_len.load(Ordering::Relaxed)
    }
}

/// A cheap, `Send`-able snapshot of the header's observable state.
///
/// Handed to telemetry (§13) and `astrs doctor` (§17) so a diagnostic does
/// not have to hold a live mapping or reason about orderings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SegmentHeaderView {
    /// The producer's incarnation counter.
    pub generation: u64,
    /// The 128-bit key digest.
    pub key_digest: u128,
    /// The producing process id.
    pub producer_pid: i64,
    /// The number of slots.
    pub slot_count: u32,
    /// The per-slot payload capacity.
    pub payload_capacity: u32,
    /// The per-slot metadata capacity.
    pub meta_capacity: u32,
    /// The last published sequence number.
    pub write_seq: u64,
    /// The oldest sequence still resident.
    pub reclaimed_seq: u64,
    /// Whether the segment has been closed.
    pub closed: bool,
    /// How many consumers are registered.
    pub attached_consumers: u32,
    /// The `shm_fallback_total` counter.
    pub fallback_total: u64,
    /// The producer heartbeat, in nanoseconds.
    pub producer_heartbeat_ns: u64,
    /// The overflow policy.
    pub overflow: OverflowPolicy,
    /// The total segment length in bytes.
    pub total_len: u64,
}

impl SegmentHeaderView {
    /// Snapshot a mapped header.
    #[must_use]
    pub fn capture(header: &SegmentHeader) -> Self {
        Self {
            generation: header.generation(),
            key_digest: header.key_digest(),
            producer_pid: header.producer_pid(),
            slot_count: header.slot_count.load(Ordering::Relaxed),
            payload_capacity: header.payload_capacity.load(Ordering::Relaxed),
            meta_capacity: header.meta_capacity.load(Ordering::Relaxed),
            write_seq: header.write_seq(),
            reclaimed_seq: header.reclaimed_seq(),
            closed: header.is_closed(),
            attached_consumers: header.attached_consumers(),
            fallback_total: header.fallback_total(),
            producer_heartbeat_ns: header.producer_heartbeat(),
            overflow: header.overflow_policy(),
            total_len: header.total_len(),
        }
    }

    /// How many messages are currently resident in the ring.
    ///
    /// # Examples
    ///
    /// ```
    /// # use astrs_shm::{SegmentConfig, Segment, SegmentKey};
    /// # use astrs_wire::DataflowId;
    /// # #[cfg(unix)] {
    /// let key = SegmentKey::from_parts(DataflowId::generate(), "n", "o", 1)?;
    /// let segment = Segment::create(key, SegmentConfig::new(4, 256)?)?;
    /// assert_eq!(segment.view().resident(), 0);
    /// # }
    /// # Ok::<(), astrs_shm::ShmError>(())
    /// ```
    #[must_use]
    pub const fn resident(&self) -> u64 {
        self.write_seq.saturating_sub(self.reclaimed_seq)
    }
}

const fn truncate_offset(offset: u64) -> u32 {
    // Every offset the layout produces is far below `u32::MAX`; the layout
    // validator enforces `MAX_SEGMENT_LEN` and the offsets are all smaller
    // still. Saturating keeps this total without an `unwrap`, and a value
    // that ever hit the cap would be rejected by `validate_header` anyway.
    if offset > u32::MAX as u64 {
        u32::MAX
    } else {
        offset as u32
    }
}

fn encode_flags(config: &SegmentConfig) -> u32 {
    let mut flags = 0;
    if config.overflow().may_overwrite() {
        flags |= FLAG_OVERWRITE;
    }
    let backing = match config.backing() {
        Backing::Named => BACKING_NAMED,
        // `Auto` is resolved to a concrete backing before the header is
        // written; anything still `Auto` here came from a build where memfd
        // was the resolution.
        Backing::Memfd | Backing::Auto => BACKING_MEMFD,
    };
    flags |= (backing << FLAG_BACKING_SHIFT) & FLAG_BACKING_MASK;
    flags
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::DataflowId;

    /// A 128-byte, 128-aligned scratch buffer standing in for a mapping.
    #[repr(C, align(128))]
    struct HeaderBox([u8; HEADER_LEN as usize]);

    fn scratch() -> Box<HeaderBox> {
        Box::new(HeaderBox([0; HEADER_LEN as usize]))
    }

    fn key() -> SegmentKey {
        SegmentKey::from_parts(DataflowId::from_u128(0xfeed), "camera", "image", 7).unwrap()
    }

    #[test]
    fn magic_hexdumps_as_ashm() {
        assert_eq!(MAGIC_WORD.to_ne_bytes(), *b"ASHM");
    }

    #[test]
    fn an_uninitialised_mapping_is_not_a_segment() {
        let buffer = scratch();
        // SAFETY: 128 aligned bytes, alive for the borrow.
        let header = unsafe { SegmentHeader::from_ptr(buffer.0.as_ptr()) };
        let fields = header.fields();
        assert_eq!(fields.magic, [0, 0, 0, 0]);
        assert!(
            SegmentLayout::validate_header(&fields, HEADER_LEN as usize).is_err(),
            "a zeroed mapping must not validate"
        );
    }

    #[test]
    fn initialize_then_validate_round_trips() {
        let buffer = scratch();
        let config = SegmentConfig::new(8, 4096)
            .unwrap()
            .with_overflow(OverflowPolicy::Overwrite)
            .with_backing(Backing::Named);
        let layout = SegmentLayout::new(&config).unwrap();
        // SAFETY: 128 aligned bytes, alive for the borrow; called once.
        let header = unsafe { SegmentHeader::from_ptr(buffer.0.as_ptr()) };
        // SAFETY: this scratch header is unreachable by any other thread.
        unsafe { header.initialize(&key(), &layout, &config, 4242, 99) };

        let fields = header.fields();
        let validated =
            SegmentLayout::validate_header(&fields, layout.total_len() as usize).unwrap();
        assert_eq!(validated, layout);
        assert_eq!(header.generation(), 7);
        assert_eq!(header.key_digest(), key().digest());
        assert_eq!(header.producer_pid(), 4242);
        assert_eq!(header.overflow_policy(), OverflowPolicy::Overwrite);
        assert_eq!(header.backing(), Backing::Named);
        assert_eq!(header.total_len(), layout.total_len());
        assert_eq!(header.producer_heartbeat(), 99);
        assert!(!header.is_closed());
    }

    #[test]
    fn sequence_publication_and_closure() {
        let buffer = scratch();
        let config = SegmentConfig::new(2, 128).unwrap();
        let layout = SegmentLayout::new(&config).unwrap();
        // SAFETY: as above.
        let header = unsafe { SegmentHeader::from_ptr(buffer.0.as_ptr()) };
        // SAFETY: as above.
        unsafe { header.initialize(&key(), &layout, &config, 1, 0) };

        assert_eq!(header.write_seq(), 0);
        header.publish_seq(5);
        assert_eq!(header.write_seq(), 5);
        assert_eq!(header.write_seq_relaxed(), 5);

        header.set_reclaimed_seq(3);
        assert_eq!(header.reclaimed_seq(), 3);
        // `fetch_max` semantics: a lower value must not move it backwards.
        header.set_reclaimed_seq(1);
        assert_eq!(header.reclaimed_seq(), 3);

        assert!(!header.is_closed());
        header.mark_closed();
        assert!(header.is_closed());
        header.mark_closed();
        assert!(header.is_closed());
    }

    #[test]
    fn consumer_counting_saturates_at_zero() {
        let buffer = scratch();
        let config = SegmentConfig::new(2, 128).unwrap();
        let layout = SegmentLayout::new(&config).unwrap();
        // SAFETY: as above.
        let header = unsafe { SegmentHeader::from_ptr(buffer.0.as_ptr()) };
        // SAFETY: as above.
        unsafe { header.initialize(&key(), &layout, &config, 1, 0) };

        assert_eq!(header.attached_consumers(), 0);
        assert_eq!(
            header.consumer_detached(),
            0,
            "detach below zero is a no-op"
        );
        assert_eq!(header.attached_consumers(), 0);

        assert_eq!(header.consumer_attached(), 0);
        assert_eq!(header.consumer_attached(), 1);
        assert_eq!(header.attached_consumers(), 2);
        assert_eq!(header.consumer_detached(), 2);
        assert_eq!(header.attached_consumers(), 1);

        let before = header.consumer_epoch();
        header.bump_consumer_epoch();
        assert!(header.consumer_epoch() > before);
    }

    #[test]
    fn heartbeat_never_moves_backwards() {
        let buffer = scratch();
        let config = SegmentConfig::new(2, 128).unwrap();
        let layout = SegmentLayout::new(&config).unwrap();
        // SAFETY: as above.
        let header = unsafe { SegmentHeader::from_ptr(buffer.0.as_ptr()) };
        // SAFETY: as above.
        unsafe { header.initialize(&key(), &layout, &config, 1, 100) };
        header.touch_producer(50);
        assert_eq!(header.producer_heartbeat(), 100);
        header.touch_producer(200);
        assert_eq!(header.producer_heartbeat(), 200);
    }

    #[test]
    fn fallback_metric_counts() {
        let buffer = scratch();
        let config = SegmentConfig::new(2, 128).unwrap();
        let layout = SegmentLayout::new(&config).unwrap();
        // SAFETY: as above.
        let header = unsafe { SegmentHeader::from_ptr(buffer.0.as_ptr()) };
        // SAFETY: as above.
        unsafe { header.initialize(&key(), &layout, &config, 1, 0) };
        assert_eq!(header.fallback_total(), 0);
        assert_eq!(header.record_fallback(), 0);
        assert_eq!(header.record_fallback(), 1);
        assert_eq!(header.fallback_total(), 2);
    }

    #[test]
    fn view_captures_and_computes_residency() {
        let buffer = scratch();
        let config = SegmentConfig::new(4, 256).unwrap();
        let layout = SegmentLayout::new(&config).unwrap();
        // SAFETY: as above.
        let header = unsafe { SegmentHeader::from_ptr(buffer.0.as_ptr()) };
        // SAFETY: as above.
        unsafe { header.initialize(&key(), &layout, &config, 9, 1) };
        header.publish_seq(10);
        header.set_reclaimed_seq(6);
        let view = SegmentHeaderView::capture(header);
        assert_eq!(view.write_seq, 10);
        assert_eq!(view.reclaimed_seq, 6);
        assert_eq!(view.resident(), 4);
        assert_eq!(view.producer_pid, 9);
        assert_eq!(view.slot_count, 4);
        assert_eq!(view.payload_capacity, 256);
        assert!(!view.closed);
    }

    #[test]
    fn producer_pid_can_be_corrected_after_spawn() {
        let buffer = scratch();
        let config = SegmentConfig::new(2, 128).unwrap();
        let layout = SegmentLayout::new(&config).unwrap();
        // SAFETY: as above.
        let header = unsafe { SegmentHeader::from_ptr(buffer.0.as_ptr()) };
        // SAFETY: as above.
        unsafe { header.initialize(&key(), &layout, &config, 1, 0) };
        header.set_producer_pid(7777);
        assert_eq!(header.producer_pid(), 7777);
    }
}
