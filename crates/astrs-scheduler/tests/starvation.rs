//! No-starvation proof for [`EventMux`]'s within-lane fair round robin
//! (blueprint §11.3).
//!
//! A two-input round-robin test (one hot, one quiet) can pass by accident —
//! with only two inputs, "alternate" and "fair" look identical. These tests
//! use enough inputs that the two would visibly diverge, and check both
//! halves of the fairness claim documented on [`EventMux::try_recv`]:
//! a message waiting on a quieter input is served within a bounded number
//! of calls regardless of how hot its neighbors are, and over a long run
//! with everything equally hot, service is split (exactly, or within one)
//! evenly.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_scheduler::{Envelope, EventMux};
use astrs_wire::{DataId, PriorityLane, QueuePolicy};
use std::collections::HashMap;

fn id(name: &str) -> DataId {
    DataId::new(name).unwrap()
}

/// One hot input refilled generously up front cannot delay a quiet
/// neighbor's single pending message past `M` calls, where `M` is the
/// number of inputs in the lane — the bound [`EventMux::try_recv`]
/// documents.
#[test]
fn a_pending_message_on_a_cold_input_is_served_within_m_calls() {
    const HOT_COUNT: usize = 4;
    const M: usize = HOT_COUNT + 1; // + the one cold input

    let mux: EventMux<Envelope<u32>> = EventMux::new();
    let mut hot_handles = Vec::new();
    for i in 0..HOT_COUNT {
        let handle = mux
            .register_input(
                id(&format!("hot{i}")),
                10_000,
                QueuePolicy::DropOldest,
                PriorityLane::Data,
            )
            .unwrap();
        // Preloaded well past `M` so it cannot possibly run dry within the
        // `M` calls this test makes.
        for n in 0..(M as u32 + 10) {
            handle.push(Envelope::new(n));
        }
        hot_handles.push(handle);
    }
    // Registered *last*, which is the worst case for it under round robin:
    // every hot input is visited at least once before rotation reaches it.
    let cold = mux
        .register_input(id("cold"), 4, QueuePolicy::DropOldest, PriorityLane::Data)
        .unwrap();
    cold.push(Envelope::new(999));

    let mut served_cold_within = None;
    for call in 1..=M {
        let (recv_id, _) = mux
            .try_recv()
            .expect("every input has at least one message ready");
        if recv_id == id("cold") {
            served_cold_within = Some(call);
            break;
        }
    }

    assert_eq!(
        served_cold_within,
        Some(M),
        "the cold input must be served by call {M} even in the worst-case rotation order"
    );
}

/// Strengthens the two-input case: with `M` inputs all continuously hot,
/// running exactly `M * k` calls splits service *exactly* evenly (`k` each)
/// — round robin visits every input once per lap, and with none of them
/// ever running dry there is no "skip an empty slot" event to unbalance it.
#[test]
fn m_equally_hot_inputs_split_service_exactly_evenly_over_a_clean_multiple_of_m() {
    const M: usize = 6;
    const LAPS: u32 = 1000;

    let mux: EventMux<Envelope<u32>> = EventMux::new();
    for i in 0..M {
        let handle = mux
            .register_input(
                id(&format!("in{i}")),
                10_000,
                QueuePolicy::DropOldest,
                PriorityLane::Data,
            )
            .unwrap();
        for n in 0..LAPS {
            handle.push(Envelope::new(n));
        }
    }

    let mut counts: HashMap<DataId, u32> = HashMap::new();
    for _ in 0..(M as u32 * LAPS) {
        let (recv_id, _) = mux
            .try_recv()
            .expect("no input should run dry within this many calls");
        *counts.entry(recv_id).or_insert(0) += 1;
    }

    assert_eq!(
        counts.len(),
        M,
        "every input must have been served at least once"
    );
    for (input, count) in &counts {
        assert_eq!(
            *count, LAPS,
            "{input} got {count}, expected exactly {LAPS} for a clean multiple of M"
        );
    }
}

/// The same setup, but with a call count that is *not* a clean multiple of
/// `M`: service still splits within one of perfectly even, matching the
/// documented bound rather than exact equality.
#[test]
fn m_equally_hot_inputs_split_service_within_one_over_an_uneven_call_count() {
    const M: usize = 5;
    const TOTAL_CALLS: u32 = 5 * 1000 + 3; // not a multiple of M

    let mux: EventMux<Envelope<u32>> = EventMux::new();
    for i in 0..M {
        let handle = mux
            .register_input(
                id(&format!("in{i}")),
                10_000,
                QueuePolicy::DropOldest,
                PriorityLane::Data,
            )
            .unwrap();
        for n in 0..TOTAL_CALLS {
            handle.push(Envelope::new(n));
        }
    }

    let mut counts: HashMap<DataId, u32> = HashMap::new();
    for _ in 0..TOTAL_CALLS {
        let (recv_id, _) = mux
            .try_recv()
            .expect("no input should run dry within this many calls");
        *counts.entry(recv_id).or_insert(0) += 1;
    }

    let min = *counts.values().min().expect("at least one input");
    let max = *counts.values().max().expect("at least one input");
    assert!(
        max - min <= 1,
        "service counts {counts:?} span more than one"
    );
    let total: u32 = counts.values().sum();
    assert_eq!(total, TOTAL_CALLS);
}

/// Removing an input mid-run must not desynchronize the rotation position
/// for the ones that remain — the exact failure mode an index-based cursor
/// (rather than the last-served-id anchor this mux uses) would hit.
#[test]
fn unregistering_an_input_mid_run_does_not_break_fairness_for_the_rest() {
    let mux: EventMux<Envelope<u32>> = EventMux::new();
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let handle = mux
                .register_input(
                    id(&format!("in{i}")),
                    10_000,
                    QueuePolicy::DropOldest,
                    PriorityLane::Data,
                )
                .unwrap();
            for n in 0..200u32 {
                handle.push(Envelope::new(n));
            }
            handle
        })
        .collect();

    // Serve a handful of rounds before removing one input.
    for _ in 0..8 {
        let _ = mux.try_recv();
    }
    assert!(mux.unregister_input(&id("in1")));
    drop(handles);

    // The three survivors must now split service evenly among themselves.
    let mut counts: HashMap<DataId, u32> = HashMap::new();
    for _ in 0..300u32 {
        let (recv_id, _) = mux
            .try_recv()
            .expect("the three survivors are still generously stocked");
        assert_ne!(
            recv_id,
            id("in1"),
            "a removed input must never be served again"
        );
        *counts.entry(recv_id).or_insert(0) += 1;
    }

    assert_eq!(
        counts.len(),
        3,
        "exactly the three surviving inputs were served"
    );
    let min = *counts.values().min().unwrap();
    let max = *counts.values().max().unwrap();
    assert!(
        max - min <= 1,
        "service counts {counts:?} span more than one after the removal"
    );
}
