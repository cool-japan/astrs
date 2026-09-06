//! The segment memory layout: offsets, alignment, and the validation that
//! makes a hostile header an error instead of a wild pointer.
//!
//! # The map
//!
//! ```text
//! offset 0                                                     total_len
//! ┌──────────┬────────────────┬───────────────────┬───────────────────────┐
//! │  header  │   slot table   │  consumer table   │      data slots       │
//! │  128 B   │ 64 B × slots   │ 64 B × consumers  │ slot_stride × slots   │
//! └──────────┴────────────────┴───────────────────┴───────────────────────┘
//!            ^128-aligned     ^128-aligned        ^128-aligned
//! ```
//!
//! and inside one data slot:
//!
//! ```text
//! ┌──────────────────────────────┬──────────────────────────┐
//! │ payload region               │ metadata region          │
//! │ align_up(payload_capacity,128)│ align_up(meta_capacity,128)│
//! └──────────────────────────────┴──────────────────────────┘
//! ^ 128-aligned (SIMD-usable Arrow buffer base, §6.1)
//!                                ^ 128-aligned
//! ```
//!
//! # Why 128 and not 64
//!
//! Blueprint §6.1 requires Arrow buffers to be 64-byte aligned and whole
//! messages in a slot to be **128-byte aligned**, "so a mapped payload is
//! directly usable as a SIMD source". 128 also happens to be the cache-line
//! size on Apple silicon, so a slot boundary is never a false-sharing
//! boundary on either P0 platform. Slot-table and consumer-table entries are
//! exactly 64 bytes — one x86-64/aarch64-Linux cache line each — so two
//! consumers updating their cursors never contend on the same line.
//!
//! # Why every offset is recomputed, never trusted
//!
//! A segment is *mapped*, not handshaken: there is no version negotiation on
//! the path from `mmap` to the first pointer arithmetic. A stale segment left
//! by a killed process, a truncated file, or a deliberately corrupted one can
//! all present a header claiming `slot_count = u32::MAX`. So
//! [`SegmentLayout::validate_header`] recomputes the *entire* geometry from
//! the four capacity numbers using `u64` checked arithmetic, compares the
//! result against every recorded offset, and finally checks that the whole
//! thing fits the number of bytes actually mapped. Only then does any code in
//! this crate form a pointer into the segment.
//!
//! # Examples
//!
//! ```
//! use astrs_shm::{SegmentConfig, SegmentLayout};
//!
//! let layout = SegmentLayout::new(&SegmentConfig::new(8, 4096)?)?;
//! assert_eq!(layout.header_offset(), 0);
//! assert_eq!(layout.slot_table_offset() % 128, 0);
//! assert_eq!(layout.data_offset() % 128, 0);
//! assert_eq!(layout.payload_offset(0) % 128, 0);
//! assert_eq!(layout.payload_offset(7) % 128, 0);
//! # Ok::<(), astrs_shm::ShmError>(())
//! ```

use crate::config::SegmentConfig;
use crate::error::{CorruptReason, ShmError, ShmResult};

/// The size of the segment header, in bytes (blueprint §6.2).
pub const HEADER_LEN: u64 = 128;

/// The size of one slot-table entry, in bytes — exactly one cache line.
pub const SLOT_ENTRY_LEN: u64 = 64;

/// The size of one consumer-table entry, in bytes — exactly one cache line.
pub const CONSUMER_ENTRY_LEN: u64 = 64;

/// The alignment every region boundary and every payload base satisfies.
pub const REGION_ALIGN: u64 = 128;

/// The magic at offset 0 of every AstRS segment.
pub const SEGMENT_MAGIC: [u8; 4] = *b"ASHM";

/// The layout version this build writes and accepts.
///
/// Bumped only for an incompatible change to the byte layout described in
/// this module. Unlike the wire protocol (§7.2) there is no negotiation: a
/// reader that does not recognise the version refuses the mapping.
pub const LAYOUT_VERSION: u16 = 1;

