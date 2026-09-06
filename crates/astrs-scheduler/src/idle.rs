//! [`IdleWatchdog`] — per-input silence detection (blueprint §8.3
//! `timeout`, §12 "routes to an unreachable daemon flip to `InputClosed`
//! after `timeout`, recover with `InputRecovered`").
//!
//! [`astrs_wire::InputSpec::timeout`] is the one field of that struct none
//! of this crate's other pieces read: [`crate::InputQueue`] and
//! [`crate::EventMux`] consume `queue_size`/`queue_policy`/`priority_lane`,
//! [`crate::DeadlineMonitor`] consumes `deadline`, and `timeout` — "declare
//! the input closed if nothing arrives for this long" — is a distinct
//! question from either: not "was this measured span too slow"
//! ([`crate::DeadlineMonitor`]'s job) but "has this input gone silent for
//! too long". `IdleWatchdog` is that check.
//!
//! # Edge-triggered, not level-triggered
//!
//! A naive silence check ("is `now - last_seen` past `timeout`?") is
//! correct but, called on every health-check tick, would report the same
//! violation over and over for as long as the input stays silent —
//! spamming the caller with what the blueprint's own vocabulary treats as a
//! single state transition (`InputClosed`, once, not repeated every 5
//! seconds while the route stays down). [`IdleWatchdog::poll_all`] instead
//! reports a key only on the tick it *first* crosses `timeout` since its
//! last [`IdleWatchdog::touch`], via an internal `escalated` flag; the next
//! `touch` (traffic resuming) clears that flag and reports whether it was
//! set, so a caller can emit the matching `InputRecovered` exactly once as
//! well. [`IdleWatchdog::snapshot`] is the complementary level-triggered,
//! non-mutating read — "how long has this been silent right now" — for a
//! metrics gauge or a TUI display that wants the current value on every
//! poll rather than only the edges.
//!
//! # Examples
//!
//! ```
//! use astrs_scheduler::IdleWatchdog;
//! use std::time::{Duration, Instant};
//!
//! let watchdog: IdleWatchdog<&str> = IdleWatchdog::new();
//! let t0 = Instant::now();
//! watchdog.register("camera/frames", Duration::from_millis(500), t0);
//!
//! // Traffic keeps arriving well within budget: never escalates.
//! watchdog.touch(&"camera/frames", t0 + Duration::from_millis(100));
//! assert!(watchdog.poll_all(t0 + Duration::from_millis(200)).is_empty());
//!
//! // The producer goes quiet past the timeout: exactly one violation.
//! let violations = watchdog.poll_all(t0 + Duration::from_millis(650));
//! assert_eq!(violations.len(), 1);
//! assert_eq!(violations[0].key, "camera/frames");
//!
//! // Polling again while still silent reports nothing new.
//! assert!(watchdog.poll_all(t0 + Duration::from_millis(700)).is_empty());
//!
//! // Traffic resumes: `touch` reports the recovery.
//! let recovered = watchdog.touch(&"camera/frames", t0 + Duration::from_millis(900));
//! assert!(recovered, "this arrival ends a previously-escalated silence");
//! ```

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use astrs_wire::InputSpec;

use crate::sync_util::lock;

/// One registered key's mutable state.
struct Entry {
    timeout: Duration,
    last_seen: Instant,
    escalated: bool,
}

/// A point-in-time, non-mutating reading of one key's idle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleSnapshot {
    /// The configured silence budget.
    pub timeout: Duration,
    /// How long it has been since the last [`IdleWatchdog::touch`]
    /// (or since registration, if `touch` was never called), as measured
    /// from the `now` passed to [`IdleWatchdog::snapshot`].
    pub idle_for: Duration,
    /// Whether [`IdleWatchdog::poll_all`] has already reported this key's
    /// current silence (and no `touch` has arrived since).
    pub escalated: bool,
}

