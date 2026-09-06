// `missing_docs` (workspace lint) would otherwise fire on a non-unix target:
// `#![cfg(unix)]` below makes this whole crate empty there, which strips the
// module doc comment along with everything else, so this `allow` has to sit
// ahead of that line to survive the stripping.
#![cfg_attr(not(unix), allow(missing_docs))]
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Single-process, multi-threaded torture tests for the SPMC ring.
//!
//! Blueprint §23 risk #3 names "SHM plane races (SPMC reclamation)" as the
//! risk this crate has to retire, with the mitigation "design reviewed at
//! Gate 2 with mandatory loom/proptest interleaving suite before any
//! dependent crate lands". This file is the *real-thread* half of that suite:
//! one producer and N consumers hammering one ring with randomised sizes and
//! pacing, checking properties that only concurrency can break.
//!
//! The properties, and why each is the one worth asserting:
//!
//! - **No loss under keep-all pacing.** With
//!   [`OverflowPolicy::Block`](astrs_shm::OverflowPolicy::Block), every
//!   consumer must see every sequence, in order, with no gaps. If the
//!   reclamation predicate were ever wrong in the permissive direction, a
//!   slot would be recycled under a reader and this would fail.
//! - **Exact accounting under overwrite.** With
//!   [`OverflowPolicy::Overwrite`](astrs_shm::OverflowPolicy::Overwrite) a
//!   slow consumer *must* lose messages — but `received + lagged` must equal
//!   what the producer published, and delivered sequences must still be
//!   strictly increasing. An off-by-one in the lag computation shows up here
//!   and nowhere else.
//! - **A pinned sample is never overwritten.** Readers that hold samples
//!   across other readers' progress must still see the bytes they were given.
//! - **Attach/detach churn does not corrupt the consumer table.**
//!
//! Every test carries a wall-clock deadline: a livelock in the protocol would
//! otherwise present as a hung CI job rather than a failure.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use astrs_shm::{
    AttachOptions, Consumer, OverflowPolicy, Producer, RecvError, Segment, SegmentConfig,
    SegmentKey, ShmError, SlotState,
};
use astrs_wire::DataflowId;

/// The budget any one torture test may take before it is declared hung.
const DEADLINE: Duration = Duration::from_secs(30);

/// A tiny xorshift64* generator.
///
/// The suite needs randomised sizes and pacing, not statistical quality, and
/// the retained-crate list (§18.1) has no RNG in it. Seeding it explicitly
/// also means a failure is reproducible from the seed printed in the
/// assertion message.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            self.next_u64() % bound
        }
    }
}

fn ring(slots: u32, payload: u32, policy: OverflowPolicy) -> Arc<Segment> {
    let key =
        SegmentKey::from_parts(DataflowId::generate(), "torture", "out", 1).expect("valid ids");
    let config = SegmentConfig::new(slots, payload)
        .expect("valid geometry")
        .with_overflow(policy)
        .with_max_consumers(16)
        .expect("valid consumer table");
    Segment::create_shared(key, config).expect("segment")
}

/// The payload every message carries: its own sequence number, repeated, so a
/// reader can verify content as well as ordering.
fn fill(buffer: &mut [u8], seq: u64) {
    let stamp = seq.to_le_bytes();
    for (index, byte) in buffer.iter_mut().enumerate() {
        *byte = stamp[index % stamp.len()];
    }
}

fn check(payload: &[u8], seq: u64) {
    let stamp = seq.to_le_bytes();
    for (index, byte) in payload.iter().enumerate() {
        assert_eq!(
            *byte,
            stamp[index % stamp.len()],
            "payload for sequence {seq} is corrupt at byte {index}"
        );
    }
}

