//! The consumer-side zero-copy plane, end to end through a real daemon
//! (blueprint §6.2, §6.3).
//!
//! `tests/zero_copy.rs` drives the producer half against a
//! [`astrs_node_api::MockDaemon`], which is the right tool for "does the node
//! do the right thing when told X". This file answers the question a mock
//! cannot: **does anybody tell it X?** §6.3's handshake is a conversation
//! between three parties —
//!
//! ```text
//!   consumer subscribes ─────────────► daemon creates the ring
//!   daemon ── InputRouteUpgrade ─────► consumer attaches (astrs-node-api)
//!   consumer's pid appears in the segment's consumer table
//!   daemon ── RouteUpgrade ──────────► producer opens its ring end
//!   producer ── RouteUpgradeAck ─────► the route table says `shm`
//!   producer publishes ──────────────► consumer reads the slot in place
//! ```
//!
//! — and a mock standing in for two of them proves nothing about the third.
//! So: a real [`astrs_daemon::Daemon`] on a real Unix socket with a real
//! segment broker, and two real [`astrs_node_api::Node`]s that were written
//! without a line of shared-memory code in them.
//!
//! # Why the producer is a dynamic node and the consumer is not
//!
//! Both "processes" here are threads of the test process, so both would report
//! the same pid — and the daemon maps a pid in the segment's consumer table
//! back to a graph node through exactly that (`Daemon::pid_index`). Two nodes
//! claiming one pid make that mapping ambiguous.
//!
//! Only the *consumer's* pid has to map, so only the consumer is given one. The
//! producer is a `path: dynamic` node (§8.3), which the daemon never spawns and
//! never assigns a pid to. That is also why the roles cannot be swapped: §6.3
//! refuses to put a *dynamic consumer* on a ring
//! ([`astrs_daemon::shm::ConsumerFacts`]), so the consumer must be a declared,
//! spawned node — pretended-spawned here, with this process's pid, exactly as
//! `astrs-daemon`'s own `shm_lifecycle` tests do.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use astrs_daemon::{Daemon, DaemonConfig, ListenConfig, RuntimePaths, plan_dataflow};
use astrs_manifest::Manifest;
use astrs_node_api::session::RoutePlane;
use astrs_node_api::{Event, EventStream, Node, NodeBuilder};
use astrs_wire::{AuthToken, DaemonId, DataflowId, NodeConfig, NodeId, NodeSource, NodeSpawnSpec};

/// How long any "wait for the plane to settle" loop is given.
///
/// Generous, and only ever reached on failure: every wait below is
/// deadline-polled on a condition, never slept through.
const SETTLE: Duration = Duration::from_secs(20);

/// One polling step for a deadline-polled wait.
const STEP: Duration = Duration::from_millis(20);

/// Payload size for the frames that must take the ring: comfortably over the
/// 4 KiB §24.2 threshold and under the ring's slot size.
const FRAME_BYTES: usize = 32 * 1024;

/// Payload size for a message that must *not* take the ring, because §6.2
/// keeps it on the control channel.
const SMALL_BYTES: usize = 16;

/// A per-thread identity, stable within one test.
fn test_id() -> u32 {
    use std::cell::Cell;
    use std::sync::atomic::AtomicU32;
    static NEXT: AtomicU32 = AtomicU32::new(1);
    thread_local! {
        static ID: Cell<u32> = const { Cell::new(0) };
    }
    ID.with(|id| {
        if id.get() == 0 {
            id.set(NEXT.fetch_add(1, Ordering::Relaxed));
        }
        id.get()
    })
}

/// A dataflow id unique across processes as well as threads.
///
/// The POSIX shared-memory name a segment gets is derived from this, and that
/// namespace is machine-global: two test processes seeded from the same
/// thread-local counter would collide with `EEXIST`.
fn dataflow() -> DataflowId {
    DataflowId::from_u128(
        (u128::from(std::process::id()) << 32) | u128::from(0x6000_0000 + test_id()),
    )
}

