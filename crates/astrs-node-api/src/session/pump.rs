//! The two tasks that move frames, and the dispatcher between them.
//!
//! * The **writer** drains the outgoing channel onto the socket, one frame at
//!   a time, and ends when the channel closes.
//! * The **reader** decodes frames and hands each [`NodeEvent`] to
//!   [`dispatch`], then closes the session when the socket ends.
//!
//! [`dispatch`] is deliberately a *free function over shared state* rather
//! than a method on a task: the testing harness calls it directly to inject an
//! event, and the operator runtime will call it to fan one event out to
//! several operators. There is exactly one implementation of "what does this
//! event mean to a node", and everything uses it.
//!
//! # What the node never sees
//!
//! Three events are handled entirely inside the session and are *not*
//! delivered to the node's stream:
//!
//! | Event | Handled by |
//! |---|---|
//! | `Registered` | The init handshake, before the pump starts |
//! | `ExtValue` | Whoever called [`crate::Node::ext_load`] |
//! | `RouteUpgrade` / `RouteDowngrade` | The [`RouteTable`](super::RouteTable), which also sends the ack (§6.3) |
//! | `InputRouteUpgrade` / `InputRouteDowngrade` | The [`InputRouteTable`](super::InputRouteTable), which attaches or detaches the ring (§6.3) |
//!
//! Everything else reaches the node, because everything else is something a
//! node might reasonably act on.
//!
//! The two route pairs are the same handshake seen from its two ends, and
//! neither reaches the node's stream: a route changing plane is not an event
//! an application acts on, it is a change in how the events it *does* act on
//! are carried. What a node sees is [`crate::Payload::is_zero_copy`] turning
//! true.

use std::sync::Arc;

use astrs_wire::{Frame, FrameKind, NodeEvent, NodeRequest, WireMessage};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::NodeError;
use crate::events::{Event, QueuedEvent};
use crate::payload::Payload;
use crate::session::connect::{LinkReader, LinkWriter};
use crate::session::routes::open_producer;
use crate::session::{Outgoing, SessionShared};

/// The join handles of a running session.
#[derive(Debug)]
pub struct SessionTasks {
    /// The frame reader.
    pub reader: JoinHandle<()>,
    /// The frame writer.
    pub writer: JoinHandle<()>,
}

impl SessionTasks {
    /// Waits for both tasks to finish.
    ///
    /// Used by the testing harness to make shutdown deterministic; a real
    /// node simply drops its [`crate::Node`].
    pub async fn join(self) {
        let _ = self.reader.await;
        let _ = self.writer.await;
    }

    /// Aborts both tasks.
    pub fn abort(&self) {
        self.reader.abort();
        self.writer.abort();
    }
}

/// Starts the reader and writer tasks for an established link.
#[must_use]
pub fn spawn(
    shared: &Arc<SessionShared>,
    reader: LinkReader,
    writer: LinkWriter,
    outgoing: mpsc::Receiver<Outgoing>,
) -> SessionTasks {
    let reader_shared = Arc::clone(shared);
    let writer_shared = Arc::clone(shared);
    SessionTasks {
        reader: shared
            .runtime
            .spawn(async move { reader_task(reader, reader_shared).await }),
        writer: shared
            .runtime
            .spawn(async move { writer_task(writer, outgoing, writer_shared).await }),
    }
}

/// Drains the outgoing channel onto the socket.
async fn writer_task(
    mut writer: LinkWriter,
    mut outgoing: mpsc::Receiver<Outgoing>,
    shared: Arc<SessionShared>,
) {
    while let Some(message) = outgoing.recv().await {
        let result = match message {
            Outgoing::Request(request) => writer.send_message(request.as_ref()).await,
            Outgoing::Log(frame) => writer.send_message(frame.as_ref()).await,
            Outgoing::Shutdown => {
                let _ = writer.flush().await;
                break;
            }
        };
        if let Err(error) = result {
            shared.source.push_control(Event::Error(format!(
                "daemon connection failed while sending: {error}"
            )));
            break;
        }
    }
    let _ = writer.shutdown().await;
}

