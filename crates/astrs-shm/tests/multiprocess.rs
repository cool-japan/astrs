// `missing_docs` (workspace lint) would otherwise fire on a non-unix target:
// `#![cfg(unix)]` below makes this whole crate empty there, which strips the
// module doc comment along with everything else, so this `allow` has to sit
// ahead of that line to survive the stripping.
#![cfg_attr(not(unix), allow(missing_docs))]
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Real multi-process integration: separate processes exchanging real frames
//! through a real segment, brokered over a real Unix socket.
//!
//! Everything else in the suite runs inside one process, where "two mappings
//! of one segment" is a plausible stand-in for the product but not the
//! product. This file is the product: the test binary re-executes *itself*
//! with a role environment variable, and the children are ordinary OS
//! processes that reach the ring exactly the way a spawned node does —
//! [`SegmentClient::attach`] over `SCM_RIGHTS`, no shared address space, no
//! inherited descriptors.
//!
//! # Why fd passing and not fd inheritance
//!
//! [`std::process::Command`] sets `FD_CLOEXEC` on every non-standard
//! descriptor, so inheriting a segment would need `pre_exec` and `unsafe`.
//! Passing it over the broker's socket is both portable and the path the
//! daemon actually takes (§6.3), so the test exercises
//! [`SegmentBroker::serve_once`] / [`SegmentClient::attach`] instead of
//! leaving them for W3 to discover.
//!
//! # What each test proves
//!
//! - **`frames_cross_a_real_process_boundary`** — payload integrity, ordering
//!   and 128-byte alignment in a *separately mapped* consumer view, plus the
//!   cross-process doorbell relay: the producer child waits until every
//!   consumer's doorbell has been relayed into its own registry before it
//!   publishes, so a nonzero `doorbell_rings` proves the relay works rather
//!   than proving the 20 ms polling fallback works.
//! - **`consumers_survive_a_kill_9_of_the_producer`** — the crash case §6.2
//!   is designed around. The producer is `SIGKILL`ed mid-stream with *no*
//!   broker intervention, and the consumers must still terminate, reporting
//!   [`ShmError::ProducerGone`] or a drained close.
//! - **`the_broker_closes_a_segment_whose_producer_was_killed`** — the same
//!   crash with the daemon alive: [`SegmentBroker::poll_producers`] marks the
//!   segment closed and [`SegmentBroker::sweep`] retires it after the drain.
//! - **`a_stale_generation_is_refused_across_processes`** — §6.2's generation
//!   safety, checked from a process that only ever saw a descriptor.

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use astrs_shm::{
    AttachOptions, Consumer, OverflowPolicy, Producer, RecvError, SegmentBroker, SegmentClient,
    SegmentConfig, SegmentKey, ShmError,
};
use astrs_wire::DataflowId;

const ROLE_ENV: &str = "ASTRS_SHM_TEST_ROLE";
const SOCKET_ENV: &str = "ASTRS_SHM_TEST_SOCKET";
const DATAFLOW_ENV: &str = "ASTRS_SHM_TEST_DATAFLOW";
const GENERATION_ENV: &str = "ASTRS_SHM_TEST_GENERATION";
const COUNT_ENV: &str = "ASTRS_SHM_TEST_COUNT";
const PEERS_ENV: &str = "ASTRS_SHM_TEST_PEERS";

const NODE: &str = "mp-producer";
const OUTPUT: &str = "frames";

/// How long a child may take before the parent declares the test hung.
const CHILD_DEADLINE: Duration = Duration::from_secs(45);

/// The payload size every message carries.
const PAYLOAD_LEN: usize = 777;

// ---------------------------------------------------------------------------
// Child entry point
// ---------------------------------------------------------------------------

/// The re-execution target.
///
/// Run normally (no role in the environment) this is a no-op, so it costs one
/// trivially passing test. Run with `ASTRS_SHM_TEST_ROLE` set — which is how
/// the parent spawns it — it becomes the child process's `main`.
#[test]
fn child_entrypoint() {
    let Ok(role) = std::env::var(ROLE_ENV) else {
        return;
    };
    let outcome = match role.as_str() {
        "producer" => run_producer(false),
        "producer-forever" => run_producer(true),
        "consumer" => run_consumer(false),
        "consumer-until-gone" => run_consumer(true),
        "consumer-idle" => run_idle_consumer(),
        "stale-consumer" => run_stale_consumer(),
        other => panic!("unknown child role {other:?}"),
    };
    match outcome {
        Ok(()) => std::process::exit(0),
        Err(err) => {
            eprintln!("child role {role} failed: {err}");
            std::process::exit(2);
        }
    }
}