/// `camera` publishes, `detect` consumes. See the module documentation for why
/// only one of them has a `path:`.
const PIPELINE: &str = "\
nodes:
  - id: camera
    path: dynamic
    outputs: [image, status]
    shm_pool_size: 1048576
  - id: detect
    path: /bin/sleep
    args: [\"600\"]
    inputs:
      frames:
        source: camera/image
        queue_size: 16
      health:
        source: camera/status
        queue_size: 16
";

/// A runtime directory short enough for `SUN_LEN` (104 bytes on macOS).
fn runtime_root() -> PathBuf {
    std::env::temp_dir().join(format!("ze{}-{}", std::process::id(), test_id()))
}

/// Something to do to the daemon between phases of a test.
type DaemonTask = Box<dyn FnOnce(&mut Daemon) + Send>;

/// A real daemon on a real socket, pumped by a thread of its own.
struct Harness {
    /// The Unix socket nodes dial.
    endpoint: String,
    /// The segment broker's socket, as a node's handshake blob would carry it.
    broker: Option<String>,
    /// Set to end the pump.
    stop: Arc<AtomicBool>,
    /// Work for the pump thread to apply to the daemon it owns.
    ///
    /// The daemon is a single-task actor by design (§4.3: *"one loop, and
    /// everything that must block lives in a task"*), so a test cannot simply
    /// reach into it. This is how the supervision a real deployment would do
    /// — a restart policy minting the next generation — is expressed from
    /// outside without breaking that rule.
    tasks: std::sync::mpsc::Sender<DaemonTask>,
    /// The pump.
    thread: Option<std::thread::JoinHandle<()>>,
    /// Where the runtime directory was, so it can be cleaned up.
    root: PathBuf,
}

impl Harness {
    /// Admits `PIPELINE`, binds the listener and starts pumping.
    fn start(dataflow: DataflowId) -> Option<Self> {
        let root = runtime_root();
        let _ = std::fs::create_dir_all(&root);
        let socket = root.join("d.sock");
        let config = DaemonConfig::new(RuntimePaths::under(root.clone()))
            .with_listen(ListenConfig::uds(socket.clone()))
            // Nothing is really spawned here, so the deadline that would fail
            // a node for not registering must not fire during the run.
            .with_spawn_deadline(Duration::from_secs(600));
        let mut daemon = Daemon::new(config).expect("a daemon");
        if !daemon.shm().is_enabled() {
            // A sandbox without a usable shared-memory backing is not a test
            // failure; the plane is an optimisation (§6.2).
            return None;
        }
        let broker = daemon
            .shm()
            .socket_path()
            .map(|path| path.display().to_string());

        let manifest = Manifest::from_yaml_str(PIPELINE).expect("a manifest");
        let plan = plan_dataflow(dataflow, &manifest, &BTreeMap::new()).expect("a plan");
        daemon.admit(&plan).expect("admitted");
        // Only the consumer gets a pid; see the module documentation.
        daemon
            .state_mut()
            .dataflow_mut(dataflow)
            .expect("admitted")
            .node_mut(&NodeId::new("detect").unwrap())
            .expect("declared")
            .mark_spawning(std::process::id());

        let stop = Arc::new(AtomicBool::new(false));
        let pump_stop = Arc::clone(&stop);
        let (tasks, inbox) = std::sync::mpsc::channel::<DaemonTask>();
        let thread = std::thread::Builder::new()
            .name("astrs-e2e-daemon".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("a runtime");
                runtime.block_on(async move {
                    daemon.bind().await.expect("the listener binds");
                    while !pump_stop.load(Ordering::Relaxed) {
                        while let Ok(task) = inbox.try_recv() {
                            task(&mut daemon);
                        }
                        daemon.pump(Duration::from_millis(10)).await;
                    }
                    daemon.close_peers().await;
                });
            })
            .expect("a daemon thread");

        let harness = Self {
            endpoint: socket.display().to_string(),
            broker,
            stop,
            tasks,
            thread: Some(thread),
            root,
        };
        // `bind` happens inside the pump thread, so the socket appears a
        // moment after this function would otherwise return — and a node that
        // dials first gets `ENOENT` rather than a retry.
        assert!(
            wait_until("the daemon's socket", || socket.exists()),
            "the daemon never bound {}",
            socket.display()
        );
        Some(harness)
    }

    /// Mints the next incarnation of `node`, as a restart policy would (§12).
    ///
    /// Waits for the pump thread to have applied it, so the phase that follows
    /// cannot race the bump.
    fn restart(&self, dataflow: DataflowId, node: &'static str) -> u64 {
        let (report, done) = std::sync::mpsc::channel();
        self.tasks
            .send(Box::new(move |daemon: &mut Daemon| {
                let generation = daemon
                    .state_mut()
                    .dataflow_mut(dataflow)
                    .expect("admitted")
                    .node_mut(&NodeId::new(node).unwrap())
                    .expect("declared")
                    .begin_next_generation();
                let _ = report.send(generation);
            }))
            .expect("the pump is running");
        done.recv_timeout(SETTLE).expect("the bump was applied")
    }

    /// The configuration blob a spawned node would have been handed.
    fn node_config(&self, dataflow: DataflowId, node: &str) -> NodeConfig {
        let spec = NodeSpawnSpec::new(dataflow, NodeId::new(node).unwrap(), 0, NodeSource::Dynamic);
        let mut config = NodeConfig::new(spec, DaemonId::generate(None), AuthToken::ZERO)
            .with_endpoint(self.endpoint.clone());
        if let Some(broker) = &self.broker {
            config = config.with_shm_broker(broker.clone());
        }
        config
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Connects the producer: a dynamic node declaring one output.
fn connect_producer(harness: &Harness, dataflow: DataflowId) -> (Node, EventStream) {
    NodeBuilder::new()
        .with_config(harness.node_config(dataflow, "camera"))
        .node_id("camera")
        .unwrap()
        .dataflow(dataflow)
        .daemon(harness.endpoint.clone())
        .dynamic(true)
        .output("image")
        .unwrap()
        .output("status")
        .unwrap()
        .connect()
        .expect("the producer registers")
}

/// Connects the consumer: a declared node the daemon believes it spawned.
fn connect_consumer(harness: &Harness, dataflow: DataflowId) -> (Node, EventStream) {
    NodeBuilder::new()
        .with_config(harness.node_config(dataflow, "detect"))
        .node_id("detect")
        .unwrap()
        .dataflow(dataflow)
        .daemon(harness.endpoint.clone())
        .connect()
        .expect("the consumer registers")
}

/// Polls `condition` until it holds or `SETTLE` elapses.
fn wait_until(what: &str, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + SETTLE;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(STEP);
    }
    eprintln!("timed out waiting for {what}");
    false
}

/// A deterministic payload of `len` bytes.
fn payload(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|index| (index as u8).wrapping_add(seed))
        .collect()
}