/// The largest segment the layout will describe: 4 GiB.
///
/// Well above the 256 MiB message cap times any sane slot count, and small
/// enough that a corrupt header cannot ask for a mapping that would succeed
/// on a 64-bit host and exhaust its address space.
pub const MAX_SEGMENT_LEN: u64 = 4 * 1024 * 1024 * 1024;

/// Round `value` up to the next multiple of `align`.
///
/// `align` must be a power of two; every caller in this crate passes a
/// compile-time constant.
///
/// # Examples
///
/// ```
/// use astrs_shm::align_up;
///
/// assert_eq!(align_up(0, 128), Some(0));
/// assert_eq!(align_up(1, 128), Some(128));
/// assert_eq!(align_up(128, 128), Some(128));
/// assert_eq!(align_up(u64::MAX, 128), None);
/// ```
#[must_use]
pub const fn align_up(value: u64, align: u64) -> Option<u64> {
    match value.checked_add(align - 1) {
        Some(sum) => Some(sum & !(align - 1)),
        None => None,
    }
}

/// The fully resolved byte geometry of one segment.
///
/// Every offset is an absolute byte offset from the start of the mapping.
/// All of them are derived — nothing in this struct is read from shared
/// memory without being recomputed and compared first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentLayout {
    slot_count: u32,
    payload_capacity: u32,
    meta_capacity: u32,
    max_consumers: u32,
    slot_table_offset: u64,
    consumer_table_offset: u64,
    data_offset: u64,
    payload_stride: u64,
    meta_stride: u64,
    slot_stride: u64,
    total_len: u64,
}

impl SegmentLayout {
    /// Resolve the geometry implied by a configuration.
    ///
    /// # Errors
    ///
    /// [`ShmError::InvalidConfig`] if the configuration is out of range, or
    /// [`ShmError::CorruptHeader`] with
    /// [`CorruptReason::GeometryOverflow`] / [`CorruptReason::GeometryTooLarge`]
    /// if the implied segment overflows [`MAX_SEGMENT_LEN`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::{SegmentConfig, SegmentLayout};
    ///
    /// let layout = SegmentLayout::new(&SegmentConfig::new(4, 1024)?)?;
    /// assert_eq!(layout.slot_count(), 4);
    /// assert!(layout.total_len() > 4 * 1024);
    /// # Ok::<(), astrs_shm::ShmError>(())
    /// ```
    pub fn new(config: &SegmentConfig) -> ShmResult<Self> {
        config.validate()?;
        Self::resolve(
            config.slot_count(),
            config.payload_capacity(),
            config.meta_capacity(),
            config.max_consumers(),
        )
    }

