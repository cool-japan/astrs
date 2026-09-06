//! Timer-wheel determinism driven entirely by `astrs_time::ManualClock`
//! (blueprint §14: `astrs run --deterministic` fixes the timer wheel to a
//! recorded clock stream instead of real time).
//!
//! Every timer-wheel test living alongside the wheel itself
//! (`src/timer/wheel.rs`) advances it with `Instant::now() + Duration`
//! arithmetic directly — sufficient to prove the wheel's own tick logic,
//! but it never actually exercises `astrs_time::ManualClock`, the type this
//! crate's docs point to for deterministic tests and replay (see
//! `TimerWheel`'s and this crate's own module docs). This file drives the
//! wheel exclusively through `ManualClock::advance` +
//! `astrs_time::Clock::now_instant` instead: no sleeping, no wall-clock
//! reads, and every `Instant` the wheel ever sees comes from the manual
//! clock's own bookkeeping.
//!
//! Two traps a naive version of this test would hit:
//!
//! - `TimerInterval::from_millis`/`from_hz` anchor their tick grid at the
//!   real `Instant::now()` of the call, not at the manual clock's starting
//!   point — every interval here is explicitly `.rebase()`d onto the
//!   clock's own epoch instant before use.
//! - An `hz` spec's period is only wheel-observable at exact millisecond
//!   boundaries when the reciprocal itself lands on one; `hz(20.0)` (50ms)
//!   is used here for that reason, not a value like `hz(60.0)` whose
//!   16.6667ms period the wheel's own `a_60hz_timer_never_fires_before_its_true_grid_point`
//!   test already covers separately.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_scheduler::{MissedTickPolicy, TimerFired, TimerId, TimerSpec, TimerWheel};
use astrs_time::{Clock, ManualClock, TimerInterval};
use std::time::{Duration, Instant};

/// Builds a fresh wheel with two timers — a 10ms-period one (tag `1`) and a
/// `hz(20.0)` = 50ms-period one (tag `2`) — both rebased onto `clock`'s own
/// starting instant, plus that starting instant itself for callers that
/// need to compute expected absolute tick instants.
fn wheel_with_two_timers(clock: &ManualClock) -> (TimerWheel, TimerId, TimerId, Instant) {
    let epoch = clock.now_instant();
    let mut wheel = TimerWheel::new(epoch);
    let millis_10 = TimerInterval::from_millis(10).unwrap().rebase(epoch);
    let hz_20 = TimerInterval::from_hz(20.0).unwrap().rebase(epoch);
    assert_eq!(
        hz_20.period(),
        Duration::from_millis(50),
        "hz(20.0) must be an exact 50ms period for this test's boundaries to be observable"
    );
    let fast = wheel.insert(
        TimerSpec::new(millis_10, MissedTickPolicy::Burst).with_tag(1),
        epoch,
    );
    let slow = wheel.insert(
        TimerSpec::new(hz_20, MissedTickPolicy::Burst).with_tag(2),
        epoch,
    );
    (wheel, fast, slow, epoch)
}

/// Drives `wheel` from `clock` in fixed 1ms simulated steps for `total_ms`
/// steps, with no real sleeping at all, collecting every fired tick in
/// order.
fn run_1ms_stepped(clock: &ManualClock, wheel: &mut TimerWheel, total_ms: u64) -> Vec<TimerFired> {
    let mut all = Vec::new();
    for _ in 0..total_ms {
        clock.advance(Duration::from_millis(1));
        all.extend(wheel.advance(clock.now_instant()));
    }
    all
}