/// Publishes until **both** ends of the route are on the ring.
///
/// Publishing is what drives the handshake forward on the producer's side: the
/// upgrade is applied by [`astrs_node_api::RawOutput::refresh`], which every
/// publish calls. The two ends move in a fixed order — the consumer attaches
/// first, because §6.3 offers the producer its upgrade only once the daemon has
/// observed that attachment — so waiting on the producer's end waits for both.
fn drive_until_upgraded(
    producer: &mut astrs_node_api::RawOutput,
    consumer: &Node,
    events: &mut EventStream,
) -> bool {
    wait_until("both ends of the route to reach the ring", || {
        let metadata = producer.metadata();
        // Marked, so a test that counts *its own* frames can tell one of these
        // apart from one of its own: the last frame published here is still in
        // flight when the handshake completes.
        let _ = producer.send_bytes(payload(FRAME_BYTES, DRIVE_MARKER), metadata);
        // Drain, so the consumer's queue does not fill and its session keeps
        // processing the daemon's route events.
        while events.try_recv().is_some() {}
        consumer.input_plane(FRAMES) == RoutePlane::Shm && producer.plane().is_zero_copy()
    })
}

/// The consumer's input name.
const FRAMES: &str = "frames";

/// The consumer's second input, fed by an output that only ever carries
/// sub-threshold payloads.
const HEALTH: &str = "health";

/// The first payload byte of a frame published only to drive the handshake.
///
/// [`drive_until_upgraded`] publishes until both ends are on the ring, and the
/// last of those frames is still travelling when it returns. A test that
/// counts its own frames filters on this.
const DRIVE_MARKER: u8 = 0xFF;