    /// Resolve the geometry from the four raw capacity numbers.
    ///
    /// This is the single arithmetic path: [`SegmentLayout::new`] and
    /// [`SegmentLayout::validate_header`] both go through it, so a header can
    /// never describe a geometry the creator would not have produced.
    ///
    /// # Errors
    ///
    /// [`ShmError::CorruptHeader`] if any product overflows or the total
    /// exceeds [`MAX_SEGMENT_LEN`].
    pub fn resolve(
        slot_count: u32,
        payload_capacity: u32,
        meta_capacity: u32,
        max_consumers: u32,
    ) -> ShmResult<Self> {
        if slot_count == 0 {
            return Err(corrupt(CorruptReason::ZeroGeometry {
                field: "slot_count",
            }));
        }
        if payload_capacity == 0 {
            return Err(corrupt(CorruptReason::ZeroGeometry {
                field: "payload_capacity",
            }));
        }
        if max_consumers == 0 {
            return Err(corrupt(CorruptReason::ZeroGeometry {
                field: "max_consumers",
            }));
        }
        if slot_count > crate::config::MAX_SLOT_COUNT {
            return Err(corrupt(CorruptReason::GeometryTooLarge {
                field: "slot_count",
                found: u64::from(slot_count),
                limit: u64::from(crate::config::MAX_SLOT_COUNT),
            }));
        }
        if payload_capacity > crate::config::MAX_PAYLOAD_CAPACITY {
            return Err(corrupt(CorruptReason::GeometryTooLarge {
                field: "payload_capacity",
                found: u64::from(payload_capacity),
                limit: u64::from(crate::config::MAX_PAYLOAD_CAPACITY),
            }));
        }
        if meta_capacity > crate::config::MAX_META_CAPACITY {
            return Err(corrupt(CorruptReason::GeometryTooLarge {
                field: "meta_capacity",
                found: u64::from(meta_capacity),
                limit: u64::from(crate::config::MAX_META_CAPACITY),
            }));
        }
        if max_consumers > crate::config::MAX_MAX_CONSUMERS {
            return Err(corrupt(CorruptReason::GeometryTooLarge {
                field: "max_consumers",
                found: u64::from(max_consumers),
                limit: u64::from(crate::config::MAX_MAX_CONSUMERS),
            }));
        }

        let slots = u64::from(slot_count);
        let consumers = u64::from(max_consumers);

        let slot_table_offset = HEADER_LEN;
        let slot_table_len = slots.checked_mul(SLOT_ENTRY_LEN).ok_or_else(|| {
            corrupt(CorruptReason::GeometryOverflow {
                field: "slot table",
            })
        })?;
        let slot_table_end = slot_table_offset
            .checked_add(slot_table_len)
            .ok_or_else(|| {
                corrupt(CorruptReason::GeometryOverflow {
                    field: "slot table",
                })
            })?;

        let consumer_table_offset = align_up(slot_table_end, REGION_ALIGN).ok_or_else(|| {
            corrupt(CorruptReason::GeometryOverflow {
                field: "consumer table",
            })
        })?;
        let consumer_table_len = consumers.checked_mul(CONSUMER_ENTRY_LEN).ok_or_else(|| {
            corrupt(CorruptReason::GeometryOverflow {
                field: "consumer table",
            })
        })?;
        let consumer_table_end = consumer_table_offset
            .checked_add(consumer_table_len)
            .ok_or_else(|| {
                corrupt(CorruptReason::GeometryOverflow {
                    field: "consumer table",
                })
            })?;

        let data_offset = align_up(consumer_table_end, REGION_ALIGN)
            .ok_or_else(|| corrupt(CorruptReason::GeometryOverflow { field: "data" }))?;

        let payload_stride = align_up(u64::from(payload_capacity), REGION_ALIGN)
            .ok_or_else(|| corrupt(CorruptReason::GeometryOverflow { field: "payload" }))?;
        let meta_stride = align_up(u64::from(meta_capacity), REGION_ALIGN)
            .ok_or_else(|| corrupt(CorruptReason::GeometryOverflow { field: "metadata" }))?;
        let slot_stride = payload_stride.checked_add(meta_stride).ok_or_else(|| {
            corrupt(CorruptReason::GeometryOverflow {
                field: "slot stride",
            })
        })?;
        let data_len = slots
            .checked_mul(slot_stride)
            .ok_or_else(|| corrupt(CorruptReason::GeometryOverflow { field: "data" }))?;
        let total_len = data_offset
            .checked_add(data_len)
            .ok_or_else(|| corrupt(CorruptReason::GeometryOverflow { field: "total" }))?;

        if total_len > MAX_SEGMENT_LEN {
            return Err(corrupt(CorruptReason::GeometryTooLarge {
                field: "total segment length",
                found: total_len,
                limit: MAX_SEGMENT_LEN,
            }));
        }

        Ok(Self {
            slot_count,
            payload_capacity,
            meta_capacity,
            max_consumers,
            slot_table_offset,
            consumer_table_offset,
            data_offset,
            payload_stride,
            meta_stride,
            slot_stride,
            total_len,
        })
    }

