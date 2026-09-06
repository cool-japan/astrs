//! Same-host SHM latency ladder (blueprint §20.4).
//!
//! | Bench | §20.4 target (0.1.0) |
//! |---|---|
//! | `shm_handoff/handoff_4mb` | p99 < 120 µs |
//! | `shm_rtt/rtt_256b` | p99 < 25 µs |
//!
//! # What is actually timed
//!
//! Both benches use one producer thread (the criterion-driven thread) and
//! one persistent helper thread, wired together exactly as
//! `tests/torture.rs` wires its producer/consumer pairs: a
//! [`std::sync::Barrier`] rendezvous before either side touches the ring, a
//! ring sized and closed the same way [`ring`] here mirrors `torture.rs`'s
//! own `ring` helper.
//!
//! **Handoff (4 MiB, one leg).** The timed span starts *after* the 4 MiB
//! slot has already been allocated and filled — a producer's own "I have
//! computed this frame" time is not ring overhead — and stops the instant
//! the consumer thread's blocking `next_blocking` returns, reported back
//! over a plain [`std::sync::mpsc`] channel as the [`Instant`] the consumer
//! observed, so the channel hop itself is not folded into the number. This
//! is deliberately **quiescent** handoff latency: the consumer is already
//! parked in `next_blocking` when the commit lands (the ring is empty
//! between messages), not a saturated producer racing a busy consumer. A
//! continuously-saturated stream would show a worse p99, because a message
//! could then also wait behind the consumer's processing of the one before
//! it.
//!
//! **RTT (256 B, two legs).** A second ring carries the echo back; the timed
//! span is commit → the echoed reply's `next_blocking` return, so it is a
//! true round trip through two independent rings and one responder thread.
//!
//! # Sampling
//!
//! Both groups pin [`SamplingMode::Flat`] (a fixed iteration count per
//! sample) and set small, explicit `warm_up_time`/`measurement_time`
//! budgets: the timed span *excludes* the 4 MiB fill, so criterion's own
//! warm-up calibration — which only sees the (much smaller) returned
//! duration — would otherwise size each sample far larger than the wall
//! clock this file actually needs, ballooning real run time without adding
//! statistical value. Every per-message latency (measurement *and* the
//! handful of warm-up calls, which criterion does not expose a boundary
//! for) is folded into the `p50`/`p99`/`max` this file reports itself,
//! separately from criterion's own report — a warm-up call is typically the
//! *worst* observed latency (first-touch page faults, cold doorbell fd), so
//! any contamination biases the reported p99 pessimistically, never
//! optimistically.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::RefCell;
use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use astrs_shm::{
    AttachOptions, Consumer, OverflowPolicy, Producer, RecvError, Segment, SegmentConfig,
    SegmentKey, ShmError,
};
use astrs_wire::DataflowId;
use criterion::{Criterion, SamplingMode, criterion_group, criterion_main};

/// Same-host SHM handoff, 4 MiB frame, p99 (blueprint §20.4).
const HANDOFF_TARGET: Duration = Duration::from_micros(120);

/// Same-host small message (256 B) RTT, p99 (blueprint §20.4).
const RTT_TARGET: Duration = Duration::from_micros(25);

/// How long any single blocking receive may take before this file declares
/// the ring stalled rather than waiting forever. Generous on purpose — see
/// `tests/harness/mod.rs` (astrs-rtps)'s identical `PATIENCE`: this bounds a
/// failure, not a success.
const PATIENCE: Duration = Duration::from_secs(5);

/// A ring built the same way `tests/torture.rs`'s `ring` helper builds one:
/// a fresh dataflow id, `Block` overflow (every message must be accounted
/// for), room for a couple of attaches.
fn ring(slots: u32, payload: u32) -> Arc<Segment> {
    let key = SegmentKey::from_parts(DataflowId::generate(), "bench", "out", 1).expect("valid ids");
    let config = SegmentConfig::new(slots, payload)
        .expect("valid geometry")
        .with_overflow(OverflowPolicy::Block)
        .with_max_consumers(2)
        .expect("valid consumer table");
    Segment::create_shared(key, config).expect("segment")
}

/// Allocates a slot of `len` bytes, copies `filler` into it and commits it
/// (empty metadata), retrying — yielding between attempts — on transient
/// pool exhaustion: the same retry shape every producer loop in
/// `tests/torture.rs` uses.
///
/// Returns the [`Instant`] captured immediately before the commit, so a
/// caller can time exactly "commit onward" and exclude the (untimed, by
/// design — see this file's header) allocate-and-fill work. `SampleMut`
/// deliberately never escapes this function: returning it from a loop that
/// also retries on the `Err` arm defeats NLL's borrow-region inference
/// (`E0499`, a known limitation short of Polonius), so the commit happens
/// here instead of at the call site.
fn allocate_fill_commit(producer: &mut Producer, len: usize, filler: &[u8]) -> Instant {
    loop {
        match producer.try_allocate(len) {
            Ok(mut window) => {
                window.as_mut_slice().copy_from_slice(filler);
                let start = Instant::now();
                window.commit(b"").expect("commit");
                return start;
            }
            Err(ShmError::PoolExhausted { .. }) => thread::yield_now(),
            Err(other) => panic!("producer allocate: {other}"),
        }
    }
}

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
/// (but non-fatal — see this crate's `Cargo.toml` bench doc, and the file
/// header of `scripts/bench-gate.sh`) warning on a miss. Never fails the
/// build itself.
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

