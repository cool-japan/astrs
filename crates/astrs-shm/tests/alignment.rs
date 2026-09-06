// `missing_docs` (workspace lint) would otherwise fire on a non-unix target:
// `#![cfg(unix)]` below makes this whole crate empty there, which strips the
// module doc comment along with everything else, so this `allow` has to sit
// ahead of that line to survive the stripping.
#![cfg_attr(not(unix), allow(missing_docs))]
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Alignment probes.
//!
//! Blueprint §6.1 makes two promises about a mapped payload, and both are
//! load-bearing for the layer above:
//!
//! - **Payload bases are 128-byte aligned.** "Whole messages are 128-byte
//!   aligned in SHM slots so a mapped payload is directly usable as a SIMD
//!   source." A consumer that has to memcpy a frame into an aligned buffer
//!   before running a kernel over it has lost the zero-copy plane's entire
//!   point.
//! - **Slot-table entries are 64-byte aligned.** One cache line each, so two
//!   consumers pinning different slots never false-share the line that
//!   carries the gate they are CAS-ing.
//!
//! These are cheap properties to state and easy to break silently — a field
//! added to the header, a capacity that stops being rounded — so they get
//! their own file, checked across a wide sweep of geometries and through
//! every path a caller can reach a pointer by.

use std::sync::Arc;

use astrs_shm::{
    AttachOptions, CONSUMER_ENTRY_LEN, Consumer, HEADER_LEN, Producer, REGION_ALIGN,
    SLOT_ENTRY_LEN, Segment, SegmentConfig, SegmentKey,
};
use astrs_wire::DataflowId;

fn ring(slots: u32, payload: u32, meta: u32, consumers: u32) -> Arc<Segment> {
    let key = SegmentKey::from_parts(DataflowId::generate(), "align", "out", 1).expect("valid ids");
    let config = SegmentConfig::new(slots, payload)
        .expect("valid geometry")
        .with_meta_capacity(meta)
        .expect("valid metadata capacity")
        .with_max_consumers(consumers)
        .expect("valid consumer table");
    Segment::create_shared(key, config).expect("segment")
}

#[test]
fn slot_pointers_and_payload_bases_are_aligned_across_a_geometry_sweep() {
    // Awkward capacities on purpose: primes, off-by-ones around the 64 and
    // 128 boundaries, and a slot count that is not a power of two.
    for slots in [1u32, 2, 3, 5, 7, 8, 17, 64] {
        for payload in [1u32, 63, 64, 65, 127, 128, 129, 1000, 4096, 65_537] {
            for meta in [0u32, 1, 63, 64, 512, 4095] {
                let segment = ring(slots, payload, meta, 3);
                let layout = segment.layout();

                assert_eq!(layout.header_offset(), 0);
                assert_eq!(layout.slot_table_offset(), HEADER_LEN);
                assert_eq!(layout.consumer_table_offset() % REGION_ALIGN, 0);
                assert_eq!(layout.data_offset() % REGION_ALIGN, 0);
                assert_eq!(layout.slot_stride() % REGION_ALIGN, 0);

                for index in 0..slots {
                    let slot = std::ptr::from_ref(segment.slot(index)) as usize;
                    assert_eq!(
                        slot % SLOT_ENTRY_LEN as usize,
                        0,
                        "slot {index} of a {slots}×{payload}/{meta} ring is not cache-line aligned"
                    );
                    assert_eq!(
                        layout.payload_offset(index) % REGION_ALIGN,
                        0,
                        "payload base {index} of a {slots}×{payload}/{meta} ring is misaligned"
                    );
                    assert_eq!(layout.meta_offset(index) % REGION_ALIGN, 0);
                }

                for index in 0..segment.layout().max_consumers() {
                    let entry = std::ptr::from_ref(segment.consumer_entry(index)) as usize;
                    assert_eq!(entry % CONSUMER_ENTRY_LEN as usize, 0);
                }
            }
        }
    }
}

