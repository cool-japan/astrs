//! The registration handshake (blueprint §7.3).
//!
//! ```text
//!   node                                       daemon
//!    ├──────── Hello / Welcome ─────────────────►│   (§7.2, astrs-transport)
//!    ├──────── Register(NodeHandshake) ─────────►│
//!    │◄─────── Registered { spec, session } ─────┤
//!    ├──────── Subscribe { inputs } ────────────►│
//!    │                                            │
//!    └──────── the pump starts here ─────────────┘
//! ```
//!
//! The handshake runs on the link *before* the reader task exists, because
//! everything after it depends on the specification `Registered` carries: a
//! `path: dynamic` node (§8.3) has no `ASTRS_NODE_CONFIG` at all and learns
//! its wiring here, and even a spawned one takes the daemon's word for its
//! generation over its own.
//!
//! # Events that arrive during the handshake
//!
//! A daemon may legitimately send an input the instant it accepts the
//! registration — it is under no obligation to wait for `Subscribe`. Those
//! frames are **buffered** here and replayed through the ordinary dispatcher
//! once the queues exist, so a message can be early but never lost.

use std::sync::Arc;
use std::time::Duration;

use astrs_wire::{
    DataId, DurationMs, NodeEvent, NodeHandshake, NodeRequest, NodeSpawnSpec, SessionId,
    WireMessage,
};
use tokio::sync::mpsc;

use crate::error::{NodeError, Result};
use crate::events::{EventSource, EventStream};
use crate::runtime::NodeRuntime;
use crate::session::connect::{LinkDuplex, NodeLink};
use crate::session::{OUTGOING_CAPACITY, SessionShared, pump};

/// How long the registration exchange waits for its answer.
pub const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(10);

/// What the registration produced.
pub struct Registration {
    /// The shared session state.
    pub shared: Arc<SessionShared>,
    /// The node's event stream.
    pub events: EventStream,
    /// The running reader and writer tasks.
    pub tasks: pump::SessionTasks,
    /// The effective specification the daemon assigned.
    pub spec: Arc<NodeSpawnSpec>,
}

impl core::fmt::Debug for Registration {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Registration")
            .field("node", &self.spec.node)
            .field("generation", &self.spec.generation)
            .finish_non_exhaustive()
    }
}

/// Registers over an established link and starts the session.
///
/// # Errors
///
/// [`NodeError::Registration`] when the daemon refuses or answers with
/// something else, [`NodeError::Timeout`] when it does not answer at all, and
/// [`NodeError::Transport`] when the link fails mid-handshake.
pub async fn register(
    link: NodeLink,
    handshake: NodeHandshake,
    runtime: NodeRuntime,
    zero_copy_threshold: u64,
    subscribe_to: Vec<DataId>,
) -> Result<Registration> {
    register_with(
        link,
        handshake,
        runtime,
        zero_copy_threshold,
        subscribe_to,
        None,
    )
    .await
}

