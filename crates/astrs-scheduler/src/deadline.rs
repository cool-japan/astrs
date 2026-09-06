//! [`DeadlineMonitor`] — per-input latency budgets (blueprint §11.3).
//!
//! A node's `deadline_ms` (carried on [`astrs_wire::InputSpec::deadline`])
//! is a promise: from the moment a specific input is received to the
//! moment the corresponding output is produced, no more than this long
//! should elapse. `DeadlineMonitor` measures that promise being kept or
//! broken, without deciding what to do about it — same as the rest of this
//! crate, that decision (log it, raise an `astrs/status` event, feed a
//! histogram) belongs to the caller.
//!
//! # Token-based, not slot-based
//!
//! A naive design keyed purely by `HashMap<K, Instant>` ("the start time
//! for key K") breaks the moment a node pipelines — a second input arriving
//! for the same key before the first one's output is produced would
//! clobber the first measurement's start time. Instead,
//! [`DeadlineMonitor::start`] hands back an owned [`DeadlineToken`] that
//! carries its own key, start instant, and budget; [`DeadlineMonitor::finish`]
//! needs nothing from the registry to compute the outcome — arbitrarily many
//! measurements for the same key can be in flight at once, each independent,
//! and the hot path is a token comparison, not a lookup.
//!
//! # Examples
//!
//! ```
//! use astrs_scheduler::{DeadlineMonitor, DeadlineOutcome};
//! use std::time::{Duration, Instant};
//!
//! let monitor: DeadlineMonitor<&str> = DeadlineMonitor::new();
//! monitor.register("camera->detections", Duration::from_millis(50));
//!
//! let t0 = Instant::now();
//! let token = monitor.start(&"camera->detections", t0).expect("registered");
//! let outcome = monitor.finish(token, t0 + Duration::from_millis(10));
//! assert!(!outcome.is_violated());
//!
//! let token = monitor.start(&"camera->detections", t0).expect("registered");
//! let outcome = monitor.finish(token, t0 + Duration::from_millis(80));
//! assert!(outcome.is_violated());
//! ```

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use astrs_wire::InputSpec;

use crate::sync_util::lock;

/// Per-key running counters, updated by [`DeadlineMonitor::finish`].
struct Entry {
    budget: Duration,
    samples: u64,
    violations: u64,
    last_latency: Duration,
    max_latency: Duration,
}

impl Entry {
    const fn new(budget: Duration) -> Self {
        Self {
            budget,
            samples: 0,
            violations: 0,
            last_latency: Duration::ZERO,
            max_latency: Duration::ZERO,
        }
    }

    fn observe(&mut self, latency: Duration, violated: bool) {
        self.samples += 1;
        if violated {
            self.violations += 1;
        }
        self.last_latency = latency;
        self.max_latency = self.max_latency.max(latency);
    }
}

/// A point-in-time reading of one key's deadline counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadlineSnapshot {
    /// The currently registered budget for this key.
    pub budget: Duration,
    /// Total measurements completed via [`DeadlineMonitor::finish`].
    pub samples: u64,
    /// Of `samples`, how many exceeded `budget`.
    pub violations: u64,
    /// The most recently measured latency.
    pub last_latency: Duration,
    /// The largest latency measured so far.
    pub max_latency: Duration,
}

/// A self-contained handle to one in-flight latency measurement.
///
/// Returned by [`DeadlineMonitor::start`], consumed by
/// [`DeadlineMonitor::finish`]. Carries everything needed to compute the
/// outcome on its own — the registry can be mutated, or the key
/// unregistered entirely, between `start` and `finish` without affecting an
/// already-issued token.
#[derive(Debug, Clone)]
pub struct DeadlineToken<K> {
    key: K,
    started_at: Instant,
    budget: Duration,
}

impl<K> DeadlineToken<K> {
    /// The key this measurement is for.
    #[must_use]
    pub const fn key(&self) -> &K {
        &self.key
    }