/// Decodes frames until the socket ends, then closes the session.
async fn reader_task(mut reader: LinkReader, shared: Arc<SessionShared>) {
    loop {
        match reader.recv_frame().await {
            Ok(Some(frame)) => {
                if let Err(error) = handle_frame(&shared, &frame) {
                    shared
                        .source
                        .push_control(Event::Error(format!("undecodable frame: {error}")));
                }
            }
            Ok(None) => break,
            Err(error) => {
                shared.source.push_control(Event::Error(format!(
                    "daemon connection failed while receiving: {error}"
                )));
                break;
            }
        }
    }
    shared.close();
}

/// Decodes one frame and dispatches it.
///
/// # Errors
///
/// [`NodeError::Wire`] when the frame is not a decodable `NodeEvent`, and
/// [`NodeError::Pattern`] for a frame kind a node has no business receiving.
pub fn handle_frame(shared: &Arc<SessionShared>, frame: &Frame) -> crate::Result<()> {
    match frame.kind() {
        FrameKind::NodeEvent => {
            let event = NodeEvent::from_frame(&frame.as_view())?;
            dispatch(shared, event);
            Ok(())
        }
        other => Err(NodeError::Pattern(format!(
            "a node received a `{other}` frame, which only flows the other way"
        ))),
    }
}

/// Turns one [`NodeEvent`] into whatever it means for this node.
pub fn dispatch(shared: &Arc<SessionShared>, event: NodeEvent) {
    shared.count_event();
    match event {
        NodeEvent::Input {
            id,
            source,
            metadata,
            payload,
        } => {
            // Fold the producer's clock reading into ours, so causal order
            // survives the hop (§4.3).
            let _accepted = shared.observe_remote(metadata.timestamp);
            shared.source.push_input(
                &id,
                QueuedEvent::Input {
                    source,
                    metadata,
                    payload: Payload::inline(payload),
                },
            );
        }
        NodeEvent::InputClosed { id, source, reason } => {
            // An input on a ring gets its closure delivered *behind* whatever
            // the ring still holds: the producer commits its last frame and
            // only then tells the daemon, so this event routinely overtakes a
            // sample still being read (§6.2, and `session::inputs`'s
            // `close_input` for the full reasoning). An input on the daemon
            // path has no such ordering question and is queued directly.
            let closed = QueuedEvent::Closed { source, reason };
            if let Some(closed) = shared.inputs.close_input(&id, closed) {
                shared.source.push_input(&id, closed);
            }
        }
        NodeEvent::InputRecovered {
            id,
            source,
            generation,
        } => {
            shared
                .source
                .push_input(&id, QueuedEvent::Recovered { source, generation });
        }
        NodeEvent::Stop { cause, .. } => {
            shared.source.push_control(Event::Stop(cause));
        }
        NodeEvent::Reload { operator, path } => {
            shared.source.push_control(Event::Reload { operator, path });
        }
        NodeEvent::AllInputsClosed => {
            // Terminal, and on the control lane — which is served *before* any
            // data. A node that winds down on it would drop whatever a ring is
            // still draining, so the drains are finished first. See
            // `session::inputs::drain_all` for why waiting is acceptable here
            // and nowhere else.
            let _waited = shared.inputs.drain_all();
            shared.source.push_control(Event::AllInputsClosed);
        }
        NodeEvent::NodeFailed { peer, cause } => {
            shared
                .source
                .push_control(Event::NodeFailed { peer, cause });
        }
        NodeEvent::Restarted { peer, generation } => {
            shared
                .source
                .push_control(Event::Restarted { peer, generation });
        }
        NodeEvent::ParamUpdate { scope, key, value } => {
            shared
                .source
                .push_control(Event::ParamUpdate { scope, key, value });
        }
        NodeEvent::ParamDeleted { scope, key } => {
            shared
                .source
                .push_control(Event::ParamDeleted { scope, key });
        }
        NodeEvent::ExtDropped { key, reason } => {
            shared
                .source
                .push_control(Event::ExtDropped { key, reason });
        }
        NodeEvent::ExtValue { key, value } => {
            let _delivered = shared.resolve_extension(&key, value);
        }
        NodeEvent::RouteUpgrade {
            output,
            segment,
            consumers,
        } => handle_route_upgrade(shared, output, segment, consumers),
        NodeEvent::RouteDowngrade { output, reason } => {
            shared.routes.slot(&output).offer_downgrade(reason);
        }
        NodeEvent::InputRouteUpgrade {
            input,
            source,
            segment,
            consumer,
        } => handle_input_route_upgrade(shared, input, source, segment, consumer),
        NodeEvent::InputRouteDowngrade { input, .. } => {
            // Detaching is all there is to do: the daemon resumes brokering
            // the route on its side, and the node keeps reading the same
            // stream (§6.3).
            let _was_attached = shared.inputs.detach(&input);
        }
        NodeEvent::Registered { .. } => {
            // The init handshake consumes the first one; a later one means the
            // daemon re-registered this node under it, which it must not.
            shared.source.push_control(Event::Error(
                "the daemon sent a second `Registered` for a live session".to_owned(),
            ));
        }
        // `NodeEvent` is `#[non_exhaustive]`: an event a future daemon adds is
        // reported rather than silently ignored, so a version skew is visible.
        other => {
            shared
                .source
                .push_control(Event::Error(format!("unrecognised daemon event: {other}")));
        }
    }
}