#[test]
fn keep_all_pacing_loses_nothing_across_four_consumers() {
    const MESSAGES: u64 = 4_000;
    const CONSUMERS: usize = 4;

    let segment = ring(8, 4096, OverflowPolicy::Block);
    let mut producer = Producer::new(Arc::clone(&segment)).expect("producer");

    let barrier = Arc::new(Barrier::new(CONSUMERS + 1));
    let started = Instant::now();
    let mut handles = Vec::new();

    for consumer_index in 0..CONSUMERS {
        let segment = Arc::clone(&segment);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let mut consumer = Consumer::attach(segment, AttachOptions::default()).expect("attach");
            barrier.wait();
            let mut rng = Rng::new(0xa11ce ^ (consumer_index as u64));
            let mut expected = 1u64;
            while expected <= MESSAGES {
                assert!(
                    started.elapsed() < DEADLINE,
                    "consumer {consumer_index} stalled at sequence {expected}"
                );
                match consumer.next_blocking(Duration::from_millis(200)) {
                    Ok(sample) => {
                        assert_eq!(
                            sample.seq(),
                            expected,
                            "consumer {consumer_index} saw a gap"
                        );
                        check(sample.payload(), sample.seq());
                        assert_eq!(sample.payload_address() % 128, 0);
                        expected += 1;
                        // Randomised hold time: sometimes keep the pin across
                        // another loop turn, which is what applies real
                        // backpressure to the producer.
                        if rng.below(64) == 0 {
                            std::thread::yield_now();
                        }
                        drop(sample);
                    }
                    Err(RecvError::Empty) => std::thread::yield_now(),
                    Err(other) => panic!("consumer {consumer_index}: {other}"),
                }
            }
            let stats = *consumer.stats();
            assert_eq!(stats.received, MESSAGES);
            assert_eq!(stats.lagged, 0, "keep-all pacing must not lose messages");
            stats
        }));
    }

    barrier.wait();
    let mut rng = Rng::new(0xbeef);
    let mut exhausted = 0u64;
    for seq in 1..=MESSAGES {
        let len = 1 + rng.below(2048) as usize;
        loop {
            assert!(
                started.elapsed() < DEADLINE,
                "producer stalled at sequence {seq}"
            );
            match producer.try_allocate(len) {
                Ok(mut window) => {
                    fill(window.as_mut_slice(), seq);
                    assert_eq!(window.commit(b"meta").expect("commit"), seq);
                    break;
                }
                // A real node falls back to the daemon path here (§6.2); the
                // test has no second path, so it yields instead. The
                // *library* never sleeps — that is what is being verified.
                Err(ShmError::PoolExhausted { .. }) => {
                    exhausted += 1;
                    std::thread::yield_now();
                }
                Err(other) => panic!("producer: {other}"),
            }
        }
    }

    for handle in handles {
        let stats = handle.join().expect("consumer thread");
        assert_eq!(stats.accounted(), MESSAGES);
    }

    assert_eq!(producer.stats().published, MESSAGES);
    assert_eq!(producer.stats().overwritten, 0);
    assert!(
        exhausted > 0,
        "an 8-slot ring with 4 consumers must have applied backpressure at least once"
    );

    // Every slot must end in a consistent state.
    for index in 0..segment.layout().slot_count() {
        assert_eq!(segment.slot(index).snapshot().violation(), None);
    }
}

