//! Composition proof: the four pieces this crate exports (`InputQueue` via
//! `EventMux`, `TimerWheelDriver`, `DeadlineMonitor`) wired together the way
//! a real daemon or node-api event loop would, not exercised in isolation.
//!
//! The crate's own docs are explicit that composition is the caller's job
//! ("Four independent pieces, composed by the caller ... not by this
//! crate"); this file is the test that the seams between them actually fit
//! — a real async [`TimerWheelDriver`] forwarding fired ticks into an
//! [`EventMux`] input by [`TimerFired::tag`], mixed with an ordinary data
//! stream on another input, with a [`DeadlineMonitor`] timing the whole
//! receive-to-"produce a response" path.
//!
//! # What this test does and does not assert
//!
//! It does **not** assert that the control-lane timer tick is always
//! observed strictly ahead of a concurrently-arriving data-lane backlog at
//! the instant of a given `recv()` call — whether a message is *queued yet*
//! when a real, independently-scheduled producer task is involved is
//! inherently timing-dependent, and asserting it here would just be a
//! slower, flakier repeat of what `mux.rs`'s
//! `control_lane_preempts_data_lane_regardless_of_backlog_size` and
//! `node_event_integration.rs`'s `mux_priority_preemption_holds_for_real_node_events`
//! already prove deterministically (both push everything up front, with no
//! concurrent producer, before ever calling `recv`). What *is* both true and
//! honestly testable under real concurrency is delivery itself: every timer
//! tick the glue task forwards is eventually received, with its tag intact,
//! and every count reconciles.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_scheduler::{
    DeadlineMonitor, Envelope, EventMux, MissedTickPolicy, TimerSpec, TimerWheelDriver,
};
use astrs_time::TimerInterval;
use astrs_wire::{DataId, PriorityLane, QueuePolicy};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

/// A node's payload: either an ordinary data frame or a forwarded timer
/// tick carrying the tag the daemon registered it under.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Payload {
    Frame(u32),
    Tick(u64),
}

#[tokio::test]
async fn timer_driver_ticks_flow_through_the_mux_alongside_data_and_deadlines_are_measured() {
    const TICK_COUNT: u64 = 20;

    // -- Wiring, exactly as a daemon's node-loop setup would do it. --
    let mux: Arc<EventMux<Envelope<Payload>>> = Arc::new(EventMux::new());
    let frames = mux
        .register_input(
            DataId::new("frames").unwrap(),
            64,
            QueuePolicy::DropOldest,
            PriorityLane::Data,
        )
        .unwrap();
    let timer_input = mux
        .register_input(
            DataId::new("timer_5ms").unwrap(),
            64,
            QueuePolicy::DropOldest,
            PriorityLane::Control,
        )
        .unwrap();

    let (driver, handle, mut ticks) = TimerWheelDriver::spawn(Duration::from_millis(1));
    let timer_id = handle.insert_now(
        TimerSpec::new(
            TimerInterval::from_millis(5).unwrap(),
            MissedTickPolicy::Burst,
        )
        .with_tag(42),
    );

    let deadlines: Arc<DeadlineMonitor<&'static str>> = Arc::new(DeadlineMonitor::new());
    deadlines.register("frame->ack", Duration::from_millis(200));

    // -- The glue a daemon writes: forward every fired tick into the
    // timer's mux input as an ordinary control-lane message. --
    let forwarder = tokio::spawn(async move {
        let mut forwarded = 0u64;
        while forwarded < TICK_COUNT {
            let fired = ticks
                .recv()
                .await
                .expect("driver channel stays open while ticks remain");
            timer_input.push(Envelope::new(Payload::Tick(fired.tag)));
            forwarded += 1;
        }
        forwarded
    });

    // -- A data producer, running concurrently with the timer forwarder. --
    let data_producer = tokio::spawn(async move {
        for n in 0..50u32 {
            frames.push(Envelope::new(Payload::Frame(n)));
            tokio::task::yield_now().await;
        }
    });

    // -- The node's own event loop: drain the mux, timing one measurement
    // per received frame against `deadlines`, until every forwarded tick
    // has been seen (data messages may still be arriving/draining
    // alongside; only the tick count gates completion, since that count is
    // known exactly up front). --
    let mut tick_count = 0u64;
    let mut seen_tags: HashSet<u64> = HashSet::new();
    let mut frame_count = 0u32;
    // Gated on the *count* of ticks received, not on how many distinct tags
    // have been seen: with only one timer registered, every tick shares the
    // same tag, so a set-cardinality gate would never reach `TICK_COUNT` and
    // this loop would wait on `mux.recv()` forever once both producer tasks
    // (which are moved into their own `tokio::spawn` and so can never push
    // again after they return) have exhausted everything they were ever
    // going to send.
    while tick_count < TICK_COUNT {
        let (_, event) = mux.recv().await;
        // Each frame's own measurement starts fresh right as it is
        // received off the mux -- the input-to-output latency this models
        // is "time from this receive to this frame's own ack", not time
        // since the test began or time spent waiting for the next message,
        // so every token gets its own start instant taken here rather than
        // sharing one hoisted before the loop (or one taken before the
        // `await`, which would fold queue-wait time into the measurement).
        let receive_instant = tokio::time::Instant::now().into_std();
        match event.payload {
            Payload::Tick(tag) => {
                assert_eq!(
                    tag, 42,
                    "every forwarded tick must carry the tag it was registered with"
                );
                seen_tags.insert(tag);
                tick_count += 1;
            }
            Payload::Frame(_) => {
                frame_count += 1;
                let token = deadlines
                    .start(&"frame->ack", receive_instant)
                    .expect("registered above");
                let outcome = deadlines.finish_now(token);
                assert!(
                    !outcome.is_violated(),
                    "a same-process, in-memory hop must not blow a 200ms budget"
                );
            }
        }
    }

    forwarder.await.expect("forwarder task must not panic");
    data_producer
        .await
        .expect("data producer task must not panic");
    driver.abort();

    assert_eq!(
        tick_count, TICK_COUNT,
        "every forwarded tick must have been received exactly once"
    );
    assert_eq!(
        seen_tags.len(),
        1,
        "all forwarded ticks share the single registered timer's tag"
    );
    assert!(
        frame_count > 0,
        "at least some data-plane frames must have been observed too"
    );

    let snapshot = deadlines.snapshot(&"frame->ack").expect("registered");
    assert_eq!(snapshot.samples, u64::from(frame_count));
    assert_eq!(snapshot.violations, 0);

    // The queues themselves must have delivered everything they accepted;
    // nothing manufactured out of thin air, nothing silently duplicated
    // beyond what was actually pushed.
    let frames_snapshot = mux.queue_snapshot(&DataId::new("frames").unwrap()).unwrap();
    assert_eq!(frames_snapshot.delivered, u64::from(frame_count));
    let timer_snapshot = mux
        .queue_snapshot(&DataId::new("timer_5ms").unwrap())
        .unwrap();
    assert_eq!(timer_snapshot.delivered, TICK_COUNT);

    assert!(handle.contains(timer_id));
}
