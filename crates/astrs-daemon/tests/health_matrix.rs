//! The post-registration liveness matrix (§12).
//!
//! A table of (what the node did, how long it stayed quiet) against (is it
//! failed?), driven end to end through a real [`astrs_daemon::Daemon`] with
//! scripted nodes.
//!
//! # Two clocks, on purpose
//!
//! Deadlines in this crate are [`std::time::Instant`] arithmetic — the same
//! discipline the spawn deadline and the finish watchdog follow — so the tests
//! below hand the daemon *synthetic instants* rather than sleeping. The HLC is
//! a different clock with a different job: it stamps events for causal
//! ordering (§4.3, §14), and [`astrs_time::ManualClock`] is what makes those
//! stamps reproducible. Both appear here, doing their own jobs, because
//! conflating them is the mistake that makes a timing test flaky.
//!
//! `a_silent_node_is_failed_with_a_typed_cause` below is §12's conformance
//! zoo scenario 13/16 (silent-source, health timeout) — see the zoo's
//! full 16-scenario list in §12 and the mapping note in
//! `crates/astrs-daemon/tests/node_roles.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use astrs_daemon::health::{HealthTable, LivenessState};
use astrs_daemon::server::NodeListeners;
use astrs_daemon::{Daemon, DaemonConfig, ListenConfig, RuntimePaths, plan_dataflow};
use astrs_manifest::Manifest;
use astrs_time::{HlcClock, ManualClock};
use astrs_wire::io::{AsyncFrameReader, AsyncFrameWriter};
use astrs_wire::{
    DataId, DataflowId, FrameLimits, NodeEvent, NodeExitCause, NodeHandshake, NodeId, NodeRequest,
    SessionId,
};
use tokio::io::DuplexStream;

fn dataflow() -> DataflowId {
    DataflowId::from_u128(0x0EA1_7000)
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}

fn data(name: &str) -> DataId {
    DataId::new(name).unwrap()
}

/// `camera` names a `health_check_timeout`; `detect` does not.
const PIPELINE: &str = "\
nodes:
  - id: camera
    path: dynamic
    health_check_timeout: 0.2
    outputs: [image]
  - id: detect
    path: dynamic
    inputs:
      frames: camera/image
";

fn daemon(name: &str) -> Daemon {
    let root = std::env::temp_dir().join(format!("astrs-health-{}-{name}", std::process::id()));
    let _ = std::fs::create_dir_all(&root);
    let config = DaemonConfig::new(RuntimePaths::under(root))
        .with_listen(ListenConfig::none())
        .with_shm(false);
    let mut daemon = Daemon::new(config).expect("a daemon");
    let manifest = Manifest::from_yaml_str(PIPELINE).expect("a manifest");
    let plan = plan_dataflow(dataflow(), &manifest, &BTreeMap::new()).expect("a plan");
    daemon.admit(&plan).expect("admitted");
    daemon
}

struct FakeNode {
    writer: AsyncFrameWriter<tokio::io::WriteHalf<DuplexStream>>,
    reader: AsyncFrameReader<tokio::io::ReadHalf<DuplexStream>>,
    #[allow(dead_code)]
    session: SessionId,
}

impl FakeNode {
    fn attach(daemon: &mut Daemon) -> Self {
        let session = daemon.sessions().mint();
        let (accepted, node_side) = NodeListeners::in_process(session);
        daemon.attach(accepted);
        let (reader, writer) = tokio::io::split(node_side);
        Self {
            writer: AsyncFrameWriter::new(writer, FrameLimits::uds()),
            reader: AsyncFrameReader::new(reader, FrameLimits::uds()),
            session,
        }
    }

    async fn send(&mut self, request: NodeRequest) {
        self.writer.send(&request).await.expect("a writable frame");
    }

    async fn recv(&mut self) -> Option<NodeEvent> {
        tokio::time::timeout(
            Duration::from_millis(300),
            self.reader.read_message::<NodeEvent>(),
        )
        .await
        .ok()?
        .ok()?
    }

    async fn register(&mut self, name: &str) {
        self.send(NodeRequest::Register(NodeHandshake::dynamic(
            dataflow(),
            node(name),
        )))
        .await;
    }
}

async fn pump(daemon: &mut Daemon) {
    daemon.pump(Duration::from_millis(80)).await;
}

