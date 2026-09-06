//! A registry-wide concurrency stress test: several threads recording
//! into counters, gauges, histograms and a labelled family at once,
//! while another thread repeatedly snapshots the whole registry mid-flight
//! -- the shape of load a real daemon (worker threads on the hot path,
//! a sampler task on a timer) puts on this crate.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use astrs_telemetry::metrics::MetricRegistry;
use astrs_time::HlcTimestamp;
use astrs_wire::MetricValue;

const WRITER_THREADS: u64 = 8;
const OBSERVATIONS_PER_THREAD: u64 = 2_000;

#[test]
fn concurrent_writers_and_a_concurrent_reader_never_lose_an_update() {
    let registry = Arc::new(MetricRegistry::new());
    let counter = registry.register_counter("events_total");
    let gauge = registry.register_gauge("in_flight");
    let histogram = registry.register_histogram("latency_seconds", vec![0.001, 0.01, 0.1, 1.0]);
    let family = registry.register_counter_family("io_bytes_total", &["direction"], 8);

    let stop = Arc::new(AtomicBool::new(false));

    // A background reader hammers `snapshot()` while writers are active,
    // to catch anything that only shows up under lock contention (a
    // deadlock, a torn read, a panic) rather than only testing the
    // quiescent-state numbers at the end.
    let reader_registry = Arc::clone(&registry);
    let reader_stop = Arc::clone(&stop);
    let reader = thread::spawn(move || {
        let mut snapshots = 0u64;
        while !reader_stop.load(Ordering::Relaxed) {
            let batch = reader_registry.snapshot(HlcTimestamp::EPOCH, "stress_test");
            assert!(batch.within_limits());
            snapshots += 1;
        }
        snapshots
    });

    let writers: Vec<_> = (0..WRITER_THREADS)
        .map(|thread_index| {
            let counter = Arc::clone(&counter);
            let gauge = Arc::clone(&gauge);
            let histogram = Arc::clone(&histogram);
            let family = Arc::clone(&family);
            let direction = if thread_index % 2 == 0 { "rx" } else { "tx" };
            thread::spawn(move || {
                let series = family.get_or_create(&[direction]);
                for i in 0..OBSERVATIONS_PER_THREAD {
                    counter.inc();
                    gauge.add(1.0);
                    gauge.add(-1.0);
                    histogram.observe(0.005 * ((i % 7) as f64));
                    series.inc();
                }
            })
        })
        .collect();

    for writer in writers {
        writer.join().expect("writer thread must not panic");
    }
    stop.store(true, Ordering::Relaxed);
    let snapshots_taken = reader.join().expect("reader thread must not panic");
    assert!(
        snapshots_taken > 0,
        "the reader must have gotten at least one snapshot in"
    );

    let expected_total = WRITER_THREADS * OBSERVATIONS_PER_THREAD;
    assert_eq!(counter.get(), expected_total);
    assert_eq!(gauge.get(), 0.0, "every +1 was paired with a -1");

    let (count, _sum, _buckets) = histogram.snapshot();
    assert_eq!(count, expected_total);

    let final_batch = registry.snapshot(HlcTimestamp::EPOCH, "final");
    let events_total = final_batch
        .points
        .iter()
        .find(|p| p.name == "events_total")
        .expect("events_total present");
    assert_eq!(events_total.value, MetricValue::Counter(expected_total));

    let rx_total: u64 = final_batch
        .points
        .iter()
        .filter(|p| p.name == "io_bytes_total" && p.label("direction") == Some("rx"))
        .map(|p| match p.value {
            MetricValue::Counter(v) => v,
            _ => 0,
        })
        .sum();
    let tx_total: u64 = final_batch
        .points
        .iter()
        .filter(|p| p.name == "io_bytes_total" && p.label("direction") == Some("tx"))
        .map(|p| match p.value {
            MetricValue::Counter(v) => v,
            _ => 0,
        })
        .sum();
    assert_eq!(rx_total + tx_total, expected_total);
    assert_eq!(rx_total, (WRITER_THREADS / 2) * OBSERVATIONS_PER_THREAD);
    assert_eq!(tx_total, (WRITER_THREADS / 2) * OBSERVATIONS_PER_THREAD);
}

#[test]
fn concurrent_registration_of_the_same_family_name_converges_on_one_definition() {
    // Several threads race to register the *same* family name for the
    // first time; `MetricRegistry`'s idempotent registration must hand
    // every one of them a handle that shares the same underlying state,
    // never a distinct, disconnected series.
    let registry = Arc::new(MetricRegistry::new());
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let registry = Arc::clone(&registry);
            thread::spawn(move || {
                let counter = registry.register_counter("races_total");
                counter.inc();
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("registration race must not panic");
    }
    assert_eq!(registry.register_counter("races_total").get(), 16);
}