#[test]
fn write_windows_and_read_views_agree_on_a_128_byte_base() {
    let segment = ring(9, 3000, 300, 4);
    let mut producer = Producer::new(Arc::clone(&segment)).expect("producer");
    let mut consumer =
        Consumer::attach(Arc::clone(&segment), AttachOptions::default()).expect("attach");

    for len in [1usize, 2, 63, 64, 127, 128, 129, 999, 3000] {
        let mut window = producer.allocate(len).expect("allocate");
        let write_base = window.as_mut_slice().as_ptr() as usize;
        assert_eq!(
            write_base % REGION_ALIGN as usize,
            0,
            "the write window for {len} bytes is misaligned"
        );
        window.as_mut_slice().fill(0x5a);
        let seq = window.commit(b"m").expect("commit");

        let sample = consumer.try_next().expect("receive");
        assert_eq!(sample.seq(), seq);
        assert_eq!(
            sample.payload_address() % REGION_ALIGN as usize,
            0,
            "the read view for {len} bytes is misaligned"
        );
        // The reader sees exactly the bytes the writer placed, at the same
        // address within this process.
        assert_eq!(sample.payload_address(), write_base);
        assert_eq!(sample.payload().len(), len);
        assert!(sample.payload().iter().all(|byte| *byte == 0x5a));
        assert_eq!(
            sample.metadata().as_ptr() as usize % REGION_ALIGN as usize,
            0,
            "the metadata region is misaligned"
        );
    }
}

#[test]
fn a_separately_mapped_consumer_still_sees_an_aligned_payload() {
    // The product claim is about a *different mapping* — a consumer in
    // another process — not about the producer's own view. `mmap` is
    // page-aligned, so the region offsets are what carry the guarantee.
    let segment = ring(5, 5000, 64, 2);
    let mut producer = Producer::new(Arc::clone(&segment)).expect("producer");

    let attached = Segment::open_fd(segment.try_clone_fd().expect("dup"), segment.key())
        .expect("attach by descriptor")
        .shared();
    let mut consumer =
        Consumer::attach(Arc::clone(&attached), AttachOptions::default()).expect("attach");

    for seq in 1..=10u64 {
        let mut window = producer.allocate(4321).expect("allocate");
        window.as_mut_slice().fill(seq as u8);
        window.commit(b"").expect("commit");

        let sample = consumer.try_next().expect("receive");
        assert_eq!(sample.seq(), seq);
        assert_eq!(
            sample.payload_address() % REGION_ALIGN as usize,
            0,
            "sequence {seq} is misaligned in the second mapping"
        );
        assert!(sample.payload().iter().all(|byte| *byte == seq as u8));
    }
}

#[test]
fn the_mapping_base_is_page_aligned_and_the_header_sits_at_offset_zero() {
    let segment = ring(4, 1024, 128, 2);
    let base = std::ptr::from_ref(segment.header()) as usize;
    assert_eq!(
        base % REGION_ALIGN as usize,
        0,
        "the header must be at least 128-byte aligned"
    );
    // Every region offset is measured from the header, so the header being
    // the mapping base is what makes the whole layout's alignment claims
    // follow from `mmap`'s page alignment.
    let slot_zero = std::ptr::from_ref(segment.slot(0)) as usize;
    assert_eq!(slot_zero - base, HEADER_LEN as usize);
}

#[test]
fn the_default_pool_geometry_is_aligned_too() {
    // The path a manifest with no explicit sizing takes (§24.2: 8 MiB).
    let key =
        SegmentKey::from_parts(DataflowId::generate(), "default", "out", 1).expect("valid ids");
    let config = SegmentConfig::from_pool_size(8 * 1024 * 1024).expect("pool geometry");
    let segment = Segment::create_shared(key, config).expect("segment");
    let layout = segment.layout();

    assert_eq!(layout.data_offset() % REGION_ALIGN, 0);
    for index in 0..layout.slot_count() {
        assert_eq!(layout.payload_offset(index) % REGION_ALIGN, 0);
        assert_eq!(
            std::ptr::from_ref(segment.slot(index)) as usize % SLOT_ENTRY_LEN as usize,
            0
        );
    }
    assert!(layout.total_len() <= 8 * 1024 * 1024);
}

#[test]
fn the_control_structures_are_exactly_one_cache_line_each() {
    // Documented in `layout`, asserted in the modules' `const _` blocks, and
    // re-asserted here so a change shows up as a failing integration test and
    // not only as a compile error inside the crate.
    assert_eq!(SLOT_ENTRY_LEN, 64);
    assert_eq!(CONSUMER_ENTRY_LEN, 64);
    assert_eq!(HEADER_LEN, 128);
    assert_eq!(REGION_ALIGN, 128);

    let segment = ring(4, 256, 64, 4);
    let first = std::ptr::from_ref(segment.slot(0)) as usize;
    let second = std::ptr::from_ref(segment.slot(1)) as usize;
    assert_eq!(
        second - first,
        SLOT_ENTRY_LEN as usize,
        "adjacent slot entries must be exactly one cache line apart"
    );
    let first = std::ptr::from_ref(segment.consumer_entry(0)) as usize;
    let second = std::ptr::from_ref(segment.consumer_entry(1)) as usize;
    assert_eq!(second - first, CONSUMER_ENTRY_LEN as usize);
}
