//! Frame codec micro-benchmarks — encode/decode throughput, small and large
//! payloads, with and without the CRC-32C trailer (blueprint §7.1).
//!
//! **Not a §20.4 latency-ladder gate.** None of the six blueprint §20.4
//! rows names `astrs-wire` directly (the codec's cost shows up inside the
//! SHM/transport/RTPS end-to-end numbers instead); this file has no target
//! and prints no `BENCH_GATE` line — `scripts/bench-gate.sh`'s "exactly six
//! lines" count deliberately does not include it. Its purpose is narrower:
//! it is the **before/after evidence a SIMD-gating decision on the CRC-32C
//! and copy paths needs** (blueprint §20.1 W6 hardening policy) — run this
//! file, change the kernel, run it again, compare.
//!
//! Each `bench_function` times exactly one `Frame::encode`/`decode_frame`
//! call; the [`criterion::Frame`]/payload each variant needs is built once,
//! outside the timed closure, so construction and cloning never leak into
//! the reported number. `Throughput::Bytes` is set for every case, so
//! criterion's own report includes MB/s alongside time.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;

use astrs_wire::{Frame, FrameFlags, FrameKind, FrameLimits, decode_frame};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};

/// A small control-message-sized payload.
const SMALL_LEN: usize = 64;

/// A large data-plane-sized payload — big enough that a CRC-32C or copy
/// kernel's throughput (not its fixed per-call overhead) dominates.
const LARGE_LEN: usize = 1024 * 1024;

/// A deterministic, non-zero fill so a CRC actually has varying bits to
/// checksum rather than a degenerate all-zero buffer.
fn filler(len: usize) -> Vec<u8> {
    (0..len).map(|index| (index % 251) as u8).collect()
}

/// Registers one encode and one decode benchmark for `(label, len, flags)`.
fn bench_one(c: &mut Criterion, label: &str, len: usize, flags: FrameFlags) {
    let limits = FrameLimits::default();
    let payload = filler(len);
    let frame = Frame::new(FrameKind::Data, flags, payload).expect("a valid frame");
    let encoded = frame
        .encode(&limits)
        .expect("encode for the decode fixture");

    let mut group = c.benchmark_group("wire_frame_codec");
    group.throughput(Throughput::Bytes(len as u64));

    group.bench_function(format!("encode_{label}"), |b| {
        b.iter(|| black_box(frame.encode(&limits).expect("encode")));
    });
    group.bench_function(format!("decode_{label}"), |b| {
        b.iter(|| black_box(decode_frame(black_box(&encoded), &limits).expect("decode")));
    });

    group.finish();
}

/// Small (64 B, control-message-sized) frames, no CRC-32C trailer.
fn bench_small_no_crc(c: &mut Criterion) {
    bench_one(c, "64b_no_crc", SMALL_LEN, FrameFlags::EMPTY);
}

/// Small (64 B) frames, with the CRC-32C trailer — the network-leg default
/// (blueprint §7.1: "CRC mandatory on network legs").
fn bench_small_crc(c: &mut Criterion) {
    bench_one(c, "64b_crc", SMALL_LEN, FrameFlags::CRC);
}

/// Large (1 MiB, data-plane-sized) frames, no CRC-32C trailer.
fn bench_large_no_crc(c: &mut Criterion) {
    bench_one(c, "1mb_no_crc", LARGE_LEN, FrameFlags::EMPTY);
}

/// Large (1 MiB) frames, with the CRC-32C trailer — this is the pair that
/// isolates the checksum kernel's own throughput: compare against
/// `1mb_no_crc` for the CRC-32C cost alone.
fn bench_large_crc(c: &mut Criterion) {
    bench_one(c, "1mb_crc", LARGE_LEN, FrameFlags::CRC);
}

criterion_group!(
    benches,
    bench_small_no_crc,
    bench_small_crc,
    bench_large_no_crc,
    bench_large_crc
);
criterion_main!(benches);
