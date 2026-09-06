//! The zero-copy plane, against a real shared-memory segment (blueprint §6.2,
//! §6.3).
//!
//! Both ends live in this process, but nothing else is simulated: the segment
//! is a real `shm_open`/`memfd` mapping, the producer is a real
//! [`astrs_shm::Producer`] the node opened from the daemon's `RouteUpgrade`,
//! and the consumer reads the ring slot the node wrote into. What the test
//! asserts is the property that matters — the bytes the consumer reads are the
//! bytes the node wrote, and no copy of them crossed the daemon.
//!
//! A sandbox that cannot create a shared-memory segment skips rather than
//! fails: §6.2 makes the plane an optimisation, and a node that cannot use it
//! stays on the reliable path.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use astrs_node_api::prelude::*;
use astrs_node_api::session::RoutePlane;
use astrs_node_api::testing::MockDaemon;
use astrs_shm::{
    AttachOptions, Backing, Consumer, Segment, SegmentConfig, SegmentKey, StartPosition,
};
use astrs_wire::{
    NodeRequest, NodeSource, NodeSpawnSpec, OutputSpec, RouteDowngradeReason, ShmSegmentSpec,
};

/// The deadline every wait in this file uses.
const WAIT: Duration = Duration::from_secs(5);

/// The ring geometry: four slots of 8 KiB, comfortably over the 4 KiB
/// zero-copy threshold (§24.2).
const SLOTS: u32 = 4;
/// Bytes per slot.
const SLOT_BYTES: u32 = 8192;

/// Everything a zero-copy test needs, or `None` on a platform without the
/// plane.
struct Plane {
    daemon: MockDaemon,
    node: Node,
    events: EventStream,
    segment: Arc<Segment>,
    key: SegmentKey,
}

/// A dataflow id unique to this test *and this process*.
///
/// Segment names hash `{dataflow}/{node}/{output}/{generation}` (§6.2), so a
/// fixed id would collide between two concurrent runs of the suite — which is
/// exactly what a `cargo test` in a loop does. Mixing in the process id and a
/// monotone counter keeps every run's names to itself.
fn unique_dataflow(tag: u128) -> astrs_wire::DataflowId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = u128::from(COUNTER.fetch_add(1, Ordering::Relaxed));
    let pid = u128::from(std::process::id());
    astrs_wire::DataflowId::from_u128((tag << 96) | (pid << 32) | nonce)
}

/// Builds the plane, or `None` on a platform that has no shared memory.
///
/// On Unix the segment creation is *asserted*, not skipped: a silent `return`
/// on every test would make this file pass while testing nothing, which is the
/// one outcome worse than a red build.
fn set_up(tag: u128) -> Option<Plane> {
    let dataflow = unique_dataflow(tag);
    let daemon = MockDaemon::start_with(
        astrs_node_api::runtime::NodeRuntime::acquire().unwrap(),
        dataflow,
        astrs_wire::AuthToken::ZERO,
    )
    .unwrap();
    let node_id = NodeId::new("camera").unwrap();
    let output = DataId::new("image").unwrap();
    let spec = NodeSpawnSpec::new(dataflow, node_id.clone(), 0, NodeSource::Dynamic)
        .with_output(OutputSpec::new(output.clone()));
    let (node, events) = daemon.connect_node(spec).unwrap();

    let key = SegmentKey::new(dataflow, node_id, output, 0);
    // `Backing::Named` is not a detail: `MockDaemon` publishes no segment
    // broker, so `open_producer` takes its by-name fallback
    // (`Segment::open_named` on `key.os_name()`). `Backing::Auto` resolves to
    // an anonymous `memfd` on Linux — no filesystem name to open — so an
    // `Auto` segment here would leave the node refusing every upgrade with
    // `shm_open: ENOENT` and this file passing only on macOS, where `Auto`
    // happens to be `Named`. The broker path has its own file,
    // `zero_copy_e2e.rs`; this one exercises the no-broker route deliberately.
    let config = SegmentConfig::new(SLOTS, SLOT_BYTES)
        .unwrap()
        .with_backing(Backing::Named);
    let created = Segment::create(key.clone(), config);
    if cfg!(unix) {
        let segment = created
            .expect("a Unix host must be able to create a shared-memory segment")
            .shared();
        return Some(Plane {
            daemon,
            node,
            events,
            segment,
            key,
        });
    }
    // A platform without the plane (§6.2: Windows in 0.1.0) has nothing to
    // test here.
    let segment = created.ok()?.shared();
    Some(Plane {
        daemon,
        node,
        events,
        segment,
        key,
    })
}