    /// The number of slots.
    #[must_use]
    pub const fn slot_count(&self) -> u32 {
        self.slot_count
    }

    /// The per-slot payload capacity, in bytes.
    #[must_use]
    pub const fn payload_capacity(&self) -> u32 {
        self.payload_capacity
    }

    /// The per-slot metadata capacity, in bytes.
    #[must_use]
    pub const fn meta_capacity(&self) -> u32 {
        self.meta_capacity
    }

    /// The consumer-table capacity.
    #[must_use]
    pub const fn max_consumers(&self) -> u32 {
        self.max_consumers
    }

    /// The header offset, which is always zero.
    #[must_use]
    pub const fn header_offset(&self) -> u64 {
        0
    }

    /// The byte offset of the slot table.
    #[must_use]
    pub const fn slot_table_offset(&self) -> u64 {
        self.slot_table_offset
    }

    /// The byte offset of the consumer table.
    #[must_use]
    pub const fn consumer_table_offset(&self) -> u64 {
        self.consumer_table_offset
    }

    /// The byte offset of the data region.
    #[must_use]
    pub const fn data_offset(&self) -> u64 {
        self.data_offset
    }

    /// The distance between two consecutive slots' data regions.
    #[must_use]
    pub const fn slot_stride(&self) -> u64 {
        self.slot_stride
    }

    /// The size of one slot's payload region (capacity rounded up).
    #[must_use]
    pub const fn payload_stride(&self) -> u64 {
        self.payload_stride
    }

    /// The size of one slot's metadata region (capacity rounded up).
    #[must_use]
    pub const fn meta_stride(&self) -> u64 {
        self.meta_stride
    }

    /// The total segment length in bytes.
    #[must_use]
    pub const fn total_len(&self) -> u64 {
        self.total_len
    }

    /// The byte offset of slot `index`'s table entry.
    ///
    /// Callers must have validated `index < slot_count`; every internal call
    /// site derives the index from `seq % slot_count`.
    #[must_use]
    pub const fn slot_entry_offset(&self, index: u32) -> u64 {
        self.slot_table_offset + (index as u64) * SLOT_ENTRY_LEN
    }

    /// The byte offset of consumer `index`'s table entry.
    #[must_use]
    pub const fn consumer_entry_offset(&self, index: u32) -> u64 {
        self.consumer_table_offset + (index as u64) * CONSUMER_ENTRY_LEN
    }

    /// The byte offset of slot `index`'s payload region.
    ///
    /// Always a multiple of [`REGION_ALIGN`] — the property the alignment
    /// probe in the test suite asserts, and the reason a mapped Arrow buffer
    /// is directly SIMD-usable (§6.1).
    #[must_use]
    pub const fn payload_offset(&self, index: u32) -> u64 {
        self.data_offset + (index as u64) * self.slot_stride
    }

    /// The byte offset of slot `index`'s metadata region.
    #[must_use]
    pub const fn meta_offset(&self, index: u32) -> u64 {
        self.payload_offset(index) + self.payload_stride
    }

    /// The slot index that carries sequence number `seq`.
    ///
    /// Sequence numbers are 1-based (`0` means "never published"), so seq `1`
    /// lives in slot `0`. The ring discipline — index is a pure function of
    /// the sequence number — is what makes a consumer's lookup O(1) with no
    /// scan and no shared index structure.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::{SegmentConfig, SegmentLayout};
    ///
    /// let layout = SegmentLayout::new(&SegmentConfig::new(4, 128)?)?;
    /// assert_eq!(layout.slot_index_for_seq(1), 0);
    /// assert_eq!(layout.slot_index_for_seq(4), 3);
    /// assert_eq!(layout.slot_index_for_seq(5), 0);
    /// # Ok::<(), astrs_shm::ShmError>(())
    /// ```
    #[must_use]
    pub const fn slot_index_for_seq(&self, seq: u64) -> u32 {
        // `seq` is 1-based; `seq == 0` never reaches a slot lookup, but map
        // it to slot 0 rather than underflowing.
        let zero_based = seq.wrapping_sub(1);
        (zero_based % (self.slot_count as u64)) as u32
    }