    /// The instant this measurement started.
    #[must_use]
    pub const fn started_at(&self) -> Instant {
        self.started_at
    }

    /// The budget this measurement is held to.
    #[must_use]
    pub const fn budget(&self) -> Duration {
        self.budget
    }
}

/// The result of one completed measurement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeadlineOutcome<K> {
    /// The measured latency was within budget.
    Met {
        /// The key this measurement was for.
        key: K,
        /// The measured input-to-output latency.
        latency: Duration,
        /// The budget it was held to.
        budget: Duration,
    },
    /// The measured latency exceeded the budget.
    Violated {
        /// The key this measurement was for.
        key: K,
        /// The measured input-to-output latency.
        latency: Duration,
        /// The budget it was held to.
        budget: Duration,
        /// How far over budget the measurement landed
        /// (`latency - budget`).
        over_by: Duration,
    },
}

impl<K> DeadlineOutcome<K> {
    /// The key this measurement was for.
    #[must_use]
    pub const fn key(&self) -> &K {
        match self {
            Self::Met { key, .. } | Self::Violated { key, .. } => key,
        }
    }

    /// The measured latency, regardless of outcome.
    #[must_use]
    pub const fn latency(&self) -> Duration {
        match self {
            Self::Met { latency, .. } | Self::Violated { latency, .. } => *latency,
        }
    }

    /// The budget the measurement was held to.
    #[must_use]
    pub const fn budget(&self) -> Duration {
        match self {
            Self::Met { budget, .. } | Self::Violated { budget, .. } => *budget,
        }
    }

    /// Whether this measurement exceeded its budget.
    #[must_use]
    pub const fn is_violated(&self) -> bool {
        matches!(self, Self::Violated { .. })
    }
}

/// Registers `(key, deadline_ms)` pairs and measures whether the
/// input-to-output latency for each stays within budget (blueprint §11.3).
///
/// `K` is left generic on purpose: a caller monitoring one budget per node
/// can key by [`astrs_wire::NodeId`]; one monitoring per input port can key
/// by [`astrs_wire::PortRef`] (which [`astrs_wire::InputSpec::deadline`]
/// suggests, since the field lives on the per-input spec); a test can key
/// by a bare `&str`.
pub struct DeadlineMonitor<K> {
    entries: Mutex<HashMap<K, Entry>>,
}

impl<K> Default for DeadlineMonitor<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K> DeadlineMonitor<K> {
    /// Creates a monitor with no registered keys.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl<K: Clone + Eq + Hash> DeadlineMonitor<K> {
    /// Registers `key` with the given budget.
    ///
    /// Idempotent by design, unlike
    /// [`EventMux::register_input`](crate::EventMux::register_input):
    /// calling this again for an already-registered key updates its budget
    /// in place (the manifest-reload case — a `deadline_ms` value changing
    /// at runtime) while preserving its accumulated counters, rather than
    /// erroring.
    pub fn register(&self, key: K, budget: Duration) {
        lock(&self.entries)
            .entry(key)
            .and_modify(|entry| entry.budget = budget)
            .or_insert_with(|| Entry::new(budget));
    }