fn child_key() -> SegmentKey {
    let dataflow = std::env::var(DATAFLOW_ENV).expect("dataflow id");
    let dataflow = u128::from_str_radix(&dataflow, 16).expect("hex dataflow id");
    let generation: u64 = std::env::var(GENERATION_ENV)
        .expect("generation")
        .parse()
        .expect("numeric generation");
    SegmentKey::from_parts(DataflowId::from_u128(dataflow), NODE, OUTPUT, generation)
        .expect("valid ids")
}

fn child_socket() -> String {
    std::env::var(SOCKET_ENV).expect("socket path")
}

fn child_count() -> u64 {
    std::env::var(COUNT_ENV)
        .expect("count")
        .parse()
        .expect("numeric count")
}

fn child_peers() -> usize {
    std::env::var(PEERS_ENV)
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(0)
}

/// Stamp a payload with its sequence number so a reader can verify content.
fn stamp(buffer: &mut [u8], seq: u64) {
    let bytes = seq.to_le_bytes();
    for (index, byte) in buffer.iter_mut().enumerate() {
        *byte = bytes[index % bytes.len()];
    }
}

fn verify(payload: &[u8], seq: u64) -> Result<(), String> {
    let bytes = seq.to_le_bytes();
    if payload.len() != PAYLOAD_LEN {
        return Err(format!(
            "sequence {seq} has {} bytes, expected {PAYLOAD_LEN}",
            payload.len()
        ));
    }
    for (index, byte) in payload.iter().enumerate() {
        if *byte != bytes[index % bytes.len()] {
            return Err(format!("sequence {seq} is corrupt at byte {index}"));
        }
    }
    Ok(())
}

