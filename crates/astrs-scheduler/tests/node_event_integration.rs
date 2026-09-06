//! End-to-end proof that `astrs_wire::NodeEvent` — the real daemon → node
//! wire event, not a stand-in — flows through this crate's queue and mux
//! machinery exactly like the crate's own [`astrs_scheduler::Envelope`]
//! does, with no adapter type in between.
//!
//! Every other test in this crate exercises `InputQueue<Envelope<T>>` and
//! `EventMux<Envelope<T>>`; `Envelope` is documented as a convenience for
//! "tests, examples, and any caller that does not already have such a
//! type" (`MetadataView`'s docs), while the crate's actual stated purpose
//! is `astrs-daemon` and `astrs-node-api` routing their *own* richer event
//! enum through the same mechanism (`lib.rs` crate docs). `NodeEvent` is
//! that enum for the daemon → node leg (blueprint §7.3), so this file
//! builds `InputQueue<NodeEvent>` and `EventMux<NodeEvent>` directly and
//! drives them with realistic events: a data plane full of `Input`
//! messages, a `request_id`-correlated service response mixed into the
//! same input, and a `Stop` arriving on the control lane.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_scheduler::{EventMux, InputQueue, PushOutcome};
use astrs_time::HlcTimestamp;
use astrs_wire::{DataId, Metadata, NodeEvent, PriorityLane, QueuePolicy, StopCause};

fn input(id: &str, source: &str, metadata: Metadata, payload: Vec<u8>) -> NodeEvent {
    NodeEvent::Input {
        id: DataId::new(id).unwrap(),
        source: source.parse().unwrap(),
        metadata,
        payload,
    }
}

fn plain_frame(seq: i64) -> NodeEvent {
    let mut meta = Metadata::new(HlcTimestamp::EPOCH);
    meta.set_seq(seq);
    input("frames", "camera/image", meta, vec![0xAA; 16])
}

fn correlated_response(request_id: &str) -> NodeEvent {
    let mut meta = Metadata::new(HlcTimestamp::EPOCH);
    meta.set_request_id(request_id);
    input("frames", "server/replies", meta, Vec::new())
}

#[test]
fn drop_oldest_evicts_plain_frames_but_never_a_correlated_response() {
    let queue: InputQueue<NodeEvent> = InputQueue::new(2, QueuePolicy::DropOldest).unwrap();

    assert_eq!(queue.push(plain_frame(1)).outcome, PushOutcome::Enqueued);
    assert_eq!(
        queue.push(correlated_response("req-1")).outcome,
        PushOutcome::Enqueued
    );
    // At capacity now, with one plain frame and one correlated response
    // queued. A third plain frame must evict the plain one, never the
    // correlated response.
    let report = queue.push(plain_frame(2));
    assert_eq!(report.outcome, PushOutcome::EnqueuedEvicting);

    let survivors: Vec<NodeEvent> = std::iter::from_fn(|| queue.pop()).collect();
    assert_eq!(survivors.len(), 2);
    assert!(
        survivors
            .iter()
            .any(|event| matches!(event, NodeEvent::Input { metadata, .. } if metadata.request_id() == Some("req-1"))),
        "the correlated response must have survived eviction"
    );
    assert!(
        !survivors.iter().any(
            |event| matches!(event, NodeEvent::Input { metadata, .. } if metadata.seq() == Some(1))
        ),
        "the first plain frame (seq 1) must have been the one evicted"
    );
}

#[test]
fn a_stop_event_is_immune_even_though_it_carries_no_metadata_at_all() {
    let queue: InputQueue<NodeEvent> = InputQueue::new(1, QueuePolicy::DropOldest).unwrap();

    let stop = NodeEvent::Stop {
        cause: StopCause::Requested,
        grace: None,
    };
    assert_eq!(queue.push(stop.clone()).outcome, PushOutcome::Enqueued);

    // The queue is at its nominal capacity of 1, occupied entirely by the
    // immune `Stop`. A plain frame arriving now cannot evict it — it is
    // the incoming message that gets refused instead.
    let report = queue.push(plain_frame(1));
    assert_eq!(report.outcome, PushOutcome::DroppedIncoming);
    assert!(matches!(
        report.signal,
        Some(astrs_scheduler::QueueSignal::ImmuneOverflow {
            immune_count: 1,
            ..
        })
    ));

    let popped = queue.pop().unwrap();
    assert!(matches!(popped, NodeEvent::Stop { .. }));
}

#[test]
fn backpressure_reports_the_must_log_signal_for_a_real_node_event_stream() {
    let queue: InputQueue<NodeEvent> = InputQueue::new(2, QueuePolicy::Backpressure).unwrap();
    for seq in 0..20 {
        let report = queue.push(plain_frame(seq));
        assert_eq!(
            report.outcome,
            PushOutcome::Enqueued,
            "frame {seq} should still buffer"
        );
    }
    let report = queue.push(plain_frame(999));
    assert_eq!(report.outcome, PushOutcome::DroppedIncoming);
    assert!(matches!(
        report.signal,
        Some(astrs_scheduler::QueueSignal::BackpressureExhausted {
            queue_size: 2,
            buffered: 20
        })
    ));
}

/// The mux side: a node's merged event loop registering a data-plane input
/// (ordinary `NodeEvent::Input` frames) and a control-plane input (the
/// `astrs/status`-style channel a `Stop` would actually arrive on), proving
/// the control lane still pre-empts the data lane when the payload is the
/// crate's real consumer type rather than `Envelope`.
#[test]
fn mux_priority_preemption_holds_for_real_node_events() {
    let mux: EventMux<NodeEvent> = EventMux::new();
    let data = mux
        .register_input(
            DataId::new("frames").unwrap(),
            16,
            QueuePolicy::DropOldest,
            PriorityLane::Data,
        )
        .unwrap();
    let control = mux
        .register_input(
            DataId::new("status").unwrap(),
            4,
            QueuePolicy::DropOldest,
            PriorityLane::Control,
        )
        .unwrap();

    for seq in 0..10 {
        data.push(plain_frame(seq));
    }
    control.push(NodeEvent::Stop {
        cause: StopCause::Requested,
        grace: None,
    });

    let (recv_id, event) = mux.try_recv().expect("something is queued");
    assert_eq!(recv_id, DataId::new("status").unwrap());
    assert!(matches!(event, NodeEvent::Stop { .. }));

    // The data-lane backlog of 10 frames is still there, untouched, and is
    // what the mux serves next once the control lane is drained.
    let (recv_id, event) = mux.try_recv().expect("data lane still has a backlog");
    assert_eq!(recv_id, DataId::new("frames").unwrap());
    assert!(matches!(event, NodeEvent::Input { .. }));
}

#[test]
fn register_from_spec_and_from_spec_both_accept_a_node_event_queue() {
    use astrs_wire::{InputSpec, NodeId, PortRef};

    let spec = InputSpec::new(
        DataId::new("frames").unwrap(),
        PortRef::new(
            NodeId::new("camera").unwrap(),
            DataId::new("image").unwrap(),
        ),
    )
    .with_queue(3, QueuePolicy::DropOldest);

    let queue: InputQueue<NodeEvent> = InputQueue::from_spec(&spec).unwrap();
    assert_eq!(queue.push(plain_frame(1)).outcome, PushOutcome::Enqueued);

    let mux: EventMux<NodeEvent> = EventMux::new();
    let handle = mux.register_from_spec(&spec).unwrap();
    handle.push(plain_frame(2));
    assert_eq!(mux.queue_snapshot(&spec.id).unwrap().depth, 1);
}