#[test]
fn overwrite_accounting_is_exact_for_a_slow_consumer() {
    const MESSAGES: u64 = 3_000;

    let segment = ring(4, 512, OverflowPolicy::Overwrite);
    let mut producer = Producer::new(Arc::clone(&segment)).expect("producer");
    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(2));
    let started = Instant::now();

    let reader = {
        let segment = Arc::clone(&segment);
        let stop = Arc::clone(&stop);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let mut consumer = Consumer::attach(segment, AttachOptions::default()).expect("attach");
            // Whatever the ring already held when this consumer attached is
            // not its responsibility; the accounting identity is stated
            // relative to where it started.
            let first_wanted = consumer.cursor();
            barrier.wait();
            let mut rng = Rng::new(0x5eed);
            let mut last_delivered = 0u64;
            loop {
                match consumer.try_next() {
                    Ok(sample) => {
                        assert!(
                            sample.seq() > last_delivered,
                            "delivered sequences must be strictly increasing: {} after {last_delivered}",
                            sample.seq()
                        );
                        check(sample.payload(), sample.seq());
                        last_delivered = sample.seq();
                        // Deliberately slow: this is the consumer that must
                        // fall behind and observe exact lag counts.
                        if rng.below(4) == 0 {
                            std::thread::sleep(Duration::from_micros(50));
                        }
                    }
                    Err(RecvError::Lagged(missed)) => {
                        assert!(missed > 0, "a lag report must name a nonzero gap");
                    }
                    Err(RecvError::Empty) => {
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        std::thread::yield_now();
                    }
                    Err(RecvError::Closed) => break,
                    Err(other) => panic!("consumer: {other}"),
                }
                assert!(started.elapsed() < DEADLINE, "consumer stalled");
            }
            (*consumer.stats(), consumer.cursor(), first_wanted)
        })
    };

    barrier.wait();
    let mut rng = Rng::new(0xfeed);
    for seq in 1..=MESSAGES {
        let len = 1 + rng.below(256) as usize;
        loop {
            assert!(started.elapsed() < DEADLINE, "producer stalled");
            match producer.try_allocate(len) {
                Ok(mut window) => {
                    fill(window.as_mut_slice(), seq);
                    window.commit(b"").expect("commit");
                    break;
                }
                // Under `Overwrite`, exhaustion means every slot is *pinned*
                // by a live reader, which resolves as soon as it drops one.
                Err(ShmError::PoolExhausted { .. }) => std::thread::yield_now(),
                Err(other) => panic!("producer: {other}"),
            }
        }
    }
    stop.store(true, Ordering::Release);
    let (stats, cursor, first_wanted) = reader.join().expect("consumer thread");

    assert_eq!(producer.stats().published, MESSAGES);
    assert_eq!(
        cursor,
        MESSAGES + 1,
        "the consumer must end having accounted for the whole stream"
    );
    // **The accounting identity.** Every sequence from where the consumer
    // started to where it ended is either a delivery or part of a reported
    // gap — never both, never neither. An off-by-one anywhere in the lag
    // computation breaks this and nothing else in the suite.
    assert_eq!(
        stats.accounted(),
        cursor - first_wanted,
        "every message must be either delivered ({}) or reported lost ({}), from sequence {first_wanted}",
        stats.received,
        stats.lagged
    );
    assert!(
        stats.lagged > 0,
        "a deliberately slow consumer on a 4-slot ring must have lagged"
    );
    assert!(stats.received > 0, "it must also have received something");
    assert!(
        producer.stats().overwritten > 0,
        "the producer must have recorded the overwrites"
    );
}

#[test]
fn a_held_sample_is_never_overwritten_while_other_readers_race_ahead() {
    const MESSAGES: u64 = 2_000;

    let segment = ring(4, 256, OverflowPolicy::Overwrite);
    let mut producer = Producer::new(Arc::clone(&segment)).expect("producer");
    let corrupted = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(2));
    let started = Instant::now();

    // The holder pins a sample and re-reads it repeatedly. Under
    // `Overwrite`, the producer is free to recycle every *unpinned* slot; if
    // the gate protocol were wrong, these bytes would change underneath.
    let holder = {
        let segment = Arc::clone(&segment);
        let corrupted = Arc::clone(&corrupted);
        let stop = Arc::clone(&stop);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let mut consumer = Consumer::attach(segment, AttachOptions::default()).expect("attach");
            // Attach before the producer starts, so the holder is guaranteed
            // a stream to hold rather than racing thread-spawn latency.
            barrier.wait();
            let mut holds = 0u64;
            loop {
                assert!(started.elapsed() < DEADLINE, "holder stalled");
                match consumer.try_next() {
                    Ok(sample) => {
                        let seq = sample.seq();
                        let snapshot = sample.to_vec();
                        for _ in 0..16 {
                            if sample.payload() != snapshot.as_slice() {
                                corrupted.fetch_add(1, Ordering::Relaxed);
                                break;
                            }
                            // While pinned, the slot must stay READY.
                            assert_eq!(
                                segment_slot_state(&consumer, sample.slot_index()),
                                SlotState::Ready,
                                "a pinned slot changed state"
                            );
                            std::thread::yield_now();
                        }
                        check(&snapshot, seq);
                        holds += 1;
                    }
                    Err(RecvError::Lagged(_)) => {}
                    // Stop only once the ring is *drained*: a producer that
                    // outruns this deliberately slow holder would otherwise
                    // finish and set the flag before a single sample was ever
                    // held, and the test would assert nothing.
                    Err(RecvError::Empty) => {
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        std::thread::yield_now();
                    }
                    Err(RecvError::Closed) => break,
                    Err(other) => panic!("holder: {other}"),
                }
            }
            holds
        })
    };

    barrier.wait();
    let mut rng = Rng::new(0x1234_5678);
    for seq in 1..=MESSAGES {
        let len = 1 + rng.below(128) as usize;
        loop {
            assert!(started.elapsed() < DEADLINE, "producer stalled");
            match producer.try_allocate(len) {
                Ok(mut window) => {
                    fill(window.as_mut_slice(), seq);
                    window.commit(b"").expect("commit");
                    break;
                }
                Err(ShmError::PoolExhausted { .. }) => std::thread::yield_now(),
                Err(other) => panic!("producer: {other}"),
            }
        }
    }
    stop.store(true, Ordering::Release);
    let holds = holder.join().expect("holder thread");

    assert_eq!(
        corrupted.load(Ordering::Relaxed),
        0,
        "a pinned sample's bytes changed under it"
    );
    assert!(holds > 0, "the holder never got a sample");
}