/// [`register`], told where the daemon's segment broker listens (§6.2).
///
/// The broker socket travels in [`astrs_wire::NodeConfig::shm_broker`], and it
/// is what turns a `RouteUpgrade` into a mapped segment on every platform —
/// see [`crate::session::routes::open_producer`]. Separate from [`register`]
/// so a caller that has no configuration blob (the in-process test harness)
/// keeps its shorter call.
///
/// # Errors
///
/// As [`register`].
pub async fn register_with(
    link: NodeLink,
    handshake: NodeHandshake,
    runtime: NodeRuntime,
    zero_copy_threshold: u64,
    subscribe_to: Vec<DataId>,
    shm_broker: Option<std::path::PathBuf>,
) -> Result<Registration> {
    let limits = link.session.frame_limits();
    let session_id = link.session.session_id;
    let mut duplex = link.duplex;

    duplex
        .send_message(&NodeRequest::Register(handshake))
        .await?;

    let (spec, session, deferred) = await_registered(&mut duplex, session_id).await?;
    let spec = Arc::new(spec);

    let source = Arc::new(EventSource::new());
    for input in &spec.inputs {
        source.register_input(input)?;
    }

    let (sender, receiver) = mpsc::channel(OUTGOING_CAPACITY);
    let shared = Arc::new(
        SessionShared::new(
            Arc::clone(&spec),
            session,
            sender,
            Arc::clone(&source),
            runtime.clone(),
            zero_copy_threshold,
            limits,
        )
        .with_shm_broker(shm_broker),
    );

    duplex
        .send_message(&NodeRequest::Subscribe {
            inputs: subscribe_to,
        })
        .await?;

    let (reader, writer) = duplex.into_halves();
    let tasks = pump::spawn(&shared, reader, writer, receiver);

    // A node that stops reading must stop the daemon queueing for it (§7.3),
    // and the only moment that is observable is when the stream is dropped.
    {
        let notify = Arc::clone(&shared);
        source.set_abandon_hook(Box::new(move || {
            let _ = notify.send_request(NodeRequest::EventStreamDropped);
        }));
    }

    // The other half of §11.3: a deadline violation `EventSource` measures
    // locally is always turned into a control-lane `Event::Error` (so the
    // node itself sees it even offline), but only a *connected* node can
    // relay it to the daemon for the `astrs/status` fan-out and the
    // registered metric — this is the one place a live session exists to
    // install that relay. `send_request` (not `_async`) matches
    // `EventStreamDropped` above: the hook itself has no `.await` point to
    // offer (its signature is a plain `Fn`, called synchronously from
    // inside `EventSource::finish_deadlines`, itself invoked from *both*
    // `SessionShared::send_request` and `send_request_async`'s publish
    // path), so the sync call is the only one it *can* make — safe from
    // either caller because `send_request` only ever blocks past
    // `NodeRuntime::block_on` when the outgoing channel is momentarily
    // full, and that call is what already makes blocking-from-async safe
    // throughout this crate: `tokio::task::block_in_place` on a
    // multi-threaded runtime, or a clean `NodeError::BlockingInAsync`
    // (never a panic) on a single-threaded one. And, exactly like a
    // dropped event stream, a relay that cannot be enqueued right now (the
    // daemon is gone, the outgoing channel is closed, or that
    // single-threaded-runtime error) is not a condition this node can act
    // on differently, so the error is intentionally discarded rather than
    // propagated out of a publish call that already succeeded on its own
    // terms.
    {
        let notify = Arc::clone(&shared);
        source.set_deadline_violation_hook(Box::new(move |input, budget, latency| {
            let _ = notify.send_request(NodeRequest::ReportDeadlineViolation {
                input,
                budget: DurationMs::from_duration(budget),
                latency: DurationMs::from_duration(latency),
            });
        }));
    }

    // Anything the daemon sent before the queues existed goes through the
    // ordinary dispatcher now, in arrival order.
    for event in deferred {
        pump::dispatch(&shared, event);
    }

    let events = EventStream::new(source, runtime);
    Ok(Registration {
        shared,
        events,
        tasks,
        spec,
    })
}

/// Reads frames until `Registered` arrives, buffering everything else.
async fn await_registered(
    duplex: &mut LinkDuplex,
    expected_session: SessionId,
) -> Result<(NodeSpawnSpec, SessionId, Vec<NodeEvent>)> {
    let mut deferred = Vec::new();
    let deadline = tokio::time::Instant::now() + REGISTRATION_TIMEOUT;
    loop {
        let frame = tokio::time::timeout_at(deadline, duplex.recv_frame())
            .await
            .map_err(|_| NodeError::Timeout {
                operation: "node registration",
                millis: as_millis(REGISTRATION_TIMEOUT),
            })??;
        let Some(frame) = frame else {
            return Err(NodeError::Registration(
                "the daemon closed the connection before answering the registration".to_owned(),
            ));
        };
        if frame.kind() != astrs_wire::FrameKind::NodeEvent {
            return Err(NodeError::Registration(format!(
                "the daemon answered a registration with a `{}` frame",
                frame.kind()
            )));
        }
        match NodeEvent::from_frame(&frame.as_view())? {
            NodeEvent::Registered { spec, session } => {
                if session != expected_session {
                    // Not fatal — the greeting's session and the
                    // registration's are allowed to differ if a daemon
                    // resumes — but worth carrying the daemon's answer, which
                    // is the authoritative one.
                }
                return Ok((*spec, session, deferred));
            }
            NodeEvent::Stop { cause, .. } => {
                return Err(NodeError::Registration(format!(
                    "the daemon stopped this node during registration: {cause}"
                )));
            }
            other => deferred.push(other),
        }
    }
}