    /// Registers `key` from a manifest-resolved
    /// [`InputSpec::deadline`](astrs_wire::InputSpec::deadline), the
    /// per-input `deadline_ms` of blueprint §11.3.
    ///
    /// Returns whether a budget was actually registered: `false`,
    /// harmlessly, when `spec.deadline` is `None` — the common case for an
    /// input with no configured latency budget, where monitoring should
    /// simply stay off rather than be registered with some made-up default.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_scheduler::DeadlineMonitor;
    /// use astrs_wire::{DataId, DurationMs, InputSpec, PortRef};
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let monitor: DeadlineMonitor<DataId> = DeadlineMonitor::new();
    ///
    /// let mut with_deadline = InputSpec::new(DataId::new("frames")?, PortRef::from_parts("camera", "image")?);
    /// with_deadline.deadline = Some(DurationMs::from_secs(1));
    /// assert!(monitor.register_from_spec(with_deadline.id.clone(), &with_deadline));
    ///
    /// let without_deadline = InputSpec::new(DataId::new("logs")?, PortRef::from_parts("camera", "log")?);
    /// assert!(!monitor.register_from_spec(without_deadline.id.clone(), &without_deadline));
    /// assert!(monitor.snapshot(&without_deadline.id).is_none());
    /// # Ok(())
    /// # }
    /// ```
    pub fn register_from_spec(&self, key: K, spec: &InputSpec) -> bool {
        match spec.deadline {
            Some(deadline) => {
                self.register(key, deadline.to_duration());
                true
            }
            None => false,
        }
    }

    /// Removes a key, returning whether it was registered.
    ///
    /// Tokens already issued for `key` remain valid — see the module docs
    /// on why tokens are self-contained.
    pub fn unregister(&self, key: &K) -> bool {
        lock(&self.entries).remove(key).is_some()
    }

    /// Starts a measurement for `key` at `now`.
    ///
    /// Returns `None` if `key` is not registered — monitoring is simply off
    /// for that pair, which is the common case for an input with no
    /// configured `deadline_ms`.
    #[must_use]
    pub fn start(&self, key: &K, now: Instant) -> Option<DeadlineToken<K>> {
        let budget = lock(&self.entries).get(key)?.budget;
        Some(DeadlineToken {
            key: key.clone(),
            started_at: now,
            budget,
        })
    }

    /// [`DeadlineMonitor::start`] using the real current instant.
    #[must_use]
    pub fn start_now(&self, key: &K) -> Option<DeadlineToken<K>> {
        self.start(key, Instant::now())
    }

    /// Completes a measurement, comparing the elapsed time against the
    /// token's budget and updating that key's running counters.
    ///
    /// O(1): a duration comparison plus one hash-map lookup to update
    /// counters (skipped, harmlessly, if `key` was unregistered since
    /// `start` — the returned outcome is unaffected either way, since it is
    /// computed entirely from the token).
    ///
    /// `now` before the token's start instant is treated as zero elapsed
    /// time rather than underflowing or panicking — it should not happen in
    /// practice (both instants should come from the same monotonic clock),
    /// but a caller-supplied `now` is not this crate's invariant to enforce
    /// by panicking.
    pub fn finish(&self, token: DeadlineToken<K>, now: Instant) -> DeadlineOutcome<K> {
        let latency = now
            .checked_duration_since(token.started_at)
            .unwrap_or(Duration::ZERO);
        let violated = latency > token.budget;

        if let Some(entry) = lock(&self.entries).get_mut(&token.key) {
            entry.observe(latency, violated);
        }

        if violated {
            DeadlineOutcome::Violated {
                key: token.key,
                latency,
                budget: token.budget,
                over_by: latency - token.budget,
            }
        } else {
            DeadlineOutcome::Met {
                key: token.key,
                latency,
                budget: token.budget,
            }
        }
    }

    /// [`DeadlineMonitor::finish`] using the real current instant.
    pub fn finish_now(&self, token: DeadlineToken<K>) -> DeadlineOutcome<K> {
        self.finish(token, Instant::now())
    }