#[test]
fn a_consumer_attaches_to_the_ring_without_being_told_how() {
    let dataflow = dataflow();
    let Some(harness) = Harness::start(dataflow) else {
        return;
    };
    let (producer, _producer_events) = connect_producer(&harness, dataflow);
    let (consumer, mut events) = connect_consumer(&harness, dataflow);

    let mut producer = producer;
    let mut image = producer.raw_output("image").expect("a declared output");
    assert!(
        drive_until_upgraded(&mut image, &consumer, &mut events),
        "the consumer never reached the shared-memory plane"
    );

    // The node wrote no shared-memory code: the session attached because the
    // daemon said which segment feeds this input (§6.3).
    let stats = consumer.input_plane_stats();
    assert!(stats.attaches >= 1, "{stats:?}");
    assert_eq!(stats.refusals, 0, "{stats:?}");
    // `health` is fed by the pipeline's second output and may or may not have
    // reached its ring yet; `frames` is the one this test drove.
    assert!(
        consumer
            .input_planes()
            .contains(&(astrs_wire::DataId::new(FRAMES).unwrap(), RoutePlane::Shm)),
        "{:?}",
        consumer.input_planes()
    );
    let segment = consumer
        .input_segment(FRAMES)
        .expect("the input names its segment");
    assert!(
        segment.name.contains("/camera/image/"),
        "the segment is the producer's ring: {segment:?}"
    );
    assert!(
        image.plane().is_zero_copy(),
        "both ends of the same route are on the ring"
    );

    // And the payloads really come out of the mapping — the M1 probe's
    // pointer-in-mapping assertion, made through the public API.
    let mut zero_copy = 0_u32;
    let mut slots: BTreeMap<u32, usize> = BTreeMap::new();
    let found = wait_until("zero-copy frames to arrive", || {
        let metadata = image.metadata();
        let _ = image.send_bytes(payload(FRAME_BYTES, 1), metadata);
        while let Some(event) = events.try_recv() {
            let Event::Input { id, data, .. } = event else {
                continue;
            };
            if id.as_str() != FRAMES || !data.is_zero_copy() {
                continue;
            }
            let slot = data.slot().expect("a zero-copy payload occupies a slot");
            assert!(slot.is_inside_mapping(), "{slot:?}");
            assert!(slot.is_at_layout_offset(), "{slot:?}");
            assert!(
                slot.address.is_multiple_of(128),
                "§6.1 promises a 128-byte aligned payload: {slot:?}"
            );
            match slots.get(&slot.slot) {
                Some(&seen) => assert_eq!(seen, slot.address, "a ring slot moved: {slot:?}"),
                None => {
                    let _ = slots.insert(slot.slot, slot.address);
                }
            }
            assert_eq!(data.len(), FRAME_BYTES);
            zero_copy += 1;
        }
        zero_copy >= 4
    });
    assert!(found, "no zero-copy frame arrived; {zero_copy} seen");
    assert!(
        slots.len() <= segment.slot_count as usize,
        "the addresses did not cycle through the ring: {slots:?}"
    );
    assert!(consumer.input_plane_stats().samples >= u64::from(zero_copy));
}

#[test]
fn one_small_message_on_an_upgraded_route_still_arrives() {
    // The regression guard for §6.2's below-threshold rule. A producer whose
    // output is on the ring still publishes small payloads through the daemon,
    // and the daemon used to skip every consumer that was on the ring — which
    // dropped the message outright. *One* message with nothing behind it is
    // what separates "delivered" from "queued and forgotten": a stream of them
    // passes either way, because each one flushes the previous.
    let dataflow = dataflow();
    let Some(harness) = Harness::start(dataflow) else {
        return;
    };
    let (mut producer, _producer_events) = connect_producer(&harness, dataflow);
    let (consumer, mut events) = connect_consumer(&harness, dataflow);
    let mut image = producer.raw_output("image").expect("a declared output");
    assert!(drive_until_upgraded(&mut image, &consumer, &mut events));

    // Nothing is published after this one, deliberately.
    let metadata = image.metadata();
    image
        .send_bytes(payload(SMALL_BYTES, 3), metadata)
        .expect("a small publish");

    let mut seen: Option<Vec<u8>> = None;
    let arrived = wait_until("the small message", || {
        while let Some(event) = events.try_recv() {
            if let Event::Input { id, data, .. } = event
                && id.as_str() == FRAMES
                && data.len() == SMALL_BYTES
            {
                assert!(
                    !data.is_zero_copy(),
                    "§6.2 keeps a sub-threshold payload on the control channel"
                );
                seen = Some(data.to_vec());
            }
        }
        seen.is_some()
    });
    assert!(
        arrived,
        "a sub-threshold message published on an upgraded route was lost"
    );
    assert_eq!(seen.as_deref(), Some(payload(SMALL_BYTES, 3).as_slice()));
    assert_eq!(
        consumer.input_plane(FRAMES),
        RoutePlane::Shm,
        "the route did not have to leave the ring to deliver it"
    );
}

