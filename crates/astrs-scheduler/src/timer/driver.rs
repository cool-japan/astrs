//! [`TimerWheelDriver`] — the async production driver for a [`TimerWheel`].
//!
//! Ticks a shared [`TimerWheel`] once per real `tick_period` (the
//! blueprint's "ms resolution" — a millisecond tick period is the intended
//! default) and forwards every [`TimerFired`] it produces over an unbounded
//! channel. For deterministic tests and §14 replay, skip this module
//! entirely and drive a bare [`TimerWheel`] with
//! [`TimerWheel::advance`] directly — see the [`super`] module docs.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use super::p2::JitterStats;
use super::wheel::{TimerFired, TimerId, TimerSpec, TimerWheel};
use crate::sync_util::lock;

/// A cheap-to-clone, thread-safe handle to a [`TimerWheel`] owned by a
/// running [`TimerWheelDriver`].
///
/// Registering and cancelling timers from any task is the intended usage —
/// a daemon reacting to `AddNode`/`RemoveNode` does not need to route
/// through the driver task itself.
#[derive(Clone)]
pub struct TimerWheelHandle {
    wheel: Arc<Mutex<TimerWheel>>,
}

impl TimerWheelHandle {
    /// Registers a new timer against the real current instant. See
    /// [`TimerWheel::insert`].
    pub fn insert_now(&self, spec: TimerSpec) -> TimerId {
        lock(&self.wheel).insert(spec, Instant::now())
    }

    /// Removes a timer. See [`TimerWheel::cancel`].
    pub fn cancel(&self, id: TimerId) -> bool {
        lock(&self.wheel).cancel(id)
    }

    /// Whether `id` is currently registered.
    #[must_use]
    pub fn contains(&self, id: TimerId) -> bool {
        lock(&self.wheel).contains(id)
    }

    /// A cloned snapshot of `id`'s running jitter statistics, if it is
    /// registered. Cloned (rather than borrowed) because the underlying
    /// wheel is behind a lock this handle does not hold open past the
    /// call.
    #[must_use]
    pub fn jitter_stats(&self, id: TimerId) -> Option<JitterStats> {
        lock(&self.wheel).jitter_stats(id).cloned()
    }
}

/// An async task driving a [`TimerWheel`] in real time.
///
/// Dropping or [`abort`](TimerWheelDriver::abort)ing this stops the
/// background task; the [`TimerWheelHandle`]s and the fired-tick receiver
/// returned alongside it remain valid (inert) afterward — inserting into a
/// stopped driver's wheel just accumulates timers nobody is ticking.
pub struct TimerWheelDriver {
    task: JoinHandle<()>,
}