    /// Validate a header's self-described geometry against this build's
    /// expectations and the size actually mapped.
    ///
    /// The order matters and is deliberate: magic, then version, then
    /// `header_len`, then geometry, then offsets, then total size. Each step
    /// only trusts what the previous step established.
    ///
    /// # Errors
    ///
    /// - [`ShmError::BadMagic`] — the bytes are not an AstRS segment.
    /// - [`ShmError::LayoutVersion`] — a future (or ancient) layout.
    /// - [`ShmError::CorruptHeader`] — anything inconsistent thereafter.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::{HeaderFields, SegmentConfig, SegmentLayout};
    ///
    /// let config = SegmentConfig::new(4, 1024)?;
    /// let layout = SegmentLayout::new(&config)?;
    /// let fields = HeaderFields::from_layout(&layout);
    /// let validated = SegmentLayout::validate_header(&fields, layout.total_len() as usize)?;
    /// assert_eq!(validated, layout);
    /// # Ok::<(), astrs_shm::ShmError>(())
    /// ```
    pub fn validate_header(fields: &HeaderFields, mapped_len: usize) -> ShmResult<Self> {
        if fields.magic != SEGMENT_MAGIC {
            return Err(ShmError::BadMagic {
                expected: SEGMENT_MAGIC,
                found: fields.magic,
            });
        }
        if fields.layout_ver != LAYOUT_VERSION {
            return Err(ShmError::LayoutVersion {
                found: fields.layout_ver,
                supported: LAYOUT_VERSION,
            });
        }
        let mapped = mapped_len as u64;
        if mapped < HEADER_LEN {
            return Err(corrupt(CorruptReason::MappingTooSmall {
                mapped: mapped_len,
                required: HEADER_LEN as usize,
            }));
        }
        if u64::from(fields.header_len) != HEADER_LEN {
            return Err(corrupt(CorruptReason::HeaderLen {
                found: fields.header_len,
                expected: HEADER_LEN as u16,
            }));
        }

        let layout = Self::resolve(
            fields.slot_count,
            fields.payload_capacity,
            fields.meta_capacity,
            fields.max_consumers,
        )?;

        check_offset(
            "slot table",
            fields.slot_table_offset,
            layout.slot_table_offset,
        )?;
        check_offset(
            "consumer table",
            fields.consumer_table_offset,
            layout.consumer_table_offset,
        )?;
        check_offset("data", fields.data_offset, layout.data_offset)?;
        if fields.total_len != layout.total_len {
            return Err(corrupt(CorruptReason::OffsetMismatch {
                region: "total length",
                recorded: fields.total_len,
                computed: layout.total_len,
            }));
        }

        check_align("slot table", layout.slot_table_offset)?;
        check_align("consumer table", layout.consumer_table_offset)?;
        check_align("data", layout.data_offset)?;
        check_align("slot stride", layout.slot_stride)?;

        if layout.total_len > mapped {
            return Err(corrupt(CorruptReason::TotalLenMismatch {
                described: layout.total_len,
                mapped,
            }));
        }

        Ok(layout)
    }
}

/// The subset of header fields the layout validator reads.
///
/// Decoupled from the mapped [`crate::header::SegmentHeader`] so the
/// validation logic is exercisable — and property-testable — without a real
/// mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderFields {
    /// The four magic bytes at offset 0.
    pub magic: [u8; 4],
    /// The layout version.
    pub layout_ver: u16,
    /// The header length, which this version fixes at [`HEADER_LEN`].
    pub header_len: u16,
    /// The number of slots.
    pub slot_count: u32,
    /// The per-slot payload capacity.
    pub payload_capacity: u32,
    /// The per-slot metadata capacity.
    pub meta_capacity: u32,
    /// The consumer-table capacity.
    pub max_consumers: u32,
    /// The recorded slot-table offset.
    pub slot_table_offset: u64,
    /// The recorded consumer-table offset.
    pub consumer_table_offset: u64,
    /// The recorded data-region offset.
    pub data_offset: u64,
    /// The recorded total length.
    pub total_len: u64,
}