    /// A point-in-time reading of `key`'s counters, if it is registered.
    #[must_use]
    pub fn snapshot(&self, key: &K) -> Option<DeadlineSnapshot> {
        lock(&self.entries).get(key).map(|entry| DeadlineSnapshot {
            budget: entry.budget,
            samples: entry.samples,
            violations: entry.violations,
            last_latency: entry.last_latency,
            max_latency: entry.max_latency,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn unregistered_key_cannot_start_a_measurement() {
        let monitor: DeadlineMonitor<&str> = DeadlineMonitor::new();
        assert!(monitor.start(&"unknown", Instant::now()).is_none());
    }

    #[test]
    fn within_budget_is_met() {
        let monitor: DeadlineMonitor<&str> = DeadlineMonitor::new();
        monitor.register("a", Duration::from_millis(50));
        let t0 = Instant::now();
        let token = monitor.start(&"a", t0).unwrap();
        let outcome = monitor.finish(token, t0 + Duration::from_millis(20));
        assert!(!outcome.is_violated());
        assert_eq!(outcome.latency(), Duration::from_millis(20));
        assert_eq!(outcome.budget(), Duration::from_millis(50));
        assert_eq!(*outcome.key(), "a");
    }

    #[test]
    fn exactly_on_budget_is_not_a_violation() {
        let monitor: DeadlineMonitor<&str> = DeadlineMonitor::new();
        monitor.register("a", Duration::from_millis(50));
        let t0 = Instant::now();
        let token = monitor.start(&"a", t0).unwrap();
        let outcome = monitor.finish(token, t0 + Duration::from_millis(50));
        assert!(!outcome.is_violated(), "exactly the budget is still met");
    }

    #[test]
    fn over_budget_is_violated_with_the_correct_overage() {
        let monitor: DeadlineMonitor<&str> = DeadlineMonitor::new();
        monitor.register("a", Duration::from_millis(50));
        let t0 = Instant::now();
        let token = monitor.start(&"a", t0).unwrap();
        let outcome = monitor.finish(token, t0 + Duration::from_millis(80));
        match outcome {
            DeadlineOutcome::Violated {
                over_by,
                latency,
                budget,
                ..
            } => {
                assert_eq!(latency, Duration::from_millis(80));
                assert_eq!(budget, Duration::from_millis(50));
                assert_eq!(over_by, Duration::from_millis(30));
            }
            DeadlineOutcome::Met { .. } => panic!("expected a violation"),
        }
    }

    #[test]
    fn re_registering_updates_budget_but_keeps_counters() {
        let monitor: DeadlineMonitor<&str> = DeadlineMonitor::new();
        monitor.register("a", Duration::from_millis(10));
        let t0 = Instant::now();
        let token = monitor.start(&"a", t0).unwrap();
        let _ = monitor.finish(token, t0 + Duration::from_millis(50));
        assert_eq!(monitor.snapshot(&"a").unwrap().violations, 1);

        monitor.register("a", Duration::from_millis(100));
        let snap = monitor.snapshot(&"a").unwrap();
        assert_eq!(snap.budget, Duration::from_millis(100));
        assert_eq!(snap.violations, 1, "counters survive a budget update");
    }

    #[test]
    fn unregister_removes_the_key_but_not_an_already_issued_token() {
        let monitor: DeadlineMonitor<&str> = DeadlineMonitor::new();
        monitor.register("a", Duration::from_millis(50));
        let t0 = Instant::now();
        let token = monitor.start(&"a", t0).unwrap();

        assert!(monitor.unregister(&"a"));
        assert!(!monitor.unregister(&"a"), "already removed");
        assert!(monitor.start(&"a", t0).is_none(), "no longer registered");

        // The token issued before unregistration still resolves correctly.
        let outcome = monitor.finish(token, t0 + Duration::from_millis(10));
        assert!(!outcome.is_violated());
        assert!(monitor.snapshot(&"a").is_none());
    }

    #[test]
    fn concurrent_in_flight_measurements_for_the_same_key_do_not_interfere() {
        let monitor: DeadlineMonitor<&str> = DeadlineMonitor::new();
        monitor.register("a", Duration::from_millis(50));
        let t0 = Instant::now();

        // Two overlapping ("pipelined") measurements for the same key.
        let early = monitor.start(&"a", t0).unwrap();
        let late = monitor.start(&"a", t0 + Duration::from_millis(5)).unwrap();

        let early_outcome = monitor.finish(early, t0 + Duration::from_millis(80));
        let late_outcome = monitor.finish(late, t0 + Duration::from_millis(20));

        assert!(early_outcome.is_violated(), "started first, ran long");
        assert!(
            !late_outcome.is_violated(),
            "started later, finished promptly"
        );
        assert_eq!(monitor.snapshot(&"a").unwrap().samples, 2);
    }

    #[test]
    fn snapshot_tracks_max_and_last_latency() {
        let monitor: DeadlineMonitor<&str> = DeadlineMonitor::new();
        monitor.register("a", Duration::from_secs(1));
        let t0 = Instant::now();

        let token = monitor.start(&"a", t0).unwrap();
        let _ = monitor.finish(token, t0 + Duration::from_millis(5));
        let token = monitor.start(&"a", t0).unwrap();
        let _ = monitor.finish(token, t0 + Duration::from_millis(50));
        let token = monitor.start(&"a", t0).unwrap();
        let _ = monitor.finish(token, t0 + Duration::from_millis(2));

        let snap = monitor.snapshot(&"a").unwrap();
        assert_eq!(snap.samples, 3);
        assert_eq!(snap.violations, 0);
        assert_eq!(snap.max_latency, Duration::from_millis(50));
        assert_eq!(snap.last_latency, Duration::from_millis(2));
    }

    #[test]
    fn now_before_start_does_not_panic_and_reads_as_zero_latency() {
        let monitor: DeadlineMonitor<&str> = DeadlineMonitor::new();
        monitor.register("a", Duration::from_millis(10));
        let t0 = Instant::now() + Duration::from_millis(100);
        let token = monitor.start(&"a", t0).unwrap();
        let outcome = monitor.finish(token, t0 - Duration::from_millis(50));
        assert!(!outcome.is_violated());
        assert_eq!(outcome.latency(), Duration::ZERO);
    }

    fn spec_with_deadline(deadline: Option<Duration>) -> InputSpec {
        let mut spec = InputSpec::new(
            astrs_wire::DataId::new("frames").unwrap(),
            astrs_wire::PortRef::new(
                astrs_wire::NodeId::new("camera").unwrap(),
                astrs_wire::DataId::new("image").unwrap(),
            ),
        );
        spec.deadline = deadline.map(astrs_wire::DurationMs::from_duration);
        spec
    }

    #[test]
    fn register_from_spec_registers_the_declared_deadline() {
        let monitor: DeadlineMonitor<&str> = DeadlineMonitor::new();
        let spec = spec_with_deadline(Some(Duration::from_millis(50)));
        assert!(monitor.register_from_spec("a", &spec));
        assert_eq!(
            monitor.snapshot(&"a").unwrap().budget,
            Duration::from_millis(50)
        );
    }

    #[test]
    fn register_from_spec_is_a_no_op_when_the_input_declares_no_deadline() {
        let monitor: DeadlineMonitor<&str> = DeadlineMonitor::new();
        let spec = spec_with_deadline(None);
        assert!(!monitor.register_from_spec("a", &spec));
        assert!(
            monitor.snapshot(&"a").is_none(),
            "no deadline_ms means monitoring stays off, not a made-up default budget"
        );
    }

    #[test]
    fn register_from_spec_can_start_and_finish_a_measurement_end_to_end() {
        let monitor: DeadlineMonitor<&str> = DeadlineMonitor::new();
        let spec = spec_with_deadline(Some(Duration::from_millis(50)));
        assert!(monitor.register_from_spec("camera->detections", &spec));

        let t0 = Instant::now();
        let token = monitor.start(&"camera->detections", t0).unwrap();
        let outcome = monitor.finish(token, t0 + Duration::from_millis(80));
        assert!(outcome.is_violated());
        match outcome {
            DeadlineOutcome::Violated { over_by, .. } => {
                assert_eq!(over_by, Duration::from_millis(30));
            }
            DeadlineOutcome::Met { .. } => panic!("expected a violation"),
        }
    }
}