/// The specification the daemon sends for `key`.
///
/// The name is the **canonical** identity (`{dataflow}/{node}/{output}/
/// {generation}`), which is what `ShmSegmentSpec::name` is documented to
/// carry and what `astrs-daemon`'s `SegmentRegistry` fills it with. The
/// hashed `shm_open` name is derived from the key by whoever needs to call
/// the OS, and never travels on the wire.
fn segment_spec(key: &SegmentKey) -> ShmSegmentSpec {
    ShmSegmentSpec::new(key.canonical(), 0, SLOTS, u64::from(SLOT_BYTES))
}

/// Waits for the node to acknowledge the upgrade (§6.3).
fn await_ack(daemon: &MockDaemon) -> bool {
    daemon
        .wait_for(WAIT, |requests| {
            requests.iter().any(|entry| {
                matches!(
                    entry.request,
                    NodeRequest::RouteUpgradeAck { accepted: true, .. }
                )
            })
        })
        .is_ok()
}

#[test]
fn a_large_payload_takes_the_ring_and_the_consumer_reads_it_in_place() {
    let Some(plane) = set_up(8193) else {
        return;
    };
    let Plane {
        daemon,
        mut node,
        events,
        segment,
        key,
    } = plane;
    let output = DataId::new("image").unwrap();

    // A consumer attaches before anything is published, from the oldest
    // sequence, so nothing can be missed.
    let mut consumer = Consumer::attach(
        Arc::clone(&segment),
        AttachOptions::new().with_start(StartPosition::Oldest),
    )
    .expect("attach a consumer");

    daemon
        .upgrade_route(node.id(), &output, segment_spec(&key), Vec::new())
        .unwrap();
    assert!(await_ack(&daemon), "the node accepted the upgrade");

    let mut image = node.raw_output("image").unwrap();
    // The first publish picks the upgrade up; both are over the 4 KiB
    // threshold, so both take the ring.
    let payload: Vec<u8> = (0..6000u32).map(|value| (value % 251) as u8).collect();
    image.send_bytes(&payload, node.metadata()).unwrap();
    image.send_bytes(&payload, node.metadata()).unwrap();
    assert_eq!(image.plane(), RoutePlane::Shm);

    // The daemon saw slot *references*, not bytes.
    let sends = daemon.wait_for_sends(node.id(), &output, 2, WAIT).unwrap();
    let zero_copy = sends
        .iter()
        .filter(|send| send.payload.is_zero_copy())
        .count();
    assert!(
        zero_copy >= 1,
        "at least one publish took the ring: {sends:?}"
    );
    let reference = sends
        .iter()
        .find(|send| send.payload.is_zero_copy())
        .unwrap();
    assert_eq!(reference.payload.len(), payload.len() as u64);
    assert_eq!(
        reference.payload.segment(),
        Some(key.canonical().as_str()),
        "the reference names the segment the daemon offered"
    );

    // And the consumer reads exactly those bytes out of the mapping.
    let sample = consumer.recv().expect("a sample");
    assert_eq!(sample.payload(), payload.as_slice());
    assert!(
        sample.payload_address().is_multiple_of(128),
        "§6.1 promises a 128-byte aligned payload"
    );

    // The metadata rode beside it in the slot.
    let slot_metadata = sample.metadata();
    assert!(!slot_metadata.is_empty(), "the slot carries the metadata");

    let stats = node.stats();
    assert!(stats.sends_zero_copy >= 1);
    drop(events);
    let _ = segment.unlink();
}

#[test]
fn a_payload_below_the_threshold_stays_on_the_daemon_path() {
    let Some(plane) = set_up(8194) else {
        return;
    };
    let Plane {
        daemon,
        mut node,
        events,
        segment,
        key,
    } = plane;
    let output = DataId::new("image").unwrap();

    daemon
        .upgrade_route(node.id(), &output, segment_spec(&key), Vec::new())
        .unwrap();
    assert!(await_ack(&daemon));

    let mut image = node.raw_output("image").unwrap();
    image.send_bytes(vec![1, 2, 3], node.metadata()).unwrap();

    let sends = daemon.wait_for_sends(node.id(), &output, 1, WAIT).unwrap();
    assert!(
        !sends[0].payload.is_zero_copy(),
        "a 3-byte pose does not repay a ring slot (§6.2)"
    );
    assert_eq!(sends[0].bytes(), Some(&[1, 2, 3][..]));
    drop(events);
    let _ = segment.unlink();
}