impl IdleSnapshot {
    /// Whether `idle_for` has reached `timeout`, regardless of whether a
    /// [`IdleWatchdog::poll_all`] sweep has already reported it.
    #[must_use]
    pub fn is_overdue(&self) -> bool {
        self.idle_for >= self.timeout
    }
}

/// One key's silence crossing its registered `timeout`, reported by
/// [`IdleWatchdog::poll_all`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleViolation<K> {
    /// The key that went silent.
    pub key: K,
    /// How long it had been silent at the moment this was reported —
    /// always `>= timeout`.
    pub idle_for: Duration,
    /// The budget it exceeded.
    pub timeout: Duration,
}

/// Registers `(key, timeout)` pairs and reports when a key goes silent for
/// longer than its budget (blueprint §8.3, §12).
///
/// `K` is left generic for the same reason as [`crate::DeadlineMonitor`]'s
/// key: a caller monitoring per-route liveness can key by
/// [`astrs_wire::PortRef`] or [`astrs_wire::NodeId`]; a test can key by a
/// bare `&str`.
pub struct IdleWatchdog<K> {
    entries: Mutex<HashMap<K, Entry>>,
}

impl<K> Default for IdleWatchdog<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K> IdleWatchdog<K> {
    /// Creates a watchdog with no registered keys.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl<K: Clone + Eq + Hash> IdleWatchdog<K> {
    /// Registers `key` with the given silence budget, seeding its "last
    /// seen" instant at `now` — so an input that never sends anything at
    /// all still goes overdue `timeout` after registration, not only after
    /// its first observed message.
    ///
    /// Idempotent, like [`crate::DeadlineMonitor::register`]: calling this
    /// again for an already-registered key updates its `timeout` in place
    /// (the manifest-reload case) while preserving its current `last_seen`
    /// and `escalated` state — a reconfigured timeout should not itself
    /// count as a fresh arrival.
    pub fn register(&self, key: K, timeout: Duration, now: Instant) {
        lock(&self.entries)
            .entry(key)
            .and_modify(|entry| entry.timeout = timeout)
            .or_insert(Entry {
                timeout,
                last_seen: now,
                escalated: false,
            });
    }

    /// [`IdleWatchdog::register`] using the real current instant.
    pub fn register_now(&self, key: K, timeout: Duration) {
        self.register(key, timeout, Instant::now());
    }

    /// Registers `key` from a manifest-resolved
    /// [`InputSpec::timeout`](astrs_wire::InputSpec::timeout).
    ///
    /// Returns whether a budget was actually registered: `false`,
    /// harmlessly, when `spec.timeout` is `None` — the common case for an
    /// input with no configured silence budget, where monitoring should
    /// simply stay off.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_scheduler::IdleWatchdog;
    /// use astrs_wire::{DataId, DurationMs, InputSpec, PortRef};
    /// use std::time::Instant;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let watchdog: IdleWatchdog<DataId> = IdleWatchdog::new();
    /// let now = Instant::now();
    ///
    /// let mut with_timeout = InputSpec::new(DataId::new("frames")?, PortRef::from_parts("camera", "image")?);
    /// with_timeout.timeout = Some(DurationMs::from_secs(5));
    /// assert!(watchdog.register_from_spec(with_timeout.id.clone(), &with_timeout, now));
    ///
    /// let without_timeout = InputSpec::new(DataId::new("logs")?, PortRef::from_parts("camera", "log")?);
    /// assert!(!watchdog.register_from_spec(without_timeout.id.clone(), &without_timeout, now));
    /// assert!(watchdog.snapshot(&without_timeout.id, now).is_none());
    /// # Ok(())
    /// # }
    /// ```
    pub fn register_from_spec(&self, key: K, spec: &InputSpec, now: Instant) -> bool {
        match spec.timeout {
            Some(timeout) => {
                self.register(key, timeout.to_duration(), now);
                true
            }
            None => false,
        }
    }