/// Milliseconds, saturating rather than wrapping.
fn as_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::session::connect::wrap_stream;
    use astrs_wire::{
        DataflowId, FrameKind, FrameLimits, InputSpec, Metadata, NodeId, NodeSource, OutputPayload,
        PortRef, StopCause,
    };

    fn spec() -> NodeSpawnSpec {
        NodeSpawnSpec::new(
            DataflowId::from_u128(4),
            NodeId::new("detect").unwrap(),
            1,
            NodeSource::Dynamic,
        )
        .with_input(InputSpec::new(
            DataId::new("frames").unwrap(),
            PortRef::from_parts("camera", "image").unwrap(),
        ))
    }

    /// As [`spec`], but `frames` carries a 1ms deadline (§11.3) — short
    /// enough that an ordinary test-thread scheduling delay between the
    /// input arriving and the node's next publish reliably runs it over
    /// budget, without a test-visible sleep longer than a couple of
    /// milliseconds.
    fn spec_with_deadline() -> NodeSpawnSpec {
        NodeSpawnSpec::new(
            DataflowId::from_u128(4),
            NodeId::new("detect").unwrap(),
            1,
            NodeSource::Dynamic,
        )
        .with_input(InputSpec {
            deadline: Some(DurationMs::new(1)),
            ..InputSpec::new(
                DataId::new("frames").unwrap(),
                PortRef::from_parts("camera", "image").unwrap(),
            )
        })
    }

    fn link(io: tokio::io::DuplexStream) -> NodeLink {
        NodeLink {
            duplex: wrap_stream(io, FrameLimits::uds()),
            session: astrs_wire::NegotiatedSession {
                protocol: astrs_wire::PROTOCOL_VERSION,
                session_id: SessionId::from_u128(11),
                features: astrs_wire::FeatureFlags::EMPTY,
                limits: astrs_wire::NegotiatedLimits::uds(),
                role: astrs_wire::Role::Node,
                peer_version: astrs_wire::AstrsVersion::current(),
                resumed: false,
            },
            endpoint: "duplex".to_owned(),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_registration_exchange_completes_and_starts_the_pump() {
        let (node_io, daemon_io) = tokio::io::duplex(64 * 1024);
        let mut daemon = wrap_stream(daemon_io, FrameLimits::uds());

        let daemon_task = tokio::spawn(async move {
            let request = daemon
                .expect_message::<NodeRequest>(FrameKind::NodeRequest)
                .await
                .unwrap();
            assert!(matches!(request, NodeRequest::Register(_)));
            daemon
                .send_message(&NodeEvent::Registered {
                    spec: Box::new(spec()),
                    session: SessionId::from_u128(11),
                })
                .await
                .unwrap();
            let request = daemon
                .expect_message::<NodeRequest>(FrameKind::NodeRequest)
                .await
                .unwrap();
            assert!(matches!(request, NodeRequest::Subscribe { .. }));
            daemon
                .send_message(&NodeEvent::Input {
                    id: DataId::new("frames").unwrap(),
                    source: PortRef::from_parts("camera", "image").unwrap(),
                    metadata: Metadata::default(),
                    payload: vec![1, 2],
                })
                .await
                .unwrap();
            daemon
        });

        let handshake =
            NodeHandshake::dynamic(DataflowId::from_u128(4), NodeId::new("detect").unwrap());
        let registration = register(
            link(node_io),
            handshake,
            NodeRuntime::acquire().unwrap(),
            4096,
            Vec::new(),
        )
        .await
        .unwrap();

        assert_eq!(registration.spec.generation, 1);
        assert_eq!(registration.spec.inputs.len(), 1);
        assert!(format!("{registration:?}").contains("detect"));

        let mut events = registration.events;
        let event = events
            .recv_async_timeout(Duration::from_secs(5))
            .await
            .unwrap();
        assert!(event.is_input());

        registration.shared.close();
        registration.tasks.abort();
        let _daemon = daemon_task.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn events_that_arrive_before_registered_are_replayed_not_lost() {
        let (node_io, daemon_io) = tokio::io::duplex(64 * 1024);
        let mut daemon = wrap_stream(daemon_io, FrameLimits::uds());

        let daemon_task = tokio::spawn(async move {
            let _ = daemon
                .expect_message::<NodeRequest>(FrameKind::NodeRequest)
                .await
                .unwrap();
            // An input *before* the registration answer.
            daemon
                .send_message(&NodeEvent::Input {
                    id: DataId::new("frames").unwrap(),
                    source: PortRef::from_parts("camera", "image").unwrap(),
                    metadata: Metadata::default(),
                    payload: vec![9],
                })
                .await
                .unwrap();
            daemon
                .send_message(&NodeEvent::Registered {
                    spec: Box::new(spec()),
                    session: SessionId::from_u128(11),
                })
                .await
                .unwrap();
            let _ = daemon
                .expect_message::<NodeRequest>(FrameKind::NodeRequest)
                .await
                .unwrap();
            daemon
        });

        let registration = register(
            link(node_io),
            NodeHandshake::dynamic(DataflowId::from_u128(4), NodeId::new("detect").unwrap()),
            NodeRuntime::acquire().unwrap(),
            4096,
            Vec::new(),
        )
        .await
        .unwrap();

        let mut events = registration.events;
        let event = events
            .recv_async_timeout(Duration::from_secs(5))
            .await
            .unwrap();
        let Some((id, _, data)) = event.into_input() else {
            panic!("expected the replayed input");
        };
        assert_eq!(id.as_str(), "frames");
        assert_eq!(data.to_vec(), vec![9]);

        registration.shared.close();
        registration.tasks.abort();
        let _daemon = daemon_task.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stop_during_registration_is_reported() {
        let (node_io, daemon_io) = tokio::io::duplex(64 * 1024);
        let mut daemon = wrap_stream(daemon_io, FrameLimits::uds());
        let daemon_task = tokio::spawn(async move {
            let _ = daemon
                .expect_message::<NodeRequest>(FrameKind::NodeRequest)
                .await
                .unwrap();
            daemon
                .send_message(&NodeEvent::Stop {
                    cause: StopCause::Destroyed,
                    grace: None,
                })
                .await
                .unwrap();
            daemon
        });

        let error = register(
            link(node_io),
            NodeHandshake::dynamic(DataflowId::from_u128(4), NodeId::new("detect").unwrap()),
            NodeRuntime::acquire().unwrap(),
            4096,
            Vec::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, NodeError::Registration(_)), "{error}");
        let _daemon = daemon_task.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_daemon_that_hangs_up_is_reported() {
        let (node_io, daemon_io) = tokio::io::duplex(64 * 1024);
        drop(daemon_io);
        let error = register(
            link(node_io),
            NodeHandshake::dynamic(DataflowId::from_u128(4), NodeId::new("detect").unwrap()),
            NodeRuntime::acquire().unwrap(),
            4096,
            Vec::new(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, NodeError::Registration(_) | NodeError::Transport(_)),
            "{error}"
        );
    }

    /// The production wiring this module's own `register_with` adds beside
    /// its abandon-hook install: a deadline violation `EventSource` measures
    /// locally must reach the daemon as [`NodeRequest::ReportDeadlineViolation`],
    /// not only the node's own control-lane `Event::Error` — see
    /// `EventSource`'s `DeadlineViolationHook` docs for the half this test
    /// does *not* cover (that local event is asserted directly in
    /// `astrs_node_api::events::source`'s own tests).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_deadline_violation_is_relayed_to_the_daemon() {
        let (node_io, daemon_io) = tokio::io::duplex(64 * 1024);
        let mut daemon = wrap_stream(daemon_io, FrameLimits::uds());

        let daemon_task = tokio::spawn(async move {
            let request = daemon
                .expect_message::<NodeRequest>(FrameKind::NodeRequest)
                .await
                .unwrap();
            assert!(matches!(request, NodeRequest::Register(_)));
            daemon
                .send_message(&NodeEvent::Registered {
                    spec: Box::new(spec_with_deadline()),
                    session: SessionId::from_u128(11),
                })
                .await
                .unwrap();
            let request = daemon
                .expect_message::<NodeRequest>(FrameKind::NodeRequest)
                .await
                .unwrap();
            assert!(matches!(request, NodeRequest::Subscribe { .. }));
            daemon
                .send_message(&NodeEvent::Input {
                    id: DataId::new("frames").unwrap(),
                    source: PortRef::from_parts("camera", "image").unwrap(),
                    metadata: Metadata::default(),
                    payload: vec![1, 2],
                })
                .await
                .unwrap();

            // The node's simulated publish, which closes the open deadline
            // measurement — always sent first (`finish_deadlines` runs
            // synchronously *after* this request is already enqueued; see
            // `SessionShared::send_request`'s own docs).
            let request = daemon
                .expect_message::<NodeRequest>(FrameKind::NodeRequest)
                .await
                .unwrap();
            assert!(matches!(request, NodeRequest::SendMessage { .. }));

            // The relay this test exists to prove: queued from inside the
            // `DeadlineViolationHook` this module's `register_with` installs,
            // strictly after the publish above on the same outgoing channel.
            let request = daemon
                .expect_message::<NodeRequest>(FrameKind::NodeRequest)
                .await
                .unwrap();
            match request {
                NodeRequest::ReportDeadlineViolation {
                    input,
                    budget,
                    latency,
                } => {
                    assert_eq!(input.as_str(), "frames");
                    assert_eq!(budget, DurationMs::new(1));
                    assert!(latency >= budget, "{latency:?} >= {budget:?}");
                }
                other => panic!("expected ReportDeadlineViolation, got {other:?}"),
            }
            daemon
        });

        let registration = register(
            link(node_io),
            NodeHandshake::dynamic(DataflowId::from_u128(4), NodeId::new("detect").unwrap()),
            NodeRuntime::acquire().unwrap(),
            4096,
            Vec::new(),
        )
        .await
        .unwrap();

        let mut events = registration.events;
        let event = events
            .recv_async_timeout(Duration::from_secs(5))
            .await
            .unwrap();
        assert!(event.is_input(), "opens the deadline measurement");

        // Comfortably past the 1ms budget, so the publish below is judged
        // late regardless of test-runner scheduling jitter.
        tokio::time::sleep(Duration::from_millis(20)).await;

        registration
            .shared
            .send_request(NodeRequest::SendMessage {
                output: DataId::new("boxes").unwrap(),
                metadata: Metadata::default(),
                payload: OutputPayload::inline(vec![0]),
            })
            .unwrap();

        // `send_request` only enqueues onto the outgoing channel — the
        // writer task still has to flush both the publish above and the
        // relay it triggers onto the duplex. Awaiting the daemon side (whose
        // own `expect_message` calls simply wait for those bytes to arrive)
        // before tearing the session down avoids racing `tasks.abort()`
        // against that still-in-flight write.
        let _daemon = daemon_task.await.unwrap();

        registration.shared.close();
        registration.tasks.abort();
    }
}