#[test]
fn allocate_writes_straight_into_the_slot() {
    let Some(plane) = set_up(8195) else {
        return;
    };
    let Plane {
        daemon,
        mut node,
        events,
        segment,
        key,
    } = plane;
    let output = DataId::new("image").unwrap();

    let mut consumer = Consumer::attach(
        Arc::clone(&segment),
        AttachOptions::new().with_start(StartPosition::Oldest),
    )
    .expect("attach a consumer");

    daemon
        .upgrade_route(node.id(), &output, segment_spec(&key), Vec::new())
        .unwrap();
    assert!(await_ack(&daemon));

    let mut image = node.raw_output("image").unwrap();
    image.refresh();
    assert_eq!(image.plane(), RoutePlane::Shm);

    let metadata = node.metadata();
    let mut sample = image.allocate(5000).unwrap();
    assert!(sample.is_zero_copy(), "the buffer is the slot itself");
    assert_eq!(sample.len(), 5000);
    assert!(sample.sequence().is_some());
    for (index, byte) in sample.as_mut_slice().iter_mut().enumerate() {
        *byte = (index % 7) as u8;
    }
    sample.send(metadata).unwrap();

    let received = consumer.recv().expect("a sample");
    assert_eq!(received.len(), 5000);
    assert_eq!(received.payload()[8], 1);
    assert_eq!(received.payload()[4999], (4999 % 7) as u8);

    drop(events);
    let _ = segment.unlink();
}

#[test]
fn a_downgrade_takes_the_output_back_to_the_daemon_path() {
    let Some(plane) = set_up(8196) else {
        return;
    };
    let Plane {
        daemon,
        mut node,
        events,
        segment,
        key,
    } = plane;
    let output = DataId::new("image").unwrap();

    daemon
        .upgrade_route(node.id(), &output, segment_spec(&key), Vec::new())
        .unwrap();
    assert!(await_ack(&daemon));

    let mut image = node.raw_output("image").unwrap();
    let payload = vec![7u8; 6000];
    image.send_bytes(&payload, node.metadata()).unwrap();
    assert_eq!(image.plane(), RoutePlane::Shm);

    daemon
        .downgrade_route(
            node.id(),
            &output,
            RouteDowngradeReason::ConsumerDetached {
                consumer: PortRef::from_parts("detect", "frames").unwrap(),
            },
        )
        .unwrap();
    // Give the downgrade time to reach the route slot, then publish again.
    for _ in 0..100 {
        image.refresh();
        if image.plane() == RoutePlane::Daemon {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        image.plane(),
        RoutePlane::Daemon,
        "§6.3 flips the route back"
    );

    image.send_bytes(&payload, node.metadata()).unwrap();
    let sends = daemon.wait_for_sends(node.id(), &output, 2, WAIT).unwrap();
    let last = sends.last().unwrap();
    assert!(!last.payload.is_zero_copy(), "back on the reliable path");
    assert_eq!(last.bytes().map(<[u8]>::len), Some(6000));

    drop(events);
    let _ = segment.unlink();
}

#[test]
fn an_unopenable_segment_is_refused_and_the_node_keeps_publishing() {
    let daemon = MockDaemon::start().unwrap();
    let spec = NodeSpawnSpec::new(
        daemon.dataflow(),
        NodeId::new("camera").unwrap(),
        0,
        NodeSource::Dynamic,
    )
    .with_output(OutputSpec::new(DataId::new("image").unwrap()));
    let (mut node, mut events) = daemon.connect_node(spec).unwrap();
    let output = DataId::new("image").unwrap();

    // A segment name that does not match this node's key.
    daemon
        .upgrade_route(
            node.id(),
            &output,
            ShmSegmentSpec::new("astrs-not-ours", 0, SLOTS, u64::from(SLOT_BYTES)),
            Vec::new(),
        )
        .unwrap();

    daemon
        .wait_for(WAIT, |requests| {
            requests.iter().any(|entry| {
                matches!(
                    entry.request,
                    NodeRequest::RouteUpgradeAck {
                        accepted: false,
                        ..
                    }
                )
            })
        })
        .unwrap();

    // The refusal is reported to the node, and publishing still works.
    let event = events.recv_timeout(WAIT).unwrap().expect("the report");
    assert!(matches!(event, Event::Error(_)), "{event}");

    let mut image = node.raw_output("image").unwrap();
    image.send_bytes(vec![9u8; 6000], node.metadata()).unwrap();
    let sends = daemon.wait_for_sends(node.id(), &output, 1, WAIT).unwrap();
    assert!(!sends[0].payload.is_zero_copy());
    assert_eq!(node.stats().shm_fallbacks, 1);
}
