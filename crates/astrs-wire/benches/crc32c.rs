//! CRC-32C micro-benchmarks — the checksum kernel itself, isolated from
//! frame encode/decode (blueprint §7.1, §20.1 W6 SIMD hardening).
//!
//! **Not a §20.4 latency-ladder gate.** None of the six blueprint §20.4 rows
//! names `astrs-wire` directly; this file has no target and prints no
//! `BENCH_GATE` line — `scripts/bench-gate.sh`'s "exactly six lines" count
//! deliberately does not include it. Its purpose is narrower: it is the
//! **before/after evidence the CRC-32C hardware-dispatch change needs**
//! (blueprint §20.1 W6 SIMD policy) — run this file against the portable
//! slice-by-8 implementation, apply the SSE4.2/aarch64-crc dispatch, run it
//! again, compare.
//!
//! Sweeps the function directly across sizes from an empty buffer up to
//! 1 MiB, including every size actually seen on the hot path — `HEADER_LEN`
//! (a frame's fixed header) is 10 bytes, and `Crc32c::new()`
//! `.chain(header).chain(payload).finalize()` runs on every framed message —
//! because a hardware CRC instruction is a real, non-inlined call
//! (`#[target_feature]` functions do not inline into a generic caller), so a
//! win at 1 MiB says nothing about whether the same change is a loss at 10
//! bytes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;

use astrs_wire::crc32c::crc32c;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

/// A deterministic, non-zero fill — the same small LCG this crate's own
/// `crc32c` tests use (`matches_the_bitwise_reference_on_a_large_buffer`),
/// so a buffer here and a proptest buffer there need no `rand` dependency
/// for what is, either way, just "some bytes that are not all zero".
fn filled(len: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(len);
    let mut x: u32 = 0x1234_5678;
    for _ in 0..len {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        data.push((x >> 24) as u8);
    }
    data
}

/// Sweeps `crc32c` across sizes from empty up to 1 MiB.
///
/// The low end (0..=64 bytes) is the actual hot path: a bare 10-byte frame
/// header, a small control message. The high end (4 KiB..1 MiB) is where
/// hardware throughput — and, on the architecture where it measures out
/// ahead, 3-way stream interleaving — is meant to pay off.
fn bench_crc32c(c: &mut Criterion) {
    let mut group = c.benchmark_group("crc32c");
    for &len in &[
        0usize, 1, 4, 8, 10, 16, 32, 64, 256, 1024, 4096, 16384, 65536, 1_048_576,
    ] {
        let data = filled(len);
        group.throughput(Throughput::Bytes(len as u64));
        group.bench_with_input(BenchmarkId::from_parameter(len), &data, |b, data| {
            b.iter(|| crc32c(black_box(data)));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_crc32c);
criterion_main!(benches);
