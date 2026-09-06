//! Timer wheel latency ladder (blueprint §20.4).
//!
//! | Bench | §20.4 target (0.1.0) |
//! |---|---|
//! | `real_jitter/wheel_1khz_jitter` | p99 < 150 µs |
//!
//! Two different questions, two different mechanisms:
//!
//! **The §20.4 gate** (`bench_real_1khz_jitter`) asks "how far does a real,
//! wall-clock-driven tick land from its exact drift-free grid point?" —
//! blueprint §11.1's `timer_jitter_us`. This is not something criterion's
//! sampling model answers (there is no "iteration" to repeat; it is one
//! continuous real-time process), so this bench runs the crate's own
//! production driver, [`TimerWheelDriver`], for a fixed wall-clock window
//! and reads the answer straight out of the crate's own [`JitterStats`] —
//! the same p50/p99 tracker [`astrs_scheduler::TimerWheel`] maintains for
//! every registered timer in production, not a percentile this file
//! computes itself. `#[bench]`-style sampling would only add noise here:
//! the wheel's own P² estimator already answers the question online, from
//! one continuous run, the way a real daemon would read it.
//!
//! **The supplementary micro-bench** (`bench_wheel_advance_overhead`) asks a
//! different, criterion-shaped question: "how much CPU does one
//! `TimerWheel::advance` cascade cost, with several timers registered?" —
//! useful for spotting an algorithmic regression, but it is **not** a §20.4
//! gate and carries no target. It drives the wheel synchronously with a
//! manually-advanced clock (no real sleeping, no driver task), which is
//! what makes it a fair criterion subject in the first place.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::hint::black_box;
use std::time::{Duration, Instant};

use astrs_scheduler::{JitterStats, MissedTickPolicy, TimerSpec, TimerWheel, TimerWheelDriver};
use astrs_time::TimerInterval;
use criterion::{Criterion, criterion_group, criterion_main};

/// Timer jitter @1 kHz, p99 (blueprint §20.4).
const JITTER_TARGET: Duration = Duration::from_micros(150);

/// How long the real driver runs before this file reads its jitter stats.
/// At a 1 ms tick period this is on the order of a couple thousand samples
/// — enough for [`JitterStats`]'s P² estimator to have long since converged
/// (see that type's own doc for the accuracy bound), without making this
/// bench file dominate the suite's wall-clock time.
const JITTER_RUN_FOR: Duration = Duration::from_secs(3);

/// Runs the real, wall-clock-driven [`TimerWheelDriver`] at `tick_period`
/// with one timer registered at the same period, for `run_for`, and returns
/// that timer's [`JitterStats`] as the driver measured them.
fn measure_real_jitter(tick_period: Duration, run_for: Duration) -> JitterStats {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async {
        let (driver, handle, _fired) = TimerWheelDriver::spawn(tick_period);
        let interval = TimerInterval::from_millis(1).expect("a 1 ms interval is valid");
        let id = handle.insert_now(TimerSpec::new(interval, MissedTickPolicy::Skip));

        tokio::time::sleep(run_for).await;

        let stats = handle
            .jitter_stats(id)
            .expect("the timer is still registered");
        driver.abort();
        stats
    })
}

/// Prints the §20.4 gate line `scripts/bench-gate.sh` greps for, plus a loud
/// (but non-fatal) warning on a miss. Never fails the build itself.
fn report(name: &str, stats: &JitterStats, target: Duration) {
    let n = stats.samples();
    let p50 = stats.p50().unwrap_or_default();
    let p99 = stats.p99().unwrap_or_default();
    let max = stats.max();
    let result = if n > 0 && p99 <= target {
        "PASS"
    } else {
        "FAIL"
    };
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

/// The §20.4 gate: one real-time, 1 kHz timer's jitter over one continuous
/// [`JITTER_RUN_FOR`] run (see this file's header for why this is not a
/// criterion sample loop — running it *inside* `iter_custom` would either
/// repeat a three-second measurement ten-plus times for no statistical
/// benefit, or, worse, print the `BENCH_GATE` line once per repeat and
/// break `scripts/bench-gate.sh`'s "exactly six lines" count). The
/// measurement happens exactly once, directly; criterion is still given a
/// trivial, fast benchmark afterward — reading the already-computed
/// [`JitterStats`] back out — purely so this file's binary produces a
/// normal criterion report section too.
fn bench_real_1khz_jitter(c: &mut Criterion) {
    let stats = measure_real_jitter(Duration::from_millis(1), JITTER_RUN_FOR);
    report("scheduler.wheel_1khz_jitter_p99", &stats, JITTER_TARGET);

    let mut group = c.benchmark_group("real_jitter");
    group.bench_function("wheel_1khz_jitter_stats_readback", |b| {
        b.iter(|| black_box(stats.samples()));
    });
    group.finish();
}

/// Supplementary, **not** a §20.4 gate (see this file's header): the pure
/// CPU cost of one `TimerWheel::advance` cascade with 64 timers registered
/// at staggered periods, driven by a manually-advanced clock rather than
/// real time.
fn bench_wheel_advance_overhead(c: &mut Criterion) {
    let epoch = Instant::now();
    let mut wheel = TimerWheel::new(epoch);
    for offset in 0..64u64 {
        let period_ms = 5 + offset % 7;
        let interval = TimerInterval::with_anchor(Duration::from_millis(period_ms), epoch)
            .expect("a millisecond-scale interval is valid");
        wheel.insert(TimerSpec::new(interval, MissedTickPolicy::Skip), epoch);
    }
    let mut now = epoch;

    let mut group = c.benchmark_group("scheduler_wheel");
    group.bench_function("advance_1ms_tick_64_timers", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                now += Duration::from_millis(1);
                let start = Instant::now();
                let fired = wheel.advance(black_box(now));
                total += start.elapsed();
                black_box(fired);
            }
            total
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_real_1khz_jitter,
    bench_wheel_advance_overhead
);
criterion_main!(benches);