#[test]
fn the_last_frame_of_a_stream_survives_the_close() {
    // The closure of an input travels on the control connection; the frame it
    // closes travels on the ring. The control connection routinely wins, so a
    // consumer that stopped reading on `InputClosed` — every consumer — would
    // lose the last frame of every stream. `session::inputs::close_input`
    // hands the closure to the ring reader instead, which drains and *then*
    // pushes it.
    const COUNT: u64 = 6;
    let dataflow = dataflow();
    let Some(harness) = Harness::start(dataflow) else {
        return;
    };
    let (mut producer, _producer_events) = connect_producer(&harness, dataflow);
    let (consumer, mut events) = connect_consumer(&harness, dataflow);
    let mut image = producer.raw_output("image").expect("a declared output");
    assert!(drive_until_upgraded(&mut image, &consumer, &mut events));

    // Numbered frames, so a gap is nameable rather than merely a count.
    for index in 0..COUNT {
        let metadata = image.metadata();
        image
            .send_bytes(payload(FRAME_BYTES, index as u8), metadata)
            .expect("a publish");
    }
    image.close().expect("the output closes");

    let mut seen: Vec<u8> = Vec::new();
    let mut closed = false;
    let drained = wait_until("every frame and then the closure", || {
        while let Some(event) = events.try_recv() {
            match event {
                Event::Input { id, data, .. } if id.as_str() == FRAMES => {
                    let marker = data.bytes()[0];
                    if marker == DRIVE_MARKER {
                        continue;
                    }
                    assert!(!closed, "a frame arrived *after* the closure that ended it");
                    seen.push(marker);
                }
                Event::InputClosed { id, .. } if id.as_str() == FRAMES => closed = true,
                _ => {}
            }
        }
        closed
    });
    assert!(drained, "the input never closed");
    assert_eq!(
        seen.len() as u64,
        COUNT,
        "frames {seen:?} arrived out of {COUNT}"
    );
    assert_eq!(
        seen,
        (0..COUNT).map(|index| index as u8).collect::<Vec<u8>>(),
        "the ring delivered its frames in order"
    );
}

#[test]
fn a_second_output_that_only_carries_small_messages_still_delivers() {
    // The probe's shape, in process. `status` is an output whose payloads are
    // *always* below §6.2's threshold, so its ring is created, attached and
    // upgraded — and then never written to, because every message it carries
    // rides the control channel instead. A consumer reading that ring must
    // still be delivered to, and the delivery must not depend on the *other*
    // output's traffic.
    let dataflow = dataflow();
    let Some(harness) = Harness::start(dataflow) else {
        return;
    };
    let (mut producer, _producer_events) = connect_producer(&harness, dataflow);
    let (consumer, mut events) = connect_consumer(&harness, dataflow);
    let mut image = producer.raw_output("image").expect("a declared output");
    let mut status = producer.raw_output("status").expect("a declared output");

    // Both routes reach the ring; `status` gets there without ever writing to
    // it, which is exactly the case under test.
    assert!(drive_until_upgraded(&mut image, &consumer, &mut events));
    assert!(
        wait_until("the status route to reach the ring", || {
            let metadata = status.metadata();
            let _ = status.send_bytes(payload(SMALL_BYTES, DRIVE_MARKER), metadata);
            while events.try_recv().is_some() {}
            consumer.input_plane(HEALTH) == RoutePlane::Shm && status.plane().is_zero_copy()
        }),
        "the second output never reached the shared-memory plane"
    );

    // One small message on the upgraded `status` route, and nothing after it.
    let metadata = status.metadata();
    status
        .send_bytes(payload(SMALL_BYTES, 5), metadata)
        .expect("a small publish");

    let mut seen: Option<Vec<u8>> = None;
    let arrived = wait_until("the small message on the second route", || {
        while let Some(event) = events.try_recv() {
            if let Event::Input { id, data, .. } = event
                && id.as_str() == HEALTH
                && data.bytes().first() == Some(&5)
            {
                assert!(!data.is_zero_copy(), "§6.2 keeps it on the control channel");
                seen = Some(data.to_vec());
            }
        }
        seen.is_some()
    });
    assert!(
        arrived,
        "a sub-threshold message on an output that only ever carries them was lost"
    );
    assert_eq!(seen.as_deref(), Some(payload(SMALL_BYTES, 5).as_slice()));
}