/// A daemon with `camera` registered and its liveness deadline armed.
async fn registered() -> (Daemon, FakeNode) {
    let mut daemon = daemon("registered");
    let mut camera = FakeNode::attach(&mut daemon);
    camera.register("camera").await;
    pump(&mut daemon).await;
    let _ = camera.recv().await;
    (daemon, camera)
}

#[tokio::test]
async fn only_a_node_that_asked_for_a_timeout_is_watched() {
    let mut daemon = daemon("opt-in");
    let mut camera = FakeNode::attach(&mut daemon);
    let mut detect = FakeNode::attach(&mut daemon);
    camera.register("camera").await;
    detect.register("detect").await;
    pump(&mut daemon).await;

    assert!(
        daemon.health().is_armed(dataflow(), &node("camera")),
        "the manifest set health_check_timeout"
    );
    assert!(
        !daemon.health().is_armed(dataflow(), &node("detect")),
        "a node that named no timeout is not monitored"
    );
    assert_eq!(daemon.health().len(), 1);
}

#[tokio::test]
async fn a_configured_default_watches_every_node() {
    let root = std::env::temp_dir().join(format!("astrs-health-{}-default", std::process::id()));
    let _ = std::fs::create_dir_all(&root);
    let config = DaemonConfig::new(RuntimePaths::under(root))
        .with_listen(ListenConfig::none())
        .with_shm(false)
        .with_default_health_timeout(Some(Duration::from_secs(5)));
    let mut daemon = Daemon::new(config).expect("a daemon");
    let manifest = Manifest::from_yaml_str(PIPELINE).expect("a manifest");
    let plan = plan_dataflow(dataflow(), &manifest, &BTreeMap::new()).expect("a plan");
    daemon.admit(&plan).expect("admitted");

    let mut detect = FakeNode::attach(&mut daemon);
    detect.register("detect").await;
    pump(&mut daemon).await;

    assert!(daemon.health().is_armed(dataflow(), &node("detect")));
    assert_eq!(
        daemon.health().timeout_of(dataflow(), &node("detect")),
        Some(Duration::from_secs(5)),
        "the manifest said nothing, so the daemon's default applies"
    );
    assert_eq!(
        daemon.health().timeout_of(dataflow(), &node("camera")),
        None,
        "camera has not registered"
    );
}

/// §12 conformance zoo, scenario 13/16 (silent-source, health timeout).
#[tokio::test]
async fn a_silent_node_is_failed_with_a_typed_cause() {
    let (mut daemon, _camera) = registered().await;
    let start = Instant::now();

    daemon.tick(start + Duration::from_secs(1));

    let node_state = daemon
        .dataflow(dataflow())
        .and_then(|state| state.node(&node("camera")))
        .expect("present");
    match node_state.exit_cause() {
        Some(NodeExitCause::HealthCheckTimeout { after }) => {
            assert_eq!(after.to_duration(), Duration::from_millis(200));
        }
        other => panic!("expected a health-check timeout, got {other:?}"),
    }
    assert_eq!(daemon.metrics().ft_stats().health_timeouts, 1);
}

#[tokio::test]
async fn a_node_that_keeps_speaking_is_never_failed() {
    let (mut daemon, mut camera) = registered().await;
    let start = Instant::now();

    for step in 1..=6u64 {
        camera
            .send(NodeRequest::SendMessage {
                output: data("image"),
                metadata: Default::default(),
                payload: astrs_wire::OutputPayload::inline(vec![step as u8]),
            })
            .await;
        pump(&mut daemon).await;
        // Each publish refreshes the deadline, so a synthetic clock that
        // advances by less than the timeout between them never expires it.
        daemon.tick(start + Duration::from_millis(100 * step));
    }

    let node_state = daemon
        .dataflow(dataflow())
        .and_then(|state| state.node(&node("camera")))
        .expect("present");
    assert!(node_state.exit_cause().is_none(), "still healthy");
    assert_eq!(daemon.metrics().ft_stats().health_timeouts, 0);
}

#[tokio::test]
async fn a_node_parked_in_next_event_is_alive_however_long_it_waits() {
    let (mut daemon, mut camera) = registered().await;

    // `camera` has no inputs, so `NextEvent` parks forever: exactly the state
    // an idle consumer of a slow sensor is in.
    camera
        .send(NodeRequest::NextEvent {
            timeout: None,
            max_batch: 8,
        })
        .await;
    pump(&mut daemon).await;

    assert_eq!(
        daemon
            .health()
            .state(dataflow(), &node("camera"), Instant::now()),
        LivenessState::Parked
    );

    let start = Instant::now();
    daemon.tick(start + Duration::from_secs(600));

    let node_state = daemon
        .dataflow(dataflow())
        .and_then(|state| state.node(&node("camera")))
        .expect("present");
    assert!(
        node_state.exit_cause().is_none(),
        "a parked node is waiting on the daemon, not hung"
    );
    assert!(daemon.health().next_deadline().is_none());
}

