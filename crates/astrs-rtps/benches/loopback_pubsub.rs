//! RTPS loopback self-interop latency ladder (blueprint §20.4).
//!
//! | Bench | §20.4 target (0.1.0) |
//! |---|---|
//! | `rtps_loopback/pubsub_1kb` | p99 < 1.5 ms |
//!
//! # Harness reuse
//!
//! `#[path]`-includes `tests/harness/mod.rs` verbatim — the same
//! `Pair`/`wire`/`large_payload`/`PATIENCE` fixtures every `tests/e2e_*.rs`
//! file in this crate builds on: two real AstRS participants, real UDP
//! sockets on loopback, unicast initial peers (blueprint §10.2's in-repo
//! interop proof; see that file's own header for why unicast, not
//! multicast). One writer and one reader are matched once via [`harness::wire`]
//! and reused for every measured publish.
//!
//! # What is timed
//!
//! [`WriterHandle::write`] "puts a sample on the wire before returning" (its
//! own doc's words) — there is no separate untimed setup step the way the
//! SHM handoff bench has one (allocating and filling a slot before commit).
//! The timed span is therefore the whole `write(...).await` through the
//! matched reader's `take_within` returning, one publish at a time — not
//! pipelined, so this is **quiescent** latency (the reader is already
//! parked in `take_within`, which itself is a [`tokio::sync::Notify`]
//! wakeup inside [`ReaderHandle`]'s sink, not a poll loop — see
//! `astrs-rtps`'s `SampleSink`). A saturated publisher would show a worse
//! p99, for the same reason noted in the SHM and transport benches' own
//! headers.
//!
//! Both endpoints run on one [`tokio::runtime::Runtime`] in this one
//! process — the harness's own "loopback self-interop" design (blueprint
//! §18: no captured or cross-stack fixtures in this repo).
//!
//! # Sampling
//!
//! [`SamplingMode::Flat`] with small, explicit `warm_up_time`/
//! `measurement_time` budgets, for the same predictability reasons as the
//! SHM and transport benches — even though nothing here is excluded from
//! criterion's own timing (unlike those two, so criterion's default
//! calibration would not itself misfire), consistent settings make the
//! whole suite's total run time easy to reason about.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "../tests/harness/mod.rs"]
mod harness;

use std::cell::RefCell;
use std::time::{Duration, Instant};

use astrs_rtps::discovery::{ReaderQos, WriterQos};
use criterion::{Criterion, SamplingMode, criterion_group, criterion_main};
use harness::{PATIENCE, Pair, large_payload, wire};

/// RTPS loopback pub→sub self-interop, 1 KB, p99 (blueprint §20.4).
const RTPS_TARGET: Duration = Duration::from_micros(1500);

/// Sorts `samples` in place and returns the value at percentile `p`
/// (`0.0..=100.0`), nearest-rank on the sorted sample set.
fn percentile(samples: &mut [Duration], p: f64) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    samples.sort_unstable();
    let rank = ((p / 100.0) * (samples.len() - 1) as f64).round() as usize;
    samples[rank.min(samples.len() - 1)]
}

/// Prints the §20.4 gate line `scripts/bench-gate.sh` greps for, plus a loud
/// (but non-fatal) warning on a miss. Never fails the build itself.
fn report(name: &str, samples: &mut [Duration], target: Duration) {
    let n = samples.len();
    let p50 = percentile(samples, 50.0);
    let p99 = percentile(samples, 99.0);
    let max = samples.iter().max().copied().unwrap_or_default();
    let result = if p99 <= target { "PASS" } else { "FAIL" };
    println!(
        "BENCH_GATE name={name} n={n} p50_us={:.2} p99_us={:.2} max_us={:.2} target_us={:.2} result={result}",
        p50.as_secs_f64() * 1e6,
        p99.as_secs_f64() * 1e6,
        max.as_secs_f64() * 1e6,
        target.as_secs_f64() * 1e6,
    );
    if result == "FAIL" {
        eprintln!(
            "WARN: {name} missed its blueprint §20.4 target: p99={:.2}us > target={:.2}us (n={n} samples)",
            p99.as_secs_f64() * 1e6,
            target.as_secs_f64() * 1e6,
        );
    }
}

/// 1 KB samples, one matched writer/reader pair, quiescent publish latency
/// (see this file's header).
fn bench_pubsub_1kb(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let pair = runtime.block_on(async {
        let pair = Pair::new().await;
        pair.await_discovery().await;
        pair
    });
    let (writer, reader) =
        runtime.block_on(async { wire(&pair, WriterQos::default(), ReaderQos::default()).await });

    let payload = large_payload(1024);
    let samples = RefCell::new(Vec::<Duration>::new());

    {
        let mut group = c.benchmark_group("rtps_loopback");
        group.sampling_mode(SamplingMode::Flat);
        group.sample_size(30);
        group.warm_up_time(Duration::from_millis(10));
        group.measurement_time(Duration::from_millis(500));
        group.bench_function("pubsub_1kb", |b| {
            b.iter_custom(|iters| {
                runtime.block_on(async {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let start = Instant::now();
                        writer.write(payload.clone()).await.expect("write");
                        let sample = reader
                            .take_within(PATIENCE)
                            .await
                            .expect("a sample within PATIENCE");
                        let latency = start.elapsed();
                        drop(sample);
                        total += latency;
                        samples.borrow_mut().push(latency);
                    }
                    total
                })
            });
        });
        group.finish();
    }

    runtime.block_on(pair.shutdown());

    let mut collected = samples.into_inner();
    report("rtps.pubsub_1kb_p99", &mut collected, RTPS_TARGET);
}

criterion_group!(benches, bench_pubsub_1kb);
criterion_main!(benches);