#[test]
fn a_dead_producer_takes_the_consumer_off_the_ring() {
    let dataflow = dataflow();
    let Some(harness) = Harness::start(dataflow) else {
        return;
    };
    let (mut producer, producer_events) = connect_producer(&harness, dataflow);
    let (consumer, mut events) = connect_consumer(&harness, dataflow);
    let mut image = producer.raw_output("image").expect("a declared output");
    assert!(drive_until_upgraded(&mut image, &consumer, &mut events));
    assert_eq!(consumer.input_plane(FRAMES), RoutePlane::Shm);

    // The producer goes away the way a crashed process does: its session ends
    // and its ring end is dropped, with no goodbye of its own.
    drop(image);
    drop(producer_events);
    drop(producer);

    assert!(
        wait_until("the consumer to leave the ring", || {
            while events.try_recv().is_some() {}
            consumer.input_plane(FRAMES) == RoutePlane::Daemon
        }),
        "the consumer stayed attached to a dead producer's ring"
    );
    let stats = consumer.input_plane_stats();
    assert!(stats.detaches >= 1, "{stats:?}");
}

#[test]
fn a_restarted_producer_is_followed_to_its_new_generation() {
    let dataflow = dataflow();
    let Some(harness) = Harness::start(dataflow) else {
        return;
    };
    let (consumer, mut events) = connect_consumer(&harness, dataflow);

    let first = {
        let (mut producer, producer_events) = connect_producer(&harness, dataflow);
        let mut image = producer.raw_output("image").expect("a declared output");
        assert!(drive_until_upgraded(&mut image, &consumer, &mut events));
        let segment = consumer.input_segment(FRAMES).expect("a segment");
        drop(image);
        drop(producer_events);
        drop(producer);
        segment
    };

    assert!(
        wait_until("the first ring to be released", || {
            while events.try_recv().is_some() {}
            consumer.input_plane(FRAMES) == RoutePlane::Daemon
        }),
        "the consumer stayed on a ring whose producer had gone"
    );

    // The supervision a real deployment performs between a crash and a
    // respawn (§12): the node's incarnation counter moves on, so the ring the
    // next producer gets is a *different* segment and every stale mapping of
    // the old one is detectable (§6.2).
    let generation = harness.restart(dataflow, "camera");
    assert_eq!(generation, first.generation + 1);

    // A new incarnation of the same node: a new session, a new generation, a
    // new ring — and the consumer must follow it there without anybody
    // restarting the consumer.
    let (mut producer, _producer_events) = connect_producer(&harness, dataflow);
    let mut image = producer.raw_output("image").expect("a declared output");
    assert!(
        drive_until_upgraded(&mut image, &consumer, &mut events),
        "the consumer did not re-attach after the producer came back"
    );

    let second = consumer.input_segment(FRAMES).expect("a segment");
    assert_ne!(
        second.generation, first.generation,
        "a restart must mint a new generation: {first:?} then {second:?}"
    );
    assert_ne!(second.name, first.name, "and a new segment with it");
    let stats = consumer.input_plane_stats();
    assert!(stats.attaches >= 2, "{stats:?}");
    assert!(stats.detaches >= 1, "{stats:?}");
}