impl HeaderFields {
    /// The fields a freshly created segment with this layout would carry.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_shm::{HeaderFields, SegmentConfig, SegmentLayout};
    ///
    /// let layout = SegmentLayout::new(&SegmentConfig::new(2, 256)?)?;
    /// let fields = HeaderFields::from_layout(&layout);
    /// assert_eq!(fields.slot_count, 2);
    /// assert_eq!(fields.header_len, 128);
    /// # Ok::<(), astrs_shm::ShmError>(())
    /// ```
    #[must_use]
    pub const fn from_layout(layout: &SegmentLayout) -> Self {
        Self {
            magic: SEGMENT_MAGIC,
            layout_ver: LAYOUT_VERSION,
            header_len: HEADER_LEN as u16,
            slot_count: layout.slot_count,
            payload_capacity: layout.payload_capacity,
            meta_capacity: layout.meta_capacity,
            max_consumers: layout.max_consumers,
            slot_table_offset: layout.slot_table_offset,
            consumer_table_offset: layout.consumer_table_offset,
            data_offset: layout.data_offset,
            total_len: layout.total_len,
        }
    }
}

fn corrupt(reason: CorruptReason) -> ShmError {
    ShmError::CorruptHeader { reason }
}

fn check_offset(region: &'static str, recorded: u64, computed: u64) -> ShmResult<()> {
    if recorded == computed {
        Ok(())
    } else {
        Err(corrupt(CorruptReason::OffsetMismatch {
            region,
            recorded,
            computed,
        }))
    }
}