/// Opens the offered segment and acknowledges the upgrade (§6.3).
fn handle_route_upgrade(
    shared: &Arc<SessionShared>,
    output: astrs_wire::DataId,
    segment: astrs_wire::ShmSegmentSpec,
    consumers: Vec<astrs_wire::PortRef>,
) {
    let slot = shared.routes.slot(&output);
    match open_producer(
        shared.dataflow,
        &shared.node,
        &output,
        &segment,
        shared.shm_broker.as_deref(),
    ) {
        Ok(producer) => {
            slot.offer_upgrade(producer, segment, consumers);
            let _ = shared.send_request(NodeRequest::RouteUpgradeAck {
                output,
                accepted: true,
                reason: None,
            });
        }
        Err(error) => {
            // §6.2: never sleep-retry. Say so, stay on the daemon path, and
            // let the operator see why.
            let reason = error.to_string();
            shared.count_shm_fallback();
            shared.source.push_control(Event::Error(format!(
                "route upgrade for `{output}` refused: {reason}"
            )));
            let _ = shared.send_request(NodeRequest::RouteUpgradeAck {
                output,
                accepted: false,
                reason: Some(reason),
            });
        }
    }
}

/// Attaches the ring an `InputRouteUpgrade` names (§6.3, consumer side).
///
/// Unlike its producer-side twin there is nothing to acknowledge: §6.3's
/// acknowledgement exists so that no message falls between the planes while a
/// *producer* switches, and a consumer switches by attaching to a ring that is
/// still empty. The daemon learns the attachment the way it learns every
/// attachment — from the segment's consumer table (§6.2: *"the daemon knows
/// attachment state because it brokers the segment fds"*) — and that
/// observation is what completes the handshake at the other end.
///
/// A failure leaves the input on the daemon path and is reported, never
/// retried in a loop (§6.2: *never sleep-retry*).
fn handle_input_route_upgrade(
    shared: &Arc<SessionShared>,
    input: astrs_wire::DataId,
    source: astrs_wire::PortRef,
    segment: astrs_wire::ShmSegmentSpec,
    consumer: astrs_wire::PortRef,
) {
    // The event names the port it is for. A session that is told to attach to
    // some other node's input is talking to a confused daemon, and attaching
    // to a ring that is not this node's business would be the wrong answer to
    // that.
    if *consumer.node() != shared.node {
        shared.inputs.count_refusal();
        shared.source.push_control(Event::Error(format!(
            "an input route upgrade for `{consumer}` reached node `{}`",
            shared.node
        )));
        return;
    }
    if let Err(error) = shared.inputs.attach(shared, &input, &source, &segment) {
        shared.inputs.count_refusal();
        shared.count_shm_fallback();
        shared.source.push_control(Event::Error(format!(
            "input route upgrade for `{input}` refused: {error}"
        )));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::events::EventSource;
    use crate::runtime::NodeRuntime;
    use crate::session::{OUTGOING_CAPACITY, RoutePlane};
    use astrs_time::HlcTimestamp;
    use astrs_wire::{
        DataId, DataflowId, ExtensionKey, FrameFlags, FrameLimits, InputSpec, Metadata, NodeId,
        NodeSource, NodeSpawnSpec, PortRef, RouteCloseReason, RouteDowngradeReason, SessionId,
        ShmSegmentSpec, StopCause,
    };

    fn spec() -> Arc<NodeSpawnSpec> {
        Arc::new(
            NodeSpawnSpec::new(
                DataflowId::from_u128(1),
                NodeId::new("detect").unwrap(),
                0,
                NodeSource::Dynamic,
            )
            .with_input(InputSpec::new(
                DataId::new("frames").unwrap(),
                PortRef::from_parts("camera", "image").unwrap(),
            )),
        )
    }

    fn shared() -> (Arc<SessionShared>, mpsc::Receiver<Outgoing>) {
        let (sender, receiver) = mpsc::channel(OUTGOING_CAPACITY);
        let source = Arc::new(EventSource::new());
        let spec = spec();
        for input in &spec.inputs {
            source.register_input(input).unwrap();
        }
        let shared = Arc::new(SessionShared::new(
            spec,
            SessionId::from_u128(3),
            sender,
            source,
            NodeRuntime::owned().unwrap(),
            4096,
            FrameLimits::uds(),
        ));
        (shared, receiver)
    }

    fn input_event(payload: Vec<u8>) -> NodeEvent {
        NodeEvent::Input {
            id: DataId::new("frames").unwrap(),
            source: PortRef::from_parts("camera", "image").unwrap(),
            metadata: Metadata::new(HlcTimestamp::new(1_000, 0)),
            payload,
        }
    }

    #[test]
    fn an_input_reaches_its_queue_with_its_payload() {
        let (shared, _rx) = shared();
        dispatch(&shared, input_event(vec![1, 2, 3]));
        let event = shared.source.try_next().unwrap();
        let Event::Input { id, data, .. } = event else {
            panic!("expected an input");
        };
        assert_eq!(id.as_str(), "frames");
        assert_eq!(data.to_vec(), vec![1, 2, 3]);
        assert_eq!(shared.stats().events_received, 1);
    }

    #[test]
    fn every_control_event_reaches_the_control_lane() {
        let (shared, _rx) = shared();
        let events = vec![
            NodeEvent::Stop {
                cause: StopCause::Requested,
                grace: None,
            },
            NodeEvent::Reload {
                operator: None,
                path: None,
            },
            NodeEvent::AllInputsClosed,
            NodeEvent::NodeFailed {
                peer: NodeId::new("camera").unwrap(),
                cause: astrs_wire::NodeExitCause::Success,
            },
            NodeEvent::Restarted {
                peer: NodeId::new("camera").unwrap(),
                generation: 2,
            },
            NodeEvent::ExtDropped {
                key: ExtensionKey::user("cal").unwrap(),
                reason: "ttl".to_owned(),
            },
        ];
        let expected = events.len();
        for event in events {
            dispatch(&shared, event);
        }
        let mut seen = 0;
        while let Some(event) = shared.source.try_next() {
            assert!(event.is_control(), "{event}");
            seen += 1;
        }
        assert_eq!(seen, expected);
    }

    #[test]
    fn input_closed_and_recovered_keep_their_input_queue() {
        let (shared, _rx) = shared();
        dispatch(&shared, input_event(vec![7]));
        dispatch(
            &shared,
            NodeEvent::InputClosed {
                id: DataId::new("frames").unwrap(),
                source: PortRef::from_parts("camera", "image").unwrap(),
                reason: RouteCloseReason::ProducerFinished,
            },
        );
        dispatch(
            &shared,
            NodeEvent::InputRecovered {
                id: DataId::new("frames").unwrap(),
                source: PortRef::from_parts("camera", "image").unwrap(),
                generation: 1,
            },
        );
        assert!(shared.source.try_next().unwrap().is_input());
        assert!(matches!(
            shared.source.try_next().unwrap(),
            Event::InputClosed { .. }
        ));
        assert!(matches!(
            shared.source.try_next().unwrap(),
            Event::InputRecovered { .. }
        ));
    }

    #[test]
    fn an_extension_value_goes_to_its_waiter_and_not_to_the_node() {
        let (shared, _rx) = shared();
        let key = ExtensionKey::user("cal").unwrap();
        let receiver = shared.watch_extension(&key);
        dispatch(
            &shared,
            NodeEvent::ExtValue {
                key: key.clone(),
                value: Some(vec![9]),
            },
        );
        assert_eq!(receiver.blocking_recv().unwrap(), Some(vec![9]));
        assert!(
            shared.source.try_next().is_none(),
            "the node never sees an ExtValue"
        );
    }

    #[test]
    fn a_route_downgrade_is_recorded_without_reaching_the_node() {
        let (shared, _rx) = shared();
        let output = DataId::new("detections").unwrap();
        dispatch(
            &shared,
            NodeEvent::RouteDowngrade {
                output: output.clone(),
                reason: RouteDowngradeReason::ConsumerDetached {
                    consumer: PortRef::from_parts("detect", "frames").unwrap(),
                },
            },
        );
        assert!(shared.routes.slot(&output).has_update());
        assert!(shared.source.try_next().is_none());
    }

    #[test]
    fn an_input_route_downgrade_detaches_without_reaching_the_node() {
        let (shared, _rx) = shared();
        let input = DataId::new("frames").unwrap();
        dispatch(
            &shared,
            NodeEvent::InputRouteDowngrade {
                input: input.clone(),
                reason: RouteDowngradeReason::SegmentClosed { generation: 2 },
            },
        );
        assert_eq!(shared.inputs.plane(&input), RoutePlane::Daemon);
        assert!(
            shared.source.try_next().is_none(),
            "a plane change is not an application event"
        );
    }

    #[test]
    fn an_unopenable_input_upgrade_is_refused_and_reported() {
        let (shared, _rx) = shared();
        let input = DataId::new("frames").unwrap();
        dispatch(
            &shared,
            NodeEvent::InputRouteUpgrade {
                input: input.clone(),
                source: PortRef::from_parts("camera", "image").unwrap(),
                // A name no key describes: refused before anything is mapped.
                segment: ShmSegmentSpec::new("astrs-bogus-name", 1, 4, 1024),
                consumer: PortRef::from_parts("detect", "frames").unwrap(),
            },
        );
        // The node stays on the daemon path and says why.
        assert_eq!(shared.inputs.plane(&input), RoutePlane::Daemon);
        assert_eq!(shared.inputs.stats().refusals, 1);
        assert_eq!(shared.stats().shm_fallbacks, 1);
        let Some(Event::Error(message)) = shared.source.try_next() else {
            panic!("expected an error");
        };
        assert!(message.contains("frames"), "{message}");
    }

    #[test]
    fn an_input_upgrade_addressed_to_another_node_is_refused() {
        let (shared, _rx) = shared();
        dispatch(
            &shared,
            NodeEvent::InputRouteUpgrade {
                input: DataId::new("frames").unwrap(),
                source: PortRef::from_parts("camera", "image").unwrap(),
                segment: ShmSegmentSpec::new("astrs-anything", 1, 4, 1024),
                // This session is `detect`; the event names somebody else.
                consumer: PortRef::from_parts("planner", "frames").unwrap(),
            },
        );
        assert_eq!(shared.inputs.stats().refusals, 1);
        assert_eq!(
            shared.inputs.stats().attaches,
            0,
            "nothing was mapped for another node's input"
        );
        let Some(Event::Error(message)) = shared.source.try_next() else {
            panic!("expected an error");
        };
        assert!(message.contains("planner/frames"), "{message}");
    }

    #[test]
    fn an_unopenable_upgrade_is_refused_and_reported() {
        let (shared, mut rx) = shared();
        let output = DataId::new("detections").unwrap();
        dispatch(
            &shared,
            NodeEvent::RouteUpgrade {
                output: output.clone(),
                segment: ShmSegmentSpec::new("astrs-bogus-name", 1, 4, 1024),
                consumers: Vec::new(),
            },
        );
        // The node stays on the daemon path and says so.
        let Some(Outgoing::Request(request)) = rx.try_recv().ok() else {
            panic!("expected an ack");
        };
        let NodeRequest::RouteUpgradeAck {
            accepted, reason, ..
        } = *request
        else {
            panic!("expected a RouteUpgradeAck");
        };
        assert!(!accepted);
        assert!(reason.is_some());
        assert_eq!(shared.stats().shm_fallbacks, 1);

        let event = shared.source.try_next().unwrap();
        assert!(matches!(event, Event::Error(_)));
    }

    #[test]
    fn a_second_registration_is_reported_as_an_error() {
        let (shared, _rx) = shared();
        dispatch(
            &shared,
            NodeEvent::Registered {
                spec: Box::new((*shared.spec).clone()),
                session: SessionId::from_u128(3),
            },
        );
        let event = shared.source.try_next().unwrap();
        let Event::Error(message) = event else {
            panic!("expected an error");
        };
        assert!(message.contains("second"), "{message}");
    }

    #[test]
    fn a_frame_of_the_wrong_kind_is_refused() {
        let (shared, _rx) = shared();
        let frame = Frame::new(FrameKind::NodeRequest, FrameFlags::EMPTY, vec![6]).unwrap();
        let error = handle_frame(&shared, &frame).unwrap_err();
        assert!(matches!(error, NodeError::Pattern(_)), "{error}");
    }

    #[test]
    fn a_well_formed_frame_is_decoded_and_dispatched() {
        let (shared, _rx) = shared();
        let bytes = input_event(vec![4, 5])
            .to_frame(FrameFlags::EMPTY, &FrameLimits::uds())
            .unwrap();
        let view = astrs_wire::decode_frame(&bytes, &FrameLimits::uds()).unwrap();
        let frame = Frame::new(view.kind(), view.flags(), view.payload().to_vec()).unwrap();
        handle_frame(&shared, &frame).unwrap();
        assert!(shared.source.try_next().unwrap().is_input());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pump_moves_frames_in_both_directions() {
        use crate::session::connect::wrap_stream;

        let (node_io, daemon_io) = tokio::io::duplex(64 * 1024);
        let limits = FrameLimits::uds();
        let node_link = wrap_stream(node_io, limits);
        let mut daemon_link = wrap_stream(daemon_io, limits);

        let (sender, receiver) = mpsc::channel(OUTGOING_CAPACITY);
        let source = Arc::new(EventSource::new());
        let spec = spec();
        for input in &spec.inputs {
            source.register_input(input).unwrap();
        }
        let shared = Arc::new(SessionShared::new(
            spec,
            SessionId::from_u128(1),
            sender,
            source,
            crate::runtime::NodeRuntime::acquire().unwrap(),
            4096,
            limits,
        ));
        let (reader, writer) = node_link.into_halves();
        let tasks = spawn(&shared, reader, writer, receiver);

        // Node → daemon.
        shared
            .send_request(NodeRequest::Subscribe { inputs: Vec::new() })
            .unwrap();
        let request = daemon_link
            .expect_message::<NodeRequest>(FrameKind::NodeRequest)
            .await
            .unwrap();
        assert!(matches!(request, NodeRequest::Subscribe { .. }));

        // Daemon → node.
        daemon_link
            .send_message(&input_event(vec![1]))
            .await
            .unwrap();
        for _ in 0..100 {
            if let Some(event) = shared.source.try_next() {
                assert!(event.is_input());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        shared.close();
        tasks.abort();
    }
}