fn segment_slot_state(consumer: &Consumer, index: u32) -> SlotState {
    consumer.segment().slot(index).state()
}

#[test]
fn attach_and_detach_churn_leaves_the_consumer_table_consistent() {
    const ROUNDS: u64 = 400;

    let segment = ring(8, 512, OverflowPolicy::Overwrite);
    let mut producer = Producer::new(Arc::clone(&segment)).expect("producer");
    let stop = Arc::new(AtomicBool::new(false));
    let started = Instant::now();

    let barrier = Arc::new(Barrier::new(5));
    let mut churners = Vec::new();
    for worker in 0..4u64 {
        let segment = Arc::clone(&segment);
        let stop = Arc::clone(&stop);
        let barrier = Arc::clone(&barrier);
        churners.push(std::thread::spawn(move || {
            barrier.wait();
            let mut rng = Rng::new(0xc0ffee ^ worker);
            let mut attaches = 0u64;
            while !stop.load(Ordering::Acquire) {
                assert!(started.elapsed() < DEADLINE, "churner {worker} stalled");
                let options = AttachOptions::default().with_doorbell(rng.below(2) == 0);
                match Consumer::attach(Arc::clone(&segment), options) {
                    Ok(mut consumer) => {
                        attaches += 1;
                        let reads = rng.below(8);
                        for _ in 0..reads {
                            match consumer.try_next() {
                                Ok(sample) => check(sample.payload(), sample.seq()),
                                Err(
                                    RecvError::Empty | RecvError::Lagged(_) | RecvError::Closed,
                                ) => {}
                                Err(other) => panic!("churner {worker}: {other}"),
                            }
                        }
                        // Dropping detaches.
                    }
                    // A full table is a legitimate outcome of the churn.
                    Err(ShmError::ConsumerTableFull { .. }) => std::thread::yield_now(),
                    Err(other) => panic!("churner {worker}: {other}"),
                }
            }
            attaches
        }));
    }

    barrier.wait();
    for seq in 1..=ROUNDS {
        loop {
            assert!(started.elapsed() < DEADLINE, "producer stalled");
            match producer.try_allocate(64) {
                Ok(mut window) => {
                    fill(window.as_mut_slice(), seq);
                    window.commit(b"").expect("commit");
                    break;
                }
                Err(ShmError::PoolExhausted { .. }) => std::thread::yield_now(),
                Err(other) => panic!("producer: {other}"),
            }
        }
        // Pace the producer so the churn genuinely overlaps the stream rather
        // than racing thread scheduling.
        if seq % 16 == 0 {
            std::thread::sleep(Duration::from_micros(200));
        }
    }
    stop.store(true, Ordering::Release);
    let mut total_attaches = 0;
    for churner in churners {
        total_attaches += churner.join().expect("churner thread");
    }
    assert!(total_attaches > 0, "no consumer ever attached");

    // Every entry must be back to empty with a cleared cursor, and the
    // attached count must agree.
    assert_eq!(segment.header().attached_consumers(), 0);
    for index in 0..segment.layout().max_consumers() {
        let entry = segment.consumer_entry(index);
        assert!(
            !entry.is_occupied(),
            "entry {index} is still occupied after every consumer detached"
        );
        assert_eq!(
            entry.cursor(),
            0,
            "entry {index} kept a stale cursor; the next claimer would inherit it"
        );
    }
    for index in 0..segment.layout().slot_count() {
        assert_eq!(segment.slot(index).snapshot().violation(), None);
    }
}