#[test]
fn exact_tick_counts_and_instants_for_millis_and_hz_specs_over_a_clean_window() {
    let clock = ManualClock::new(0);
    let (mut wheel, fast, slow, epoch) = wheel_with_two_timers(&clock);

    let fired = run_1ms_stepped(&clock, &mut wheel, 100);

    let fast_ticks: Vec<&TimerFired> = fired.iter().filter(|f| f.id == fast).collect();
    let slow_ticks: Vec<&TimerFired> = fired.iter().filter(|f| f.id == slow).collect();
    assert_eq!(
        fast_ticks.len(),
        10,
        "10ms period over a 100ms window: 10 ticks"
    );
    assert_eq!(
        slow_ticks.len(),
        2,
        "50ms period (hz 20) over a 100ms window: 2 ticks"
    );
    assert_eq!(fired.len(), 12, "no ticks beyond the two timers' own");

    for (index, tick) in fast_ticks.iter().enumerate() {
        assert_eq!(tick.tag, 1);
        assert_eq!(
            tick.skipped, 0,
            "1ms-stepped driving should never miss a tick"
        );
        let expected = epoch + Duration::from_millis(10) * (index as u32 + 1);
        assert_eq!(tick.scheduled_at, expected, "fast tick {index}");
    }
    for (index, tick) in slow_ticks.iter().enumerate() {
        assert_eq!(tick.tag, 2);
        assert_eq!(tick.skipped, 0);
        let expected = epoch + Duration::from_millis(50) * (index as u32 + 1);
        assert_eq!(tick.scheduled_at, expected, "slow tick {index}");
    }
}

#[test]
fn missed_tick_policies_produce_the_documented_catch_up_behaviour_when_driven_by_manualclock() {
    // `Skip`: one big `ManualClock` jump (5.5 periods) collapses into a
    // single coalesced delivery reporting the periods it absorbed.
    let clock = ManualClock::new(0);
    let epoch = clock.now_instant();
    let interval = TimerInterval::from_millis(10).unwrap().rebase(epoch);
    let mut wheel = TimerWheel::new(epoch);
    let id = wheel.insert(
        TimerSpec::new(interval, MissedTickPolicy::Skip).with_tag(7),
        epoch,
    );

    clock.advance(Duration::from_millis(55));
    let fired = wheel.advance(clock.now_instant());

    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].id, id);
    assert_eq!(fired[0].tag, 7);
    assert_eq!(fired[0].scheduled_at, epoch + Duration::from_millis(10));
    // Grid points at 20,30,40,50ms all fall in (10ms, 55ms]: 4 skipped.
    assert_eq!(fired[0].skipped, 4);

    // `Burst`: the identical jump, same period, delivers all 5 individually.
    let clock2 = ManualClock::new(0);
    let epoch2 = clock2.now_instant();
    let interval2 = TimerInterval::from_millis(10).unwrap().rebase(epoch2);
    let mut wheel2 = TimerWheel::new(epoch2);
    let id2 = wheel2.insert(
        TimerSpec::new(interval2, MissedTickPolicy::Burst).with_tag(8),
        epoch2,
    );

    clock2.advance(Duration::from_millis(55));
    let fired2 = wheel2.advance(clock2.now_instant());

    assert_eq!(fired2.len(), 5);
    assert!(
        fired2
            .iter()
            .all(|f| f.id == id2 && f.tag == 8 && f.skipped == 0)
    );
    let expected: Vec<Instant> = (1..=5)
        .map(|k| epoch2 + Duration::from_millis(10) * k)
        .collect();
    let actual: Vec<Instant> = fired2.iter().map(|f| f.scheduled_at).collect();
    assert_eq!(actual, expected);
}

/// A summary of one [`TimerFired`] expressed relative to its own run's
/// epoch, so two independent `ManualClock`+`TimerWheel` pairs — whose raw
/// `Instant`s necessarily differ, since each `ManualClock` captures its own
/// `base = Instant::now()` at construction — can still be compared for
/// exact structural equality.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FiredSummary {
    tag: u64,
    scheduled_offset_ms: u128,
    fired_offset_ms: u128,
    skipped: u64,
}

/// Runs an arbitrary (not a clean multiple of either period), mixed
/// 1ms-stepped-then-one-big-jump scenario against a fresh `ManualClock` and
/// `TimerWheel`, and returns every fired tick as an epoch-relative summary.
fn run_deterministic_scenario() -> Vec<FiredSummary> {
    let clock = ManualClock::new(0);
    let (mut wheel, _fast, _slow, epoch) = wheel_with_two_timers(&clock);

    let mut fired = run_1ms_stepped(&clock, &mut wheel, 37);
    // A single large jump, exercising the `Burst` catch-up path identically
    // across both runs on top of the 1ms-stepped prefix.
    clock.advance(Duration::from_millis(63));
    fired.extend(wheel.advance(clock.now_instant()));

    fired
        .into_iter()
        .map(|f| FiredSummary {
            tag: f.tag,
            scheduled_offset_ms: f.scheduled_at.saturating_duration_since(epoch).as_millis(),
            fired_offset_ms: f.fired_at.saturating_duration_since(epoch).as_millis(),
            skipped: f.skipped,
        })
        .collect()
}

