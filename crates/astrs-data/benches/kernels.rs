//! Compute kernel micro-benchmarks — `concat`/`cast`/`filter`/`take` on a
//! 1M-row primitive array, plus validity-bitmap `AND`/`OR` (blueprint §5.2's
//! "compute kernels (slice/concat/cast)" line, and §6.1's 64-byte-aligned
//! buffers).
//!
//! **Not a §20.4 latency-ladder gate.** None of the six blueprint §20.4 rows
//! names `astrs-data` directly; this file has no target and prints no
//! `BENCH_GATE` line — `scripts/bench-gate.sh`'s "exactly six lines" count
//! deliberately does not include it. Its purpose is narrower: it is the
//! **before/after evidence a SIMD-gating decision on these kernels needs**
//! (blueprint §20.1 W6 hardening policy) — run this file, change a kernel,
//! run it again, compare. `Throughput::Elements` is set on every case, so
//! criterion's own report includes rows/s alongside time.
//!
//! Every fixture array/bitmap/mask is built once, outside the timed
//! closures — construction cost never leaks into a reported number.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;

use astrs_data::BitmapBuilder;
use astrs_data::prelude::*;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};

/// Row count every kernel in this file is measured at.
const ROWS: usize = 1_000_000;

/// A 1M-row `Int32` array, `0..ROWS`.
fn make_array() -> ArrayRef {
    Int32Array::from_values(0..ROWS as i32).into_array_ref()
}

/// A 1M-entry mask keeping two rows out of three.
fn make_mask() -> BooleanArray {
    BooleanArray::from_values((0..ROWS).map(|index| index % 3 != 0))
}

/// A 1M-entry, fully-reversed index array — every kernel's least
/// cache-friendly access pattern, and the one worth benchmarking.
fn make_reversed_indices() -> ArrayRef {
    Int32Array::from_values((0..ROWS as i32).rev()).into_array_ref()
}

/// A tiny xorshift64* generator — the retained-crate list (§18.1) has no RNG
/// in it, and a bitmap benchmark only needs a non-degenerate bit pattern,
/// not statistical quality.
struct Rng(u64);

impl Rng {
    fn next_bit(&mut self) -> bool {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x & 1 == 1
    }
}

/// A 1M-bit [`Bitmap`] seeded from `seed`.
fn make_bitmap(seed: u64) -> Bitmap {
    let mut rng = Rng(seed | 1);
    let mut builder = BitmapBuilder::with_capacity(ROWS);
    for _ in 0..ROWS {
        builder.append(rng.next_bit());
    }
    builder.finish()
}

/// `concat` — two 500k-row halves rejoined into one 1M-row array.
fn bench_concat_1m(c: &mut Criterion) {
    let left = Int32Array::from_values(0..(ROWS / 2) as i32).into_array_ref();
    let right = Int32Array::from_values((ROWS / 2) as i32..ROWS as i32).into_array_ref();

    let mut group = c.benchmark_group("data_kernels");
    group.throughput(Throughput::Elements(ROWS as u64));
    group.bench_function("concat_1m_i32", |b| {
        b.iter(|| black_box(concat(&[left.clone(), right.clone()]).expect("concat")));
    });
    group.finish();
}

/// `cast` — `Int32` to `Float64` (blueprint §5.2's compute-kernel row),
/// saturating on overflow (never triggers here, but it is the policy a real
/// caller pays for).
fn bench_cast_1m(c: &mut Criterion) {
    let array = make_array();

    let mut group = c.benchmark_group("data_kernels");
    group.throughput(Throughput::Elements(ROWS as u64));
    group.bench_function("cast_1m_i32_to_f64", |b| {
        b.iter(|| {
            black_box(cast(&array, &DataType::Float64, OverflowPolicy::Saturate).expect("cast"))
        });
    });
    group.finish();
}

/// `filter` — a two-thirds-keep boolean mask over 1M rows.
fn bench_filter_1m(c: &mut Criterion) {
    let array = make_array();
    let mask = make_mask();

    let mut group = c.benchmark_group("data_kernels");
    group.throughput(Throughput::Elements(ROWS as u64));
    group.bench_function("filter_1m_i32_two_thirds", |b| {
        b.iter(|| black_box(filter(&array, &mask).expect("filter")));
    });
    group.finish();
}

/// `take` — a fully-reversed 1M-row gather, this kernel's worst-case access
/// pattern.
fn bench_take_1m(c: &mut Criterion) {
    let array = make_array();
    let indices = make_reversed_indices();

    let mut group = c.benchmark_group("data_kernels");
    group.throughput(Throughput::Elements(ROWS as u64));
    group.bench_function("take_1m_i32_reversed", |b| {
        b.iter(|| black_box(take(&array, &*indices).expect("take")));
    });
    group.finish();
}

/// Validity-bitmap `AND`/`OR` over 1M bits (blueprint §6.1's bitmap
/// validity buffers) — the SIMD-gating evidence the doc comment above
/// names, alongside the four array kernels.
fn bench_bitmap_and_or_1m(c: &mut Criterion) {
    let left = make_bitmap(0x1234_5678_9abc_def0);
    let right = make_bitmap(0x0fed_cba9_8765_4321);

    let mut group = c.benchmark_group("data_kernels");
    group.throughput(Throughput::Elements(ROWS as u64));
    group.bench_function("bitmap_and_1m", |b| {
        b.iter(|| black_box(left.and(&right).expect("and")));
    });
    group.bench_function("bitmap_or_1m", |b| {
        b.iter(|| black_box(left.or(&right).expect("or")));
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_concat_1m,
    bench_cast_1m,
    bench_filter_1m,
    bench_take_1m,
    bench_bitmap_and_or_1m
);
criterion_main!(benches);