/// The producer child: attach, claim the producer role, wait for every peer's
/// doorbell to be relayed, then publish.
fn run_producer(forever: bool) -> Result<(), String> {
    let key = child_key();
    let socket = child_socket();
    let count = child_count();
    let peers = child_peers();

    let mut client = SegmentClient::connect(&socket).map_err(|err| err.to_string())?;
    let segment = Arc::new(client.attach(&key).map_err(|err| err.to_string())?);
    if segment.header().generation() != key.generation() {
        return Err("attached the wrong generation".to_owned());
    }
    let channel = SegmentClient::connect(&socket)
        .map_err(|err| err.to_string())?
        .claim_producer(&key, Arc::clone(&segment))
        .map_err(|err| err.to_string())?;

    // Wait for the broker to relay every consumer's doorbell into *this*
    // process's registry. Without this the test could pass on the bounded
    // polling fallback and never exercise the relay at all.
    let deadline = Instant::now() + Duration::from_secs(20);
    while segment.doorbells().len() < peers {
        channel.poll().map_err(|err| err.to_string())?;
        if Instant::now() > deadline {
            return Err(format!(
                "only {} of {peers} consumer doorbells were relayed",
                segment.doorbells().len()
            ));
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    let mut producer = Producer::new(Arc::clone(&segment)).map_err(|err| err.to_string())?;
    let mut payload = vec![0u8; PAYLOAD_LEN];
    let mut seq = 1u64;
    loop {
        stamp(&mut payload, seq);
        match producer.send(&payload, &seq.to_le_bytes()) {
            Ok(published) => {
                if published != seq {
                    return Err(format!("expected sequence {seq}, published {published}"));
                }
                seq += 1;
            }
            // The reliable-path fallback does not exist in this test; yield
            // and try again, exactly as the torture suite does.
            Err(ShmError::PoolExhausted { .. }) => std::thread::yield_now(),
            Err(other) => return Err(other.to_string()),
        }
        if !forever && seq > count {
            break;
        }
        if forever {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    if peers > 0 && producer.stats().doorbell_rings == 0 {
        return Err("no doorbell was ever rung; the relay is dead".to_owned());
    }
    // Leave the segment open: the parent decides when it closes.
    std::thread::sleep(Duration::from_millis(50));
    Ok(())
}

/// The consumer child: attach, register its doorbell, read and verify.
fn run_consumer(until_producer_gone: bool) -> Result<(), String> {
    let key = child_key();
    let socket = child_socket();
    let count = child_count();

    let mut client = SegmentClient::connect(&socket).map_err(|err| err.to_string())?;
    let segment = Arc::new(client.attach(&key).map_err(|err| err.to_string())?);
    let mut consumer = Consumer::attach(
        Arc::clone(&segment),
        AttachOptions::default().expecting(key.clone()),
    )
    .map_err(|err| err.to_string())?;

    let ringer = consumer
        .doorbell()
        .ok_or("a doorbell was requested but not created")?
        .ringer()
        .map_err(|err| err.to_string())?;
    client
        .register_doorbell(&key, consumer.index(), consumer.token(), ringer)
        .map_err(|err| err.to_string())?;

    let mut received = 0u64;
    let deadline = Instant::now() + CHILD_DEADLINE;
    loop {
        if Instant::now() > deadline {
            return Err(format!("timed out after {received} messages"));
        }
        match consumer.next_blocking(Duration::from_millis(500)) {
            Ok(sample) => {
                verify(sample.payload(), sample.seq())?;
                if sample.metadata() != sample.seq().to_le_bytes() {
                    return Err(format!("metadata for sequence {} is wrong", sample.seq()));
                }
                // The product claim: a payload mapped in a *different
                // process* is still 128-byte aligned, so it is directly
                // usable as a SIMD source (§6.1).
                if sample.payload_address() % 128 != 0 {
                    return Err(format!(
                        "sequence {} is misaligned at {:#x}",
                        sample.seq(),
                        sample.payload_address()
                    ));
                }
                received += 1;
                if !until_producer_gone && received >= count {
                    return Ok(());
                }
            }
            Err(RecvError::Empty) => {}
            Err(RecvError::Closed) => {
                return if until_producer_gone {
                    // A drained close is the other legal way to observe the
                    // producer's death: the broker got there first.
                    Ok(())
                } else {
                    Err(format!("segment closed after only {received} messages"))
                };
            }
            Err(RecvError::Shm(ShmError::ProducerGone { .. })) => {
                return if until_producer_gone {
                    if received == 0 {
                        Err("the producer vanished before delivering anything".to_owned())
                    } else {
                        Ok(())
                    }
                } else {
                    Err("the producer vanished unexpectedly".to_owned())
                };
            }
            Err(other) => return Err(other.to_string()),
        }
    }
}

/// A consumer child that attaches, registers, and then simply waits to be
/// killed — the crashed-reader case the eviction pass exists for.
fn run_idle_consumer() -> Result<(), String> {
    let key = child_key();
    let socket = child_socket();
    let mut client = SegmentClient::connect(&socket).map_err(|err| err.to_string())?;
    let segment = Arc::new(client.attach(&key).map_err(|err| err.to_string())?);
    let consumer = Consumer::attach(Arc::clone(&segment), AttachOptions::default())
        .map_err(|err| err.to_string())?;
    let ringer = consumer
        .doorbell()
        .ok_or("no doorbell")?
        .ringer()
        .map_err(|err| err.to_string())?;
    client
        .register_doorbell(&key, consumer.index(), consumer.token(), ringer)
        .map_err(|err| err.to_string())?;
    // Signal readiness by leaving the entry occupied, then wait to be killed.
    // `std::mem::forget` keeps the consumer-table entry claimed even if the
    // runtime unwinds this frame for any reason.
    std::mem::forget(consumer);
    std::thread::sleep(Duration::from_secs(600));
    Err("the idle consumer was never killed".to_owned())
}

/// A child that must be refused because it asks for the wrong generation.
fn run_stale_consumer() -> Result<(), String> {
    let key = child_key();
    let socket = child_socket();
    let mut client = SegmentClient::connect(&socket).map_err(|err| err.to_string())?;

    // The broker has no segment at this generation at all.
    match client.attach(&key.clone().with_generation(key.generation() + 1)) {
        Err(ShmError::BrokerRefused { .. }) => {}
        Ok(_) => return Err("the broker served an unknown generation".to_owned()),
        Err(other) => return Err(format!("unexpected broker error: {other}")),
    }

    // And the live segment must refuse a mismatched expectation even after a
    // successful descriptor handover — §6.2: *every* attach path verifies.
    let segment = Arc::new(client.attach(&key).map_err(|err| err.to_string())?);
    let stale = key.clone().with_generation(key.generation() + 7);
    match Consumer::attach(
        Arc::clone(&segment),
        AttachOptions::default().expecting(stale),
    ) {
        Err(ShmError::StaleGeneration { .. }) => Ok(()),
        Ok(_) => Err("a stale generation was accepted".to_owned()),
        Err(other) => Err(format!("unexpected attach error: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Parent side
// ---------------------------------------------------------------------------

struct Fixture {
    broker: Arc<SegmentBroker>,
    key: SegmentKey,
    socket: std::path::PathBuf,
    _handle: astrs_shm::BrokerHandle,
}

impl Fixture {
    fn new(label: &str, policy: OverflowPolicy, slots: u32) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let socket = std::env::temp_dir().join(format!(
            "as-mp-{label}-{}-{unique}.sock",
            std::process::id()
        ));
        let broker = SegmentBroker::bind(&socket).expect("bind broker");
        let key =
            SegmentKey::from_parts(DataflowId::generate(), NODE, OUTPUT, 3).expect("valid ids");
        let config = SegmentConfig::new(slots, 4096)
            .expect("valid geometry")
            .with_overflow(policy)
            .with_max_consumers(8)
            .expect("valid consumer table");
        broker
            .create_segment(key.clone(), config)
            .expect("create segment");
        let handle = SegmentBroker::spawn(&broker);
        Self {
            broker,
            key,
            socket,
            _handle: handle,
        }
    }

    fn spawn(&self, role: &str, count: u64, peers: usize) -> Child {
        let exe = std::env::current_exe().expect("current exe");
        Command::new(exe)
            // The child re-enters this binary at `child_entrypoint`; libtest's
            // own filter arguments are what select it.
            .args(["--exact", "child_entrypoint", "--nocapture"])
            .env(ROLE_ENV, role)
            .env(SOCKET_ENV, &self.socket)
            .env(
                DATAFLOW_ENV,
                format!("{:032x}", self.key.dataflow().as_u128()),
            )
            .env(GENERATION_ENV, self.key.generation().to_string())
            .env(COUNT_ENV, count.to_string())
            .env(PEERS_ENV, peers.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn child")
    }
}

/// Wait for a child, failing the test if it does not finish in time or exits
/// nonzero.
fn expect_success(mut child: Child, what: &str) {
    let deadline = Instant::now() + CHILD_DEADLINE;
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                assert!(status.success(), "{what} exited with {status}");
                return;
            }
            None => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{what} did not finish within {CHILD_DEADLINE:?}");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[test]
fn frames_cross_a_real_process_boundary() {
    const MESSAGES: u64 = 400;
    const CONSUMERS: usize = 2;

    let fixture = Fixture::new("happy", OverflowPolicy::Block, 8);

    // Consumers first: the producer child blocks until their doorbells have
    // been relayed to it, which is what makes the relay assertion meaningful.
    let consumers: Vec<Child> = (0..CONSUMERS)
        .map(|_| fixture.spawn("consumer", MESSAGES, 0))
        .collect();
    let producer = fixture.spawn("producer", MESSAGES, CONSUMERS);

    expect_success(producer, "producer child");
    for (index, consumer) in consumers.into_iter().enumerate() {
        expect_success(consumer, &format!("consumer child {index}"));
    }

    let segment = fixture
        .broker
        .segment(fixture.key.digest(), fixture.key.generation())
        .expect("the broker still holds the segment");
    assert_eq!(segment.header().write_seq(), MESSAGES);
    assert!(
        fixture.broker.stats().doorbells_relayed >= CONSUMERS as u64,
        "every consumer's doorbell must have been relayed to the producer"
    );
    assert!(fixture.broker.stats().served >= 3);
}

#[test]
fn consumers_survive_a_kill_9_of_the_producer() {
    const CONSUMERS: usize = 2;

    let fixture = Fixture::new("kill", OverflowPolicy::Overwrite, 8);

    let consumers: Vec<Child> = (0..CONSUMERS)
        .map(|_| fixture.spawn("consumer-until-gone", u64::MAX, 0))
        .collect();
    let mut producer = fixture.spawn("producer-forever", u64::MAX, CONSUMERS);

    // Let real frames flow before pulling the plug.
    let segment = fixture
        .broker
        .segment(fixture.key.digest(), fixture.key.generation())
        .expect("segment");
    let deadline = Instant::now() + Duration::from_secs(20);
    while segment.header().write_seq() < 20 {
        assert!(
            Instant::now() < deadline,
            "the producer child never got going (write_seq {})",
            segment.header().write_seq()
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    producer.kill().expect("SIGKILL the producer");
    // **Reaping matters.** `kill(pid, 0)` reports a *zombie* as alive, so
    // until the parent reaps the killed child every consumer's liveness check
    // would say the producer is still running and they would wait forever.
    // A daemon reaps its children for exactly this reason (§12).
    producer.wait().expect("reap the producer");

    // Deliberately do *not* call `poll_producers`: this test is the case
    // where the daemon died too and nobody is left to set `closed`. The
    // consumers must reach that conclusion on their own.
    for (index, consumer) in consumers.into_iter().enumerate() {
        expect_success(consumer, &format!("consumer child {index}"));
    }

    assert!(
        segment.header().is_closed(),
        "a consumer that detects a dead producer must mark the segment closed \
         so its peers converge on the same conclusion"
    );
}

#[test]
fn the_broker_closes_a_segment_whose_producer_was_killed() {
    let fixture = Fixture::new("broker-kill", OverflowPolicy::Overwrite, 8);
    let mut producer = fixture.spawn("producer-forever", u64::MAX, 0);

    let segment = fixture
        .broker
        .segment(fixture.key.digest(), fixture.key.generation())
        .expect("segment");
    let deadline = Instant::now() + Duration::from_secs(20);
    while segment.header().write_seq() < 5 {
        assert!(Instant::now() < deadline, "the producer never got going");
        std::thread::sleep(Duration::from_millis(10));
    }

    // `claim_producer` stamped the child's pid into the header, so the
    // broker's watch is aimed at the child rather than at this process.
    assert_ne!(
        segment.header().producer_pid(),
        i64::from(std::process::id()),
        "the producer child must have claimed the segment"
    );

    producer.kill().expect("SIGKILL");
    producer.wait().expect("reap");

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut closed = Vec::new();
    while closed.is_empty() {
        assert!(
            Instant::now() < deadline,
            "the broker never noticed the producer's death"
        );
        closed = fixture.broker.poll_producers();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0], fixture.key);
    assert!(segment.header().is_closed());
    assert_eq!(fixture.broker.stats().closed_on_death, 1);

    // No consumer ever attached, so the drain is trivially complete and the
    // sweep retires the segment.
    assert_eq!(fixture.broker.sweep(), 1);
    assert!(fixture.broker.is_empty());
}

#[test]
fn a_stale_generation_is_refused_across_processes() {
    let fixture = Fixture::new("stale", OverflowPolicy::Block, 4);
    let child = fixture.spawn("stale-consumer", 0, 0);
    expect_success(child, "stale consumer child");
}

#[test]
fn the_broker_evicts_a_consumer_whose_process_was_killed() {
    let fixture = Fixture::new("evict", OverflowPolicy::Block, 4);
    let segment = fixture
        .broker
        .segment(fixture.key.digest(), fixture.key.generation())
        .expect("segment");

    let mut consumer = fixture.spawn("consumer-idle", 0, 0);
    let deadline = Instant::now() + Duration::from_secs(20);
    while segment.header().attached_consumers() == 0 {
        assert!(
            Instant::now() < deadline,
            "the idle consumer child never attached"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // A *live* consumer is never evicted, however stale its heartbeat looks —
    // that is the half of the predicate that keeps a node blocked on a slow
    // sensor from losing its place in the ring.
    assert_eq!(fixture.broker.evict_stale_consumers(Duration::ZERO), 0);
    assert!(segment.consumer_entry(0).is_occupied());

    consumer.kill().expect("SIGKILL the consumer");
    // Reap, for the same reason the producer-kill test does: `kill(pid, 0)`
    // reports a zombie as alive, so the eviction predicate would refuse.
    consumer.wait().expect("reap the consumer");

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut evicted = 0;
    while evicted == 0 {
        assert!(
            Instant::now() < deadline,
            "the broker never evicted the dead consumer"
        );
        evicted = fixture.broker.evict_stale_consumers(Duration::ZERO);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(evicted, 1);
    assert_eq!(fixture.broker.stats().evicted_consumers, 1);
    assert!(!segment.consumer_entry(0).is_occupied());
    assert_eq!(
        segment.consumer_entry(0).cursor(),
        0,
        "an evicted entry must leave no cursor behind to block reclamation"
    );

    // With the crashed reader's cursor gone, the ring recycles again.
    let mut producer = Producer::new(Arc::clone(&segment)).expect("producer");
    let payload = vec![0u8; PAYLOAD_LEN];
    for _ in 0..32 {
        producer.send(&payload, b"").expect("send after eviction");
    }
}

#[test]
fn a_child_reads_a_backlog_the_parent_produced_before_it_started() {
    const MESSAGES: u64 = 6;

    let fixture = Fixture::new("backlog", OverflowPolicy::Block, 16);
    let segment = fixture
        .broker
        .segment(fixture.key.digest(), fixture.key.generation())
        .expect("segment");

    // The parent is the producer this time, writing before any consumer
    // process exists. `StartPosition::Oldest` must still deliver the backlog.
    let mut producer = Producer::new(Arc::clone(&segment)).expect("producer");
    let mut payload = vec![0u8; PAYLOAD_LEN];
    for seq in 1..=MESSAGES {
        stamp(&mut payload, seq);
        producer.send(&payload, &seq.to_le_bytes()).expect("send");
    }

    let consumer = fixture.spawn("consumer", MESSAGES, 0);
    expect_success(consumer, "backlog consumer child");
    assert_eq!(producer.stats().published, MESSAGES);
}