#[test]
fn identical_manualclock_operation_sequences_produce_bit_for_bit_identical_results() {
    // The actual determinism property §14 replay depends on: the same
    // sequence of clock operations against a fresh wheel produces the same
    // sequence of fired ticks, run after run — no dependence on wall-clock
    // timing, thread scheduling, or anything outside the two operation
    // sequences themselves.
    let run1 = run_deterministic_scenario();
    let run2 = run_deterministic_scenario();
    assert_eq!(run1, run2);
    assert!(
        !run1.is_empty(),
        "the scenario must actually exercise both timers"
    );
    assert!(
        run1.iter().any(|f| f.tag == 1) && run1.iter().any(|f| f.tag == 2),
        "both timers must have fired at least once across the scenario"
    );
}

#[test]
fn manualclock_epoch_and_wheel_epoch_stay_in_lockstep_with_no_wall_clock_reads() {
    // A direct check that nothing here secretly falls back to real time:
    // advancing the `ManualClock` by exactly one timer period must produce
    // exactly one fired tick, however many (simulated) milliseconds of
    // process wall-clock time actually elapse while the test runs.
    let clock = ManualClock::new(0);
    let epoch = clock.now_instant();
    let interval = TimerInterval::from_millis(5).unwrap().rebase(epoch);
    let mut wheel = TimerWheel::new(epoch);
    let id = wheel.insert(TimerSpec::new(interval, MissedTickPolicy::Burst), epoch);

    for expected_tick in 1..=20u32 {
        clock.advance(Duration::from_millis(5));
        let fired = wheel.advance(clock.now_instant());
        assert_eq!(fired.len(), 1, "tick {expected_tick}");
        assert_eq!(fired[0].id, id);
        assert_eq!(
            fired[0].scheduled_at,
            epoch + Duration::from_millis(5) * expected_tick
        );
        assert_eq!(fired[0].skipped, 0);
    }
}

/// The intended "sleep until actually due" usage `TimerWheel::next_deadline`
/// exists for (its own docs): instead of stepping in fixed 1ms increments,
/// jump the clock straight to `next_deadline()` before each `advance` call.
/// Driven entirely by `ManualClock`, this both proves `next_deadline`
/// tracks the earliest of several out-of-phase timers correctly across
/// many reschedules and demonstrates the pattern is exact — no missed or
/// early ticks — when a caller skips straight to the computed deadline
/// instead of polling.
#[test]
fn next_deadline_driven_stepping_delivers_every_tick_with_no_polling() {
    let clock = ManualClock::new(0);
    let epoch = clock.now_instant();
    let mut wheel = TimerWheel::new(epoch);
    let a = wheel.insert(
        TimerSpec::new(
            TimerInterval::from_millis(7).unwrap().rebase(epoch),
            MissedTickPolicy::Burst,
        )
        .with_tag(1),
        epoch,
    );
    let b = wheel.insert(
        TimerSpec::new(
            TimerInterval::from_millis(11).unwrap().rebase(epoch),
            MissedTickPolicy::Burst,
        )
        .with_tag(2),
        epoch,
    );

    let mut fired_a = 0u32;
    let mut fired_b = 0u32;
    let end = epoch + Duration::from_millis(200);
    loop {
        let Some(deadline) = wheel.next_deadline() else {
            panic!("both timers remain registered for the whole test");
        };
        if deadline > end {
            break;
        }
        let jump = deadline.saturating_duration_since(clock.now_instant());
        clock.advance(jump);
        for event in wheel.advance(clock.now_instant()) {
            assert_eq!(
                event.skipped, 0,
                "jumping exactly to next_deadline must never miss a tick"
            );
            assert_eq!(
                event.scheduled_at, event.fired_at,
                "arriving exactly on the deadline: zero jitter"
            );
            if event.id == a {
                fired_a += 1;
            } else if event.id == b {
                fired_b += 1;
            } else {
                panic!("unexpected timer id {:?}", event.id);
            }
        }
    }

    assert_eq!(fired_a, 200 / 7, "7ms period over a 200ms window");
    assert_eq!(fired_b, 200 / 11, "11ms period over a 200ms window");
}