#[tokio::test]
async fn an_exiting_node_disarms_its_deadline() {
    let (mut daemon, camera) = registered().await;
    assert!(daemon.health().is_armed(dataflow(), &node("camera")));

    drop(camera);
    for _ in 0..6 {
        pump(&mut daemon).await;
        if !daemon.health().is_armed(dataflow(), &node("camera")) {
            break;
        }
    }
    assert!(
        !daemon.health().is_armed(dataflow(), &node("camera")),
        "a node that is gone has no deadline to miss"
    );
}

/// The matrix itself, against the pure table: every combination of what a node
/// last did and how long ago, versus what the daemon believes.
#[test]
fn the_liveness_matrix() {
    #[derive(Debug)]
    struct Case {
        /// What the node last did.
        last: &'static str,
        /// How long ago, in milliseconds.
        elapsed_ms: u64,
        /// What the table should say.
        expected: LivenessState,
        /// Whether the node should be failed.
        expires: bool,
    }

    let cases = [
        Case {
            last: "spoke",
            elapsed_ms: 0,
            expected: LivenessState::Active,
            expires: false,
        },
        Case {
            last: "spoke",
            elapsed_ms: 199,
            expected: LivenessState::Active,
            expires: false,
        },
        Case {
            last: "spoke",
            elapsed_ms: 200,
            expected: LivenessState::Overdue,
            expires: true,
        },
        Case {
            last: "spoke",
            elapsed_ms: 10_000,
            expected: LivenessState::Overdue,
            expires: true,
        },
        Case {
            last: "parked",
            elapsed_ms: 0,
            expected: LivenessState::Parked,
            expires: false,
        },
        Case {
            last: "parked",
            elapsed_ms: 10_000,
            expected: LivenessState::Parked,
            expires: false,
        },
        Case {
            last: "unparked",
            elapsed_ms: 100,
            expected: LivenessState::Active,
            expires: false,
        },
        Case {
            last: "unparked",
            elapsed_ms: 300,
            expected: LivenessState::Overdue,
            expires: true,
        },
        Case {
            last: "never-armed",
            elapsed_ms: 10_000,
            expected: LivenessState::Unmonitored,
            expires: false,
        },
    ];

    for case in cases {
        let start = Instant::now();
        let mut table = HealthTable::new();
        if case.last != "never-armed" {
            table.arm(
                dataflow(),
                node("camera"),
                0,
                Some(Duration::from_millis(200)),
                start,
            );
        }
        match case.last {
            "spoke" => {
                table.touch(dataflow(), &node("camera"), start);
            }
            "parked" => {
                table.park(dataflow(), &node("camera"), start);
            }
            "unparked" => {
                table.park(dataflow(), &node("camera"), start);
                table.unpark(dataflow(), &node("camera"), start);
            }
            _ => {}
        }

        let at = start + Duration::from_millis(case.elapsed_ms);
        assert_eq!(
            table.state(dataflow(), &node("camera"), at),
            case.expected,
            "{case:?}"
        );
        assert_eq!(!table.expired(at).is_empty(), case.expires, "{case:?}");
    }
}

/// The HLC is the *other* clock: it stamps events so a recording can be
/// replayed, and a manual one makes those stamps reproducible (§4.3, §14).
#[test]
fn a_manual_clock_gives_reproducible_stamps() {
    /// One scripted run of the clock, returning the stamps it produced.
    fn script() -> (astrs_time::HlcTimestamp, astrs_time::HlcTimestamp) {
        let hlc = HlcClock::new(ManualClock::new(1_000_000_000));
        let first = hlc.now();
        hlc.clock().advance(Duration::from_millis(500));
        let second = hlc.now();
        (first, second)
    }

    let (first, second) = script();
    assert!(second > first, "the stamp advanced with the clock");

    // The same script against a fresh clock produces the same stamps, which is
    // what a replay depends on (§14).
    assert_eq!(script(), (first, second));
}