fn check_align(region: &'static str, offset: u64) -> ShmResult<()> {
    if offset.is_multiple_of(REGION_ALIGN) {
        Ok(())
    } else {
        Err(corrupt(CorruptReason::Misaligned {
            region,
            offset,
            align: REGION_ALIGN,
        }))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::SegmentConfig;
    use proptest::prelude::*;

    fn layout(slots: u32, payload: u32) -> SegmentLayout {
        SegmentLayout::new(&SegmentConfig::new(slots, payload).unwrap()).unwrap()
    }

    #[test]
    fn align_up_is_exact_and_saturation_free() {
        assert_eq!(align_up(0, 128), Some(0));
        assert_eq!(align_up(1, 128), Some(128));
        assert_eq!(align_up(127, 128), Some(128));
        assert_eq!(align_up(128, 128), Some(128));
        assert_eq!(align_up(129, 128), Some(256));
        assert_eq!(align_up(u64::MAX, 128), None);
        assert_eq!(align_up(u64::MAX - 126, 128), None);
    }

    #[test]
    fn every_region_is_128_byte_aligned() {
        for slots in [1u32, 2, 3, 5, 8, 13, 64, 1000] {
            for payload in [1u32, 63, 64, 128, 129, 4096, 65_536] {
                let layout = layout(slots, payload);
                assert_eq!(layout.slot_table_offset() % 128, 0);
                assert_eq!(layout.consumer_table_offset() % 128, 0);
                assert_eq!(layout.data_offset() % 128, 0);
                assert_eq!(layout.slot_stride() % 128, 0);
                for index in 0..slots {
                    assert_eq!(layout.payload_offset(index) % 128, 0);
                    assert_eq!(layout.meta_offset(index) % 128, 0);
                    assert_eq!(layout.slot_entry_offset(index) % 64, 0);
                }
            }
        }
    }

    #[test]
    fn regions_do_not_overlap_and_fit_the_total() {
        let layout = layout(16, 4096);
        let slot_table_end = layout.slot_table_offset() + 16 * SLOT_ENTRY_LEN;
        assert!(slot_table_end <= layout.consumer_table_offset());
        let consumer_end =
            layout.consumer_table_offset() + u64::from(layout.max_consumers()) * CONSUMER_ENTRY_LEN;
        assert!(consumer_end <= layout.data_offset());
        let data_end = layout.data_offset() + 16 * layout.slot_stride();
        assert_eq!(data_end, layout.total_len());
        for index in 0..16 {
            let payload_end = layout.payload_offset(index) + layout.payload_stride();
            assert_eq!(payload_end, layout.meta_offset(index));
            assert!(layout.meta_offset(index) + layout.meta_stride() <= layout.total_len());
        }
    }

    #[test]
    fn slot_index_wraps_with_one_based_sequences() {
        let layout = layout(4, 128);
        assert_eq!(layout.slot_index_for_seq(1), 0);
        assert_eq!(layout.slot_index_for_seq(2), 1);
        assert_eq!(layout.slot_index_for_seq(4), 3);
        assert_eq!(layout.slot_index_for_seq(5), 0);
        assert_eq!(
            layout.slot_index_for_seq(u64::MAX),
            ((u64::MAX - 1) % 4) as u32
        );
        // Defensive: seq 0 must not underflow into a wild index.
        assert!(layout.slot_index_for_seq(0) < 4);
    }

    #[test]
    fn a_well_formed_header_validates_to_the_same_layout() {
        let layout = layout(8, 4096);
        let fields = HeaderFields::from_layout(&layout);
        let validated =
            SegmentLayout::validate_header(&fields, layout.total_len() as usize).unwrap();
        assert_eq!(validated, layout);
    }

    #[test]
    fn hostile_headers_are_rejected_not_dereferenced() {
        let good = layout(8, 4096);
        let base = HeaderFields::from_layout(&good);
        let mapped = good.total_len() as usize;

        let mut bad = base;
        bad.magic = *b"NOPE";
        assert!(matches!(
            SegmentLayout::validate_header(&bad, mapped),
            Err(ShmError::BadMagic { .. })
        ));

        let mut bad = base;
        bad.layout_ver = LAYOUT_VERSION + 1;
        assert!(matches!(
            SegmentLayout::validate_header(&bad, mapped),
            Err(ShmError::LayoutVersion { .. })
        ));

        let mut bad = base;
        bad.header_len = 64;
        assert!(matches!(
            SegmentLayout::validate_header(&bad, mapped),
            Err(ShmError::CorruptHeader {
                reason: CorruptReason::HeaderLen { .. }
            })
        ));

        // The headline case: a slot count that would compute wild pointers.
        let mut bad = base;
        bad.slot_count = u32::MAX;
        assert!(matches!(
            SegmentLayout::validate_header(&bad, mapped),
            Err(ShmError::CorruptHeader {
                reason: CorruptReason::GeometryTooLarge { .. }
            })
        ));

        let mut bad = base;
        bad.payload_capacity = 0;
        assert!(matches!(
            SegmentLayout::validate_header(&bad, mapped),
            Err(ShmError::CorruptHeader {
                reason: CorruptReason::ZeroGeometry { .. }
            })
        ));

        let mut bad = base;
        bad.data_offset += 128;
        assert!(matches!(
            SegmentLayout::validate_header(&bad, mapped),
            Err(ShmError::CorruptHeader {
                reason: CorruptReason::OffsetMismatch { region: "data", .. }
            })
        ));

        let mut bad = base;
        bad.total_len += 128;
        assert!(matches!(
            SegmentLayout::validate_header(&bad, mapped),
            Err(ShmError::CorruptHeader {
                reason: CorruptReason::OffsetMismatch { .. }
            })
        ));

        // A truncated mapping: the geometry is self-consistent but does not
        // fit what was actually mapped.
        assert!(matches!(
            SegmentLayout::validate_header(&base, mapped - 128),
            Err(ShmError::CorruptHeader {
                reason: CorruptReason::TotalLenMismatch { .. }
            })
        ));

        assert!(matches!(
            SegmentLayout::validate_header(&base, 8),
            Err(ShmError::CorruptHeader {
                reason: CorruptReason::MappingTooSmall { .. }
            })
        ));
    }

    #[test]
    fn oversized_geometry_is_capped_before_it_overflows() {
        let err = SegmentLayout::resolve(
            crate::config::MAX_SLOT_COUNT,
            crate::config::MAX_PAYLOAD_CAPACITY,
            0,
            1,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ShmError::CorruptHeader {
                reason: CorruptReason::GeometryTooLarge {
                    field: "total segment length",
                    ..
                }
            }
        ));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// No combination of in-range capacities may produce overlapping
        /// regions, a misaligned payload base, or a total that disagrees with
        /// the sum of the parts.
        #[test]
        fn resolve_is_internally_consistent(
            slots in 1u32..=512,
            payload in 1u32..=(1 << 20),
            meta in 0u32..=4096,
            consumers in 1u32..=64,
        ) {
            let layout = SegmentLayout::resolve(slots, payload, meta, consumers)?;
            prop_assert_eq!(layout.data_offset() % REGION_ALIGN, 0);
            prop_assert_eq!(layout.slot_stride() % REGION_ALIGN, 0);
            prop_assert!(
                layout.slot_table_offset() + u64::from(slots) * SLOT_ENTRY_LEN
                    <= layout.consumer_table_offset()
            );
            prop_assert!(
                layout.consumer_table_offset() + u64::from(consumers) * CONSUMER_ENTRY_LEN
                    <= layout.data_offset()
            );
            prop_assert_eq!(
                layout.data_offset() + u64::from(slots) * layout.slot_stride(),
                layout.total_len()
            );
            prop_assert!(u64::from(payload) <= layout.payload_stride());
            prop_assert!(u64::from(meta) <= layout.meta_stride());
        }

        /// Every sequence number maps into range, and the mapping is a
        /// bijection over any window of `slot_count` consecutive sequences.
        #[test]
        fn slot_index_is_a_ring_bijection(slots in 1u32..=64, start in 1u64..u64::MAX / 2) {
            let layout = SegmentLayout::resolve(slots, 128, 0, 1)?;
            let mut seen = vec![false; slots as usize];
            for offset in 0..u64::from(slots) {
                let index = layout.slot_index_for_seq(start + offset);
                prop_assert!(index < slots);
                prop_assert!(!seen[index as usize]);
                seen[index as usize] = true;
            }
            prop_assert!(seen.into_iter().all(|hit| hit));
        }

        /// Round-tripping a valid layout through the header fields must
        /// always validate; perturbing any single number must not silently
        /// produce a *different but accepted* layout.
        #[test]
        fn header_round_trip_and_perturbation(
            slots in 1u32..=256,
            payload in 128u32..=(1 << 16),
            meta in 0u32..=1024,
            consumers in 1u32..=32,
            perturb in 1u64..=4096,
        ) {
            let layout = SegmentLayout::resolve(slots, payload, meta, consumers)?;
            let fields = HeaderFields::from_layout(&layout);
            let mapped = usize::try_from(layout.total_len()).unwrap_or(usize::MAX);
            prop_assert_eq!(SegmentLayout::validate_header(&fields, mapped)?, layout);

            let mut bad = fields;
            bad.data_offset = bad.data_offset.wrapping_add(perturb);
            prop_assert!(SegmentLayout::validate_header(&bad, mapped).is_err());

            let mut bad = fields;
            bad.total_len = bad.total_len.wrapping_add(perturb);
            prop_assert!(SegmentLayout::validate_header(&bad, mapped).is_err());
        }
    }
}