    /// [`IdleWatchdog::register_from_spec`] using the real current instant.
    pub fn register_from_spec_now(&self, key: K, spec: &InputSpec) -> bool {
        self.register_from_spec(key, spec, Instant::now())
    }

    /// Removes a key, returning whether it was registered.
    pub fn unregister(&self, key: &K) -> bool {
        lock(&self.entries).remove(key).is_some()
    }

    /// Records that a message just arrived for `key` at `now`.
    ///
    /// Returns whether this arrival is a **recovery**: `key` had
    /// previously been reported overdue by [`IdleWatchdog::poll_all`] and
    /// had not been touched since. A caller uses that to emit the matching
    /// `InputRecovered` exactly once, mirroring how `poll_all` reports
    /// `InputClosed` exactly once. Returns `false`, harmlessly, for an
    /// unregistered key.
    pub fn touch(&self, key: &K, now: Instant) -> bool {
        let mut entries = lock(&self.entries);
        let Some(entry) = entries.get_mut(key) else {
            return false;
        };
        let was_escalated = entry.escalated;
        entry.last_seen = now;
        entry.escalated = false;
        was_escalated
    }

    /// [`IdleWatchdog::touch`] using the real current instant.
    pub fn touch_now(&self, key: &K) -> bool {
        self.touch(key, Instant::now())
    }

    /// A non-mutating, point-in-time reading of `key`'s idle state, if it
    /// is registered.
    #[must_use]
    pub fn snapshot(&self, key: &K, now: Instant) -> Option<IdleSnapshot> {
        lock(&self.entries).get(key).map(|entry| IdleSnapshot {
            timeout: entry.timeout,
            idle_for: now.saturating_duration_since(entry.last_seen),
            escalated: entry.escalated,
        })
    }

    /// Sweeps every registered key, reporting exactly the ones whose
    /// silence just crossed their `timeout` since the last sweep or
    /// [`IdleWatchdog::touch`] — the edge-triggered check the module docs
    /// describe. Intended to be called once per health-check tick
    /// (blueprint §4.3's `health per-manifest` timer), the way
    /// [`crate::EventMux::snapshot_all`] is intended for a metrics tick.
    ///
    /// `O(registered keys)`: a full scan, same cost class as
    /// [`crate::TimerWheel::next_deadline`] and for the same reason — this
    /// is meant to run once per health-check period, not once per message.
    pub fn poll_all(&self, now: Instant) -> Vec<IdleViolation<K>> {
        let mut entries = lock(&self.entries);
        let mut violations = Vec::new();
        for (key, entry) in entries.iter_mut() {
            if entry.escalated {
                continue;
            }
            let idle_for = now.saturating_duration_since(entry.last_seen);
            if idle_for >= entry.timeout {
                entry.escalated = true;
                violations.push(IdleViolation {
                    key: key.clone(),
                    idle_for,
                    timeout: entry.timeout,
                });
            }
        }
        violations
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn unregistered_key_has_no_snapshot_and_touch_is_a_harmless_no_op() {
        let watchdog: IdleWatchdog<&str> = IdleWatchdog::new();
        assert!(watchdog.snapshot(&"unknown", Instant::now()).is_none());
        assert!(!watchdog.touch(&"unknown", Instant::now()));
        assert!(watchdog.poll_all(Instant::now()).is_empty());
    }

    #[test]
    fn fresh_traffic_never_escalates() {
        let watchdog: IdleWatchdog<&str> = IdleWatchdog::new();
        let t0 = Instant::now();
        watchdog.register("a", Duration::from_millis(100), t0);

        for step in 1..10u64 {
            let now = t0 + Duration::from_millis(step * 20);
            watchdog.touch(&"a", now);
            assert!(
                watchdog.poll_all(now).is_empty(),
                "step {step}: still within budget"
            );
        }
    }

    #[test]
    fn silence_past_the_timeout_is_reported_exactly_once() {
        let watchdog: IdleWatchdog<&str> = IdleWatchdog::new();
        let t0 = Instant::now();
        watchdog.register("a", Duration::from_millis(100), t0);

        assert!(watchdog.poll_all(t0 + Duration::from_millis(50)).is_empty());

        let violations = watchdog.poll_all(t0 + Duration::from_millis(150));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].key, "a");
        assert_eq!(violations[0].idle_for, Duration::from_millis(150));
        assert_eq!(violations[0].timeout, Duration::from_millis(100));

