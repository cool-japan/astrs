//! Integration test: [`HlcClock`], [`Stamped`], [`TimerInterval`], and
//! [`Deadline`] composed together in a simulated event loop, all driven by
//! one shared [`ManualClock`] — the composition pattern downstream crates
//! (astrs-daemon's supervisor loop, astrs-scheduler's timer wheel,
//! astrs-node-api's event stream) are expected to use, exercised
//! deterministically with no real sleeping.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use astrs_time::{Clock, Deadline, HlcClock, ManualClock, Stamped, TimerInterval};

/// A minimal simulated event payload: which tick fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Tick(u32);

#[test]
fn simulated_event_loop_composes_hlc_clock_stamped_timer_and_deadline() {
    // One shared clock drives everything — `HlcClock` (via the `Clock` for
    // `Arc<C>` blanket impl) and the raw `Instant` queries `TimerInterval`/
    // `Deadline` need, so advancing it once keeps all three in lockstep.
    let shared_clock = Arc::new(ManualClock::new(1_000_000_000));
    let hlc = HlcClock::new(Arc::clone(&shared_clock));
    let start = shared_clock.now_instant();

    // Simulates parsing a manifest's `astrs/timer/millis/100` virtual
    // source at `astrs validate` time, then re-anchoring it at the instant
    // the dataflow actually starts — the production flow documented on
    // `TimerInterval`.
    let parsed_at_validate_time =
        TimerInterval::from_virtual_source_path("astrs/timer/millis/100").unwrap();
    let timer = parsed_at_validate_time.rebase(start);

    // A 350ms budget for "the operation this loop is running under".
    let deadline = Deadline::after(start, Duration::from_millis(350)).unwrap();

    let mut events: Vec<Stamped<Tick>> = Vec::new();
    let mut now = start;
    let mut expired_at_tick = None;

    for i in 0..5u32 {
        let next = timer.next_tick(now);
        // Advance the shared clock by exactly the gap to the next grid
        // point, landing `now_instant()` exactly on it — deterministic,
        // no real sleeping, and exercises the same "wall clock and
        // monotonic clock both move together" path `ManualClock::advance`
        // is documented for.
        let gap = next.duration_since(now);
        shared_clock.advance(gap);
        now = shared_clock.now_instant();
        assert_eq!(now, next, "the loop must land exactly on the timer's grid");

        events.push(hlc.stamp(Tick(i)));

        if expired_at_tick.is_none() && deadline.is_expired(now) {
            expired_at_tick = Some(i);
        }
    }

    // 5 ticks * 100ms = 500ms elapsed; the 350ms deadline crosses between
    // tick 3 (300ms elapsed) and tick 4 (400ms elapsed).
    assert_eq!(expired_at_tick, Some(3));
    assert!(!deadline.is_expired(start + Duration::from_millis(300)));
    assert!(deadline.is_expired(start + Duration::from_millis(400)));

    // Every stamped event's HLC timestamp strictly increases tick over
    // tick, and `Stamped<Tick>`'s derived `Ord` (ts-first) already
    // reflects that: the events are emitted in `.sort()`-order without
    // needing to sort them.
    assert_eq!(events.len(), 5);
    for pair in events.windows(2) {
        assert!(pair[0].ts < pair[1].ts);
        assert!(pair[0] < pair[1]);
    }
    let mut sorted = events.clone();
    sorted.sort();
    assert_eq!(
        sorted, events,
        "events were already emitted in causal order"
    );

    // The tick payload order matches emission order too (sanity: `map`/
    // `as_ref` don't disturb identity).
    let tick_indices: Vec<u32> = events.iter().map(|e| e.inner.0).collect();
    assert_eq!(tick_indices, vec![0, 1, 2, 3, 4]);

    // `TimerInterval`'s own bookkeeping agrees with the loop: 5 completed
    // periods have elapsed by the end, and none were "missed" since this
    // loop always caught up to the exact next grid point before advancing
    // again.
    assert_eq!(timer.ticks_elapsed(now), 5);
    assert_eq!(timer.ticks_between(start, now), 5);

    // The last-emitted timestamp equals the clock's own idea of "the last
    // timestamp issued" — no event was silently dropped or duplicated.
    assert_eq!(events.last().map(|e| e.ts), Some(hlc.last()));
}

#[test]
fn a_burst_of_same_instant_events_still_produces_strictly_ordered_stamps() {
    // A scheduler that batches several events for the same tick (e.g. fan-
    // in from multiple inputs firing "at once") still gets a total order
    // out of `HlcClock::stamp`, because the wall clock is frozen but the
    // logical counter is not.
    let clock = HlcClock::new(ManualClock::new(1_000_000_000));
    let batch: Vec<Stamped<u32>> = (0..16u32).map(|i| clock.stamp(i)).collect();

    for pair in batch.windows(2) {
        assert!(pair[0].ts < pair[1].ts);
    }
    // Same physical nanosecond throughout (the `ManualClock` never
    // advanced), only the logical counter moved.
    let physical: Vec<u64> = batch.iter().map(|e| e.ts.physical_ns()).collect();
    assert!(physical.iter().all(|&p| p == physical[0]));
    let logical: Vec<u32> = batch.iter().map(|e| e.ts.logical()).collect();
    assert_eq!(logical, (0..16).collect::<Vec<_>>());
}