/// 4 MiB frame, one producer thread, one consumer thread, quiescent
/// handoff latency (see this file's header).
fn bench_handoff_4mb(c: &mut Criterion) {
    const PAYLOAD_LEN: usize = 4 * 1024 * 1024;
    const SLOT_COUNT: u32 = 4;

    let segment = ring(SLOT_COUNT, PAYLOAD_LEN as u32);
    let barrier = Arc::new(Barrier::new(2));
    let (ack_tx, ack_rx) = mpsc::channel::<Instant>();

    let consumer_segment = Arc::clone(&segment);
    let consumer_barrier = Arc::clone(&barrier);
    let consumer_thread = thread::spawn(move || {
        let mut consumer =
            Consumer::attach(consumer_segment, AttachOptions::default()).expect("attach");
        consumer_barrier.wait();
        loop {
            match consumer.next_blocking(PATIENCE) {
                Ok(sample) => {
                    let observed = Instant::now();
                    drop(sample);
                    if ack_tx.send(observed).is_err() {
                        break;
                    }
                }
                Err(RecvError::Closed) => break,
                Err(RecvError::Empty) => panic!("consumer: no commit within {PATIENCE:?}"),
                Err(other) => panic!("consumer recv: {other}"),
            }
        }
    });

    let mut producer = Producer::new(Arc::clone(&segment)).expect("producer");
    barrier.wait();

    let filler = vec![0xa5_u8; PAYLOAD_LEN];
    let samples = RefCell::new(Vec::<Duration>::new());

    {
        let mut group = c.benchmark_group("shm_handoff");
        group.sampling_mode(SamplingMode::Flat);
        group.sample_size(10);
        group.warm_up_time(Duration::from_millis(5));
        group.measurement_time(Duration::from_millis(200));
        group.bench_function("handoff_4mb", |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let start = allocate_fill_commit(&mut producer, PAYLOAD_LEN, &filler);
                    let observed = ack_rx
                        .recv_timeout(PATIENCE)
                        .expect("consumer thread alive");
                    let latency = observed.saturating_duration_since(start);
                    total += latency;
                    samples.borrow_mut().push(latency);
                }
                total
            });
        });
        group.finish();
    }

    producer.close();
    consumer_thread.join().expect("consumer thread");

    let mut collected = samples.into_inner();
    report("shm.handoff_4mb_p99", &mut collected, HANDOFF_TARGET);
}

/// 256 B request/reply over two rings: a strict one-at-a-time ping-pong, so
/// each measurement is a genuine round trip rather than a pipelined one.
fn bench_rtt_256b(c: &mut Criterion) {
    const PAYLOAD_LEN: usize = 256;
    const SLOT_COUNT: u32 = 4;

    let request_ring = ring(SLOT_COUNT, PAYLOAD_LEN as u32);
    let reply_ring = ring(SLOT_COUNT, PAYLOAD_LEN as u32);
    let barrier = Arc::new(Barrier::new(2));

    let responder_request = Arc::clone(&request_ring);
    let responder_reply = Arc::clone(&reply_ring);
    let responder_barrier = Arc::clone(&barrier);
    let responder_thread = thread::spawn(move || {
        let mut request_consumer =
            Consumer::attach(responder_request, AttachOptions::default()).expect("attach");
        let mut reply_producer = Producer::new(responder_reply).expect("producer");
        responder_barrier.wait();
        loop {
            match request_consumer.next_blocking(PATIENCE) {
                Ok(sample) => {
                    let _ = allocate_fill_commit(
                        &mut reply_producer,
                        sample.payload().len(),
                        sample.payload(),
                    );
                }
                Err(RecvError::Closed) => break,
                Err(RecvError::Empty) => panic!("responder: no request within {PATIENCE:?}"),
                Err(other) => panic!("responder recv: {other}"),
            }
        }
    });

    let mut request_producer = Producer::new(Arc::clone(&request_ring)).expect("producer");
    let mut reply_consumer =
        Consumer::attach(Arc::clone(&reply_ring), AttachOptions::default()).expect("attach");
    barrier.wait();

    let payload = vec![0x5a_u8; PAYLOAD_LEN];
    let samples = RefCell::new(Vec::<Duration>::new());

    {
        let mut group = c.benchmark_group("shm_rtt");
        group.sampling_mode(SamplingMode::Flat);
        group.sample_size(30);
        group.warm_up_time(Duration::from_millis(5));
        group.measurement_time(Duration::from_millis(300));
        group.bench_function("rtt_256b", |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let start = allocate_fill_commit(&mut request_producer, PAYLOAD_LEN, &payload);
                    match reply_consumer.next_blocking(PATIENCE) {
                        Ok(reply) => drop(reply),
                        Err(RecvError::Empty) => panic!("no reply within {PATIENCE:?}"),
                        Err(other) => panic!("reply recv: {other}"),
                    }
                    let latency = start.elapsed();
                    total += latency;
                    samples.borrow_mut().push(latency);
                }
                total
            });
        });
        group.finish();
    }

    request_producer.close();
    responder_thread.join().expect("responder thread");

    let mut collected = samples.into_inner();
    report("shm.rtt_256b_p99", &mut collected, RTT_TARGET);
}

criterion_group!(benches, bench_handoff_4mb, bench_rtt_256b);
criterion_main!(benches);