#[test]
fn many_consumers_on_one_ring_all_converge_on_the_same_stream() {
    const MESSAGES: u64 = 1_500;
    const CONSUMERS: usize = 8;

    let segment = ring(16, 1024, OverflowPolicy::Block);
    let mut producer = Producer::new(Arc::clone(&segment)).expect("producer");
    let barrier = Arc::new(Barrier::new(CONSUMERS + 1));
    let started = Instant::now();

    let mut handles = Vec::new();
    for consumer_index in 0..CONSUMERS {
        let segment = Arc::clone(&segment);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let mut consumer = Consumer::attach(segment, AttachOptions::default()).expect("attach");
            barrier.wait();
            let mut checksum = 0u64;
            let mut expected = 1u64;
            while expected <= MESSAGES {
                assert!(
                    started.elapsed() < DEADLINE,
                    "consumer {consumer_index} stalled at {expected}"
                );
                match consumer.next_blocking(Duration::from_millis(250)) {
                    Ok(sample) => {
                        assert_eq!(sample.seq(), expected);
                        checksum = checksum
                            .wrapping_mul(31)
                            .wrapping_add(sample.seq())
                            .wrapping_add(sample.payload().len() as u64);
                        expected += 1;
                    }
                    Err(RecvError::Empty) => std::thread::yield_now(),
                    Err(other) => panic!("consumer {consumer_index}: {other}"),
                }
            }
            checksum
        }));
    }

    barrier.wait();
    let mut rng = Rng::new(0xdead_beef);
    for seq in 1..=MESSAGES {
        let len = 1 + rng.below(900) as usize;
        loop {
            assert!(started.elapsed() < DEADLINE, "producer stalled at {seq}");
            match producer.try_allocate(len) {
                Ok(mut window) => {
                    fill(window.as_mut_slice(), seq);
                    window.commit(b"").expect("commit");
                    break;
                }
                Err(ShmError::PoolExhausted { .. }) => std::thread::yield_now(),
                Err(other) => panic!("producer: {other}"),
            }
        }
    }

    let checksums: Vec<u64> = handles
        .into_iter()
        .map(|handle| handle.join().expect("consumer thread"))
        .collect();
    let first = checksums[0];
    for (index, checksum) in checksums.iter().enumerate() {
        assert_eq!(
            *checksum, first,
            "consumer {index} saw a different stream than consumer 0"
        );
    }
    assert_eq!(producer.stats().published, MESSAGES);
}

#[test]
fn a_producer_closing_mid_stream_lets_every_consumer_drain_its_tail() {
    const MESSAGES: u64 = 500;
    const CONSUMERS: usize = 3;

    let segment = ring(8, 256, OverflowPolicy::Block);
    let mut producer = Producer::new(Arc::clone(&segment)).expect("producer");
    let barrier = Arc::new(Barrier::new(CONSUMERS + 1));
    let started = Instant::now();

    let mut handles = Vec::new();
    for consumer_index in 0..CONSUMERS {
        let segment = Arc::clone(&segment);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let mut consumer = Consumer::attach(segment, AttachOptions::default()).expect("attach");
            barrier.wait();
            let mut received = 0u64;
            loop {
                assert!(
                    started.elapsed() < DEADLINE,
                    "consumer {consumer_index} stalled after {received}"
                );
                match consumer.next_blocking(Duration::from_millis(200)) {
                    Ok(sample) => {
                        assert_eq!(sample.seq(), received + 1);
                        received += 1;
                    }
                    // A closed *and drained* segment ends the loop; an empty
                    // one just means the producer has not caught up.
                    Err(RecvError::Closed) => break,
                    Err(RecvError::Empty) => std::thread::yield_now(),
                    Err(other) => panic!("consumer {consumer_index}: {other}"),
                }
            }
            received
        }));
    }

    barrier.wait();
    for seq in 1..=MESSAGES {
        loop {
            assert!(started.elapsed() < DEADLINE, "producer stalled");
            match producer.try_allocate(32) {
                Ok(mut window) => {
                    fill(window.as_mut_slice(), seq);
                    window.commit(b"").expect("commit");
                    break;
                }
                Err(ShmError::PoolExhausted { .. }) => std::thread::yield_now(),
                Err(other) => panic!("producer: {other}"),
            }
        }
    }
    producer.close();

    for handle in handles {
        assert_eq!(
            handle.join().expect("consumer thread"),
            MESSAGES,
            "closing must drain the tail, not discard it"
        );
    }
    assert!(segment.header().is_closed());
}