        // Still silent on the next sweep: not reported again.
        assert!(
            watchdog
                .poll_all(t0 + Duration::from_millis(500))
                .is_empty()
        );
        assert!(watchdog.poll_all(t0 + Duration::from_secs(60)).is_empty());
    }

    #[test]
    fn exactly_at_the_timeout_is_overdue() {
        let watchdog: IdleWatchdog<&str> = IdleWatchdog::new();
        let t0 = Instant::now();
        watchdog.register("a", Duration::from_millis(100), t0);
        let violations = watchdog.poll_all(t0 + Duration::from_millis(100));
        assert_eq!(
            violations.len(),
            1,
            "the boundary instant itself counts as overdue"
        );
    }

    #[test]
    fn touch_after_escalation_reports_a_recovery_and_resets_the_clock() {
        let watchdog: IdleWatchdog<&str> = IdleWatchdog::new();
        let t0 = Instant::now();
        watchdog.register("a", Duration::from_millis(100), t0);
        assert_eq!(watchdog.poll_all(t0 + Duration::from_millis(200)).len(), 1);

        let recovered = watchdog.touch(&"a", t0 + Duration::from_millis(250));
        assert!(recovered, "traffic resumed after an escalated silence");

        // The clock is reset: an immediate poll does not re-escalate.
        assert!(
            watchdog
                .poll_all(t0 + Duration::from_millis(260))
                .is_empty()
        );

        // A second touch, with nothing escalated in between, is not itself
        // reported as a recovery.
        assert!(!watchdog.touch(&"a", t0 + Duration::from_millis(270)));
    }

    #[test]
    fn touch_before_any_escalation_is_never_a_recovery() {
        let watchdog: IdleWatchdog<&str> = IdleWatchdog::new();
        let t0 = Instant::now();
        watchdog.register("a", Duration::from_millis(100), t0);
        assert!(!watchdog.touch(&"a", t0 + Duration::from_millis(10)));
        assert!(!watchdog.touch(&"a", t0 + Duration::from_millis(20)));
    }

    #[test]
    fn an_input_that_never_sends_anything_still_goes_overdue_from_registration() {
        let watchdog: IdleWatchdog<&str> = IdleWatchdog::new();
        let t0 = Instant::now();
        watchdog.register("a", Duration::from_millis(50), t0);
        // No `touch` ever happens; silence is measured from registration.
        let violations = watchdog.poll_all(t0 + Duration::from_millis(60));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].idle_for, Duration::from_millis(60));
    }

    #[test]
    fn re_registering_updates_timeout_but_preserves_last_seen_and_escalation() {
        let watchdog: IdleWatchdog<&str> = IdleWatchdog::new();
        let t0 = Instant::now();
        watchdog.register("a", Duration::from_millis(50), t0);
        assert_eq!(watchdog.poll_all(t0 + Duration::from_millis(60)).len(), 1);

        // Re-registering with a longer budget must not clear the
        // already-reported escalation or reset `last_seen` to `now`.
        let requeue_at = t0 + Duration::from_millis(70);
        watchdog.register("a", Duration::from_secs(10), requeue_at);
        let snap = watchdog.snapshot(&"a", requeue_at).unwrap();
        assert_eq!(snap.timeout, Duration::from_secs(10));
        assert!(snap.escalated, "escalation survives a budget-only update");
        assert_eq!(
            snap.idle_for,
            Duration::from_millis(70),
            "last_seen is unchanged, not reset to requeue_at"
        );
    }

    #[test]
    fn unregister_removes_the_key() {
        let watchdog: IdleWatchdog<&str> = IdleWatchdog::new();
        let t0 = Instant::now();
        watchdog.register("a", Duration::from_millis(50), t0);
        assert!(watchdog.unregister(&"a"));
        assert!(!watchdog.unregister(&"a"), "already removed");
        assert!(watchdog.snapshot(&"a", t0).is_none());
        assert!(watchdog.poll_all(t0 + Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn snapshot_is_read_only_and_never_escalates_on_its_own() {
        let watchdog: IdleWatchdog<&str> = IdleWatchdog::new();
        let t0 = Instant::now();
        watchdog.register("a", Duration::from_millis(50), t0);

        let past_budget = t0 + Duration::from_millis(100);
        let snap = watchdog.snapshot(&"a", past_budget).unwrap();
        assert!(snap.is_overdue());
        assert!(
            !snap.escalated,
            "a mere snapshot read must not itself flip the escalation flag"
        );

        // `poll_all` still reports it as a fresh escalation afterward --
        // proof the snapshot really did not mutate anything.
        assert_eq!(watchdog.poll_all(past_budget).len(), 1);
    }

    #[test]
    fn multiple_keys_are_tracked_independently() {
        let watchdog: IdleWatchdog<&str> = IdleWatchdog::new();
        let t0 = Instant::now();
        watchdog.register("hot", Duration::from_millis(1_000), t0);
        watchdog.register("cold", Duration::from_millis(50), t0);

        watchdog.touch(&"hot", t0 + Duration::from_millis(500));
        let violations = watchdog.poll_all(t0 + Duration::from_millis(600));

        assert_eq!(
            violations.len(),
            1,
            "only the untouched, short-budget input is overdue"
        );
        assert_eq!(violations[0].key, "cold");
    }

    fn spec_with_timeout(timeout: Option<Duration>) -> InputSpec {
        let mut spec = InputSpec::new(
            astrs_wire::DataId::new("frames").unwrap(),
            astrs_wire::PortRef::new(
                astrs_wire::NodeId::new("camera").unwrap(),
                astrs_wire::DataId::new("image").unwrap(),
            ),
        );
        spec.timeout = timeout.map(astrs_wire::DurationMs::from_duration);
        spec
    }

    #[test]
    fn register_from_spec_registers_the_declared_timeout() {
        let watchdog: IdleWatchdog<&str> = IdleWatchdog::new();
        let now = Instant::now();
        let spec = spec_with_timeout(Some(Duration::from_millis(250)));
        assert!(watchdog.register_from_spec("a", &spec, now));
        assert_eq!(
            watchdog.snapshot(&"a", now).unwrap().timeout,
            Duration::from_millis(250)
        );
    }

    #[test]
    fn register_from_spec_is_a_no_op_when_the_input_declares_no_timeout() {
        let watchdog: IdleWatchdog<&str> = IdleWatchdog::new();
        let now = Instant::now();
        let spec = spec_with_timeout(None);
        assert!(!watchdog.register_from_spec("a", &spec, now));
        assert!(
            watchdog.snapshot(&"a", now).is_none(),
            "no timeout means monitoring stays off, not a made-up default budget"
        );
    }

    #[test]
    fn now_and_register_now_variants_use_real_time_without_panicking() {
        let watchdog: IdleWatchdog<&str> = IdleWatchdog::new();
        watchdog.register_now("a", Duration::from_secs(1));
        assert!(!watchdog.touch_now(&"a"));
        assert!(watchdog.snapshot(&"a", Instant::now()).is_some());

        let spec = spec_with_timeout(Some(Duration::from_secs(1)));
        assert!(watchdog.register_from_spec_now("b", &spec));
    }
}