impl TimerWheelDriver {
    /// Spawns a driver ticking a fresh [`TimerWheel`] (epoched at the real
    /// current instant) every `tick_period`, returning the driver itself,
    /// a handle for registering timers, and the channel every
    /// [`TimerFired`] is forwarded on.
    ///
    /// The channel is unbounded: a wheel's ticks are rare and small
    /// compared to a node's data plane, and the alternative (a bounded
    /// channel silently dropping ticks) would itself become another
    /// missed-tick policy this crate would then need to explain.
    #[must_use]
    pub fn spawn(
        tick_period: Duration,
    ) -> (Self, TimerWheelHandle, mpsc::UnboundedReceiver<TimerFired>) {
        let wheel = Arc::new(Mutex::new(TimerWheel::new(Instant::now())));
        let (tx, rx) = mpsc::unbounded_channel();
        let driver_wheel = Arc::clone(&wheel);

        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick_period);
            // The wheel's own `advance` already coalesces or bursts any
            // number of missed *registered timer* periods correctly
            // (`MissedTickPolicy`); there is no reason to also let tokio's
            // *outer* driver loop itself spin rapidly re-firing for a
            // period it fell behind on. `Delay` re-anchors the outer loop
            // to "period after the tick that just ran" instead.
            interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                let _ = interval.tick().await;
                // Routed through `tokio::time::Instant` rather than
                // `std::time::Instant::now()` directly so this driver
                // behaves correctly under `tokio::time::pause`-based
                // testing, should a future test need it — with no
                // observable difference in real (unpaused) operation.
                let now = tokio::time::Instant::now().into_std();
                let fired = lock(&driver_wheel).advance(now);
                for event in fired {
                    if tx.send(event).is_err() {
                        // The receiver was dropped: nobody is listening
                        // anymore, so stop ticking rather than run forever
                        // in the background for no observer.
                        return;
                    }
                }
            }
        });

        (Self { task }, TimerWheelHandle { wheel }, rx)
    }

    /// Stops the driver task immediately.
    pub fn abort(&self) {
        self.task.abort();
    }

    /// Waits for the driver task to finish (only returns on
    /// [`TimerWheelDriver::abort`] or a panic inside the task; the loop
    /// otherwise runs until the fired-tick receiver is dropped).
    ///
    /// # Errors
    ///
    /// Propagates the task's [`tokio::task::JoinError`] (panic or abort).
    pub async fn join(self) -> Result<(), tokio::task::JoinError> {
        self.task.await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::timer::MissedTickPolicy;
    use astrs_time::TimerInterval;

    #[tokio::test]
    async fn driver_delivers_ticks_for_a_registered_timer() {
        let (driver, handle, mut rx) = TimerWheelDriver::spawn(Duration::from_millis(2));
        let id = handle.insert_now(TimerSpec::new(
            TimerInterval::from_millis(10).unwrap(),
            MissedTickPolicy::Burst,
        ));

        let fired = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("driver should deliver a tick within 5s")
            .expect("channel should not close while the driver runs");
        assert_eq!(fired.id, id);

        driver.abort();
    }

    #[tokio::test]
    async fn cancel_through_the_handle_stops_further_delivery() {
        let (driver, handle, mut rx) = TimerWheelDriver::spawn(Duration::from_millis(1));
        let id = handle.insert_now(TimerSpec::new(
            TimerInterval::from_millis(5).unwrap(),
            MissedTickPolicy::Burst,
        ));
        assert!(handle.contains(id));
        assert!(handle.cancel(id));
        assert!(!handle.contains(id));

        // Drain anything already in flight, then confirm nothing more
        // arrives for a window comfortably longer than the timer's period.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            assert_ne!(event.id, id, "a cancelled timer must not keep firing");
        }

        driver.abort();
    }

    #[tokio::test]
    async fn jitter_stats_are_reachable_through_the_handle() {
        let (driver, handle, mut rx) = TimerWheelDriver::spawn(Duration::from_millis(2));
        let id = handle.insert_now(TimerSpec::new(
            TimerInterval::from_millis(10).unwrap(),
            MissedTickPolicy::Burst,
        ));
        assert!(handle.jitter_stats(id).unwrap().samples() == 0);

        let _ = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("driver should deliver a tick within 5s");

        assert!(handle.jitter_stats(id).unwrap().samples() >= 1);
        driver.abort();
    }

    /// `N` registered [`TimerSpec`] subscriptions through the real async
    /// driver at once — every other driver test here registers exactly
    /// one. Deliberately asserts nothing about *how many* times each timer
    /// fired or in what relative order: a 1ms `tokio::interval` on a loaded
    /// CI machine can fall behind (`MissedTickBehavior::Delay` on this
    /// driver only prevents it from then *spinning* to catch up, not from
    /// falling behind in the first place), so an exact per-period tick
    /// count is a property of machine load, not of this driver's
    /// correctness — [`TimerWheel`]'s own synchronous tests already cover
    /// exact counts deterministically without real time in the loop at
    /// all. What this test can honestly prove, and does: every one of the
    /// `N` timers fires at least once within a generous window, each
    /// [`TimerFired::tag`] always matches the timer it was registered
    /// with, and a cancelled timer never fires again.
    #[tokio::test]
    async fn many_registered_timers_all_fire_through_the_real_driver_with_tags_intact() {
        const N: u64 = 40;

        let (driver, handle, mut rx) = TimerWheelDriver::spawn(Duration::from_millis(1));
        let mut tag_by_id: std::collections::HashMap<TimerId, u64> =
            std::collections::HashMap::with_capacity(N as usize);
        for tag in 0..N {
            // Periods spread across a small range so timers cascade past
            // each other rather than all firing in lockstep.
            let period_ms = 5 + (tag % 7);
            let id = handle.insert_now(
                TimerSpec::new(
                    TimerInterval::from_millis(period_ms).unwrap(),
                    MissedTickPolicy::Burst,
                )
                .with_tag(tag),
            );
            tag_by_id.insert(id, tag);
        }
        // Cancel one immediately; it must never appear in what follows.
        let cancelled = *tag_by_id.keys().next().expect("N > 0");
        assert!(handle.cancel(cancelled));
        tag_by_id.remove(&cancelled);

        let mut fired_ids: std::collections::HashSet<TimerId> = std::collections::HashSet::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while fired_ids.len() < tag_by_id.len() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let event = tokio::time::timeout(remaining, rx.recv())
                .await
                .expect("every surviving timer should fire within 5s")
                .expect("channel should not close while the driver runs");

            assert_ne!(event.id, cancelled, "a cancelled timer must never fire");
            let expected_tag = tag_by_id
                .get(&event.id)
                .unwrap_or_else(|| panic!("fired id {:?} was never registered", event.id));
            assert_eq!(
                event.tag, *expected_tag,
                "tag must match this timer's registration"
            );
            fired_ids.insert(event.id);
        }

        assert_eq!(
            fired_ids.len(),
            tag_by_id.len(),
            "every surviving timer fired at least once"
        );
        driver.abort();
    }
}
