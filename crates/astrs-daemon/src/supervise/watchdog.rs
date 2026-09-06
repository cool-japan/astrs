//! Deadlines and the finish-straggler watchdog (§12).
//!
//! > *Health model: post-registration liveness pings (`health_check_timeout`);
//! > a node hanging *before* subscribe is caught by a separate spawn-deadline
//! > (dora's gap, closed). Finish-straggler watchdog escalates SIGTERM→SIGKILL
//! > after `finish_grace_secs`.*
//!
//! Two pieces:
//!
//! - [`DeadlineTable`] — a keyed set of one-shot deadlines, each stamped with
//!   the generation it was armed for. That stamp is what makes it safe to fire
//!   a deadline into a system where the node underneath may have been replaced
//!   in the meantime: the expiry carries the generation, the caller compares
//!   it against the live [`crate::spawn::ProcessHandle`], and a mismatch is a
//!   no-op ([`crate::spawn::SignalOutcome::StaleGeneration`]).
//! - [`FinishWatchdogSet`] — the same idea with a two-step ladder:
//!   `SIGTERM` when the grace period runs out, then `SIGKILL` after a second,
//!   shorter grace. A node that ignores `SIGTERM` (one of the conformance
//!   zoo's misbehaviours) is a node that gets killed, on schedule, exactly
//!   once.
//!
//! ```text
//!   stop sent          finish_grace          kill_grace
//!   ────────►│◄───────────────────────►│◄──────────────►│
//!            arm                    SIGTERM          SIGKILL
//!                                  (step 1)          (step 2)
//!   node exits at any point ⇒ disarm, nothing fires
//! ```
//!
//! Neither type touches a process or a clock of its own: expiry is a function
//! of an [`Instant`] the caller supplies, so the daemon's merged event loop
//! stays the only thing that decides what time it is (and a `--deterministic`
//! replay stays reproducible, §14).
//!
//! # Examples
//!
//! ```
//! use std::time::{Duration, Instant};
//! use astrs_daemon::supervise::{EscalationStep, FinishWatchdogSet};
//! use astrs_wire::NodeId;
//!
//! let now = Instant::now();
//! let mut watchdogs = FinishWatchdogSet::new(Duration::from_secs(15), Duration::from_secs(5));
//! let node = NodeId::new("straggler")?;
//! watchdogs.arm(node.clone(), 3, now);
//!
//! assert!(watchdogs.expired(now + Duration::from_secs(14)).is_empty());
//!
//! let first = watchdogs.expired(now + Duration::from_secs(15));
//! assert_eq!(first[0].step, EscalationStep::Terminate);
//! assert_eq!(first[0].generation, 3);
//!
//! let second = watchdogs.expired(now + Duration::from_secs(20));
//! assert_eq!(second[0].step, EscalationStep::Kill);
//! assert!(watchdogs.is_empty(), "the ladder ends after the kill");
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use astrs_wire::NodeId;

/// One armed deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArmedDeadline {
    /// When it fires.
    pub at: Instant,
    /// The incarnation it was armed for (§12).
    pub generation: u64,
}

/// One expired deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expiry<K> {
    /// What it was armed against.
    pub key: K,
    /// The incarnation it was armed for.
    pub generation: u64,
    /// How late it is, at the moment it was collected.
    pub overdue: Duration,
}

/// A keyed set of one-shot, generation-stamped deadlines.
#[derive(Debug, Clone)]
pub struct DeadlineTable<K: Ord + Clone> {
    /// The armed deadlines.
    entries: BTreeMap<K, ArmedDeadline>,
}

impl<K: Ord + Clone> DeadlineTable<K> {
    /// An empty table.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Arms a deadline, replacing any deadline already armed for `key`.
    pub fn arm(&mut self, key: K, generation: u64, at: Instant) {
        self.entries.insert(key, ArmedDeadline { at, generation });
    }

    /// Arms a deadline `after` from `now`.
    pub fn arm_in(&mut self, key: K, generation: u64, now: Instant, after: Duration) {
        self.arm(key, generation, now + after);
    }

    /// Disarms `key`, whatever generation it was armed for.
    pub fn disarm(&mut self, key: &K) -> bool {
        self.entries.remove(key).is_some()
    }

    /// Disarms `key` only if it was armed for `generation`.
    ///
    /// The form a supervisor uses when a node exits: a deadline armed for the
    /// *new* incarnation must survive the old one's exit.
    pub fn disarm_generation(&mut self, key: &K, generation: u64) -> bool {
        match self.entries.get(key) {
            Some(entry) if entry.generation == generation => {
                self.entries.remove(key);
                true
            }
            _ => false,
        }
    }

    /// What is armed for `key`, if anything.
    #[must_use]
    pub fn get(&self, key: &K) -> Option<&ArmedDeadline> {
        self.entries.get(key)
    }

    /// Whether `key` has a deadline armed.
    #[must_use]
    pub fn is_armed(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// How many deadlines are armed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is armed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The earliest deadline, which is how long the event loop may sleep.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.entries.values().map(|entry| entry.at).min()
    }

    /// How long until the earliest deadline, at `now`.
    #[must_use]
    pub fn time_to_next(&self, now: Instant) -> Option<Duration> {
        self.next_deadline()
            .map(|at| at.saturating_duration_since(now))
    }

    /// Collects and removes everything that has expired at `now`.
    pub fn expired(&mut self, now: Instant) -> Vec<Expiry<K>> {
        let due: Vec<K> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.at <= now)
            .map(|(key, _)| key.clone())
            .collect();

        due.into_iter()
            .filter_map(|key| {
                let entry = self.entries.remove(&key)?;
                Some(Expiry {
                    key,
                    generation: entry.generation,
                    overdue: now.saturating_duration_since(entry.at),
                })
            })
            .collect()
    }

    /// Disarms everything.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

impl<K: Ord + Clone> Default for DeadlineTable<K> {
    fn default() -> Self {
        Self::new()
    }
}

/// A rung on the finish-straggler ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum EscalationStep {
    /// Send `SIGTERM` to the node's process group.
    Terminate,
    /// Send `SIGKILL` to the node's process group.
    Kill,
}

impl EscalationStep {
    /// The signal this step sends.
    #[must_use]
    pub const fn signal(self) -> rustix::process::Signal {
        match self {
            Self::Terminate => rustix::process::Signal::TERM,
            Self::Kill => rustix::process::Signal::KILL,
        }
    }

    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn kind_name(self) -> &'static str {
        match self {
            Self::Terminate => "terminate",
            Self::Kill => "kill",
        }
    }

    /// The exit intent this step implies, for the exit classifier.
    #[must_use]
    pub const fn intent(self) -> super::exit::ExitIntent {
        match self {
            Self::Terminate => super::exit::ExitIntent::TerminatedByWatchdog,
            Self::Kill => super::exit::ExitIntent::KilledByWatchdog,
        }
    }
}

impl core::fmt::Display for EscalationStep {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Terminate => "SIGTERM",
            Self::Kill => "SIGKILL",
        })
    }
}

/// One rung that came due.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Escalation {
    /// The node to signal.
    pub node: NodeId,
    /// The incarnation the watchdog was armed for.
    pub generation: u64,
    /// Which signal to send.
    pub step: EscalationStep,
}

/// One node's armed rung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rung {
    /// When it fires.
    at: Instant,
    /// The incarnation.
    generation: u64,
    /// Which signal.
    step: EscalationStep,
}

/// The finish-straggler watchdogs for a dataflow's nodes.
#[derive(Debug, Clone)]
pub struct FinishWatchdogSet {
    /// How long a node has after being told to stop, before `SIGTERM`.
    grace: Duration,
    /// How long it then has before `SIGKILL`.
    kill_grace: Duration,
    /// One rung per node — the ladder advances in place.
    rungs: BTreeMap<NodeId, Rung>,
}

impl FinishWatchdogSet {
    /// A set with the given grace periods.
    #[must_use]
    pub const fn new(grace: Duration, kill_grace: Duration) -> Self {
        Self {
            grace,
            kill_grace,
            rungs: BTreeMap::new(),
        }
    }

    /// The grace period before `SIGTERM`.
    #[must_use]
    pub const fn grace(&self) -> Duration {
        self.grace
    }

    /// The grace period between `SIGTERM` and `SIGKILL`.
    #[must_use]
    pub const fn kill_grace(&self) -> Duration {
        self.kill_grace
    }

    /// Arms the ladder for `node` at `now`, using the set's default grace.
    pub fn arm(&mut self, node: NodeId, generation: u64, now: Instant) {
        self.arm_with_grace(node, generation, now, self.grace);
    }

    /// Arms the ladder with a node-specific grace period
    /// ([`astrs_wire::NodeSpawnSpec::finish_grace`]).
    pub fn arm_with_grace(&mut self, node: NodeId, generation: u64, now: Instant, grace: Duration) {
        self.rungs.insert(
            node,
            Rung {
                at: now + grace,
                generation,
                step: EscalationStep::Terminate,
            },
        );
    }

    /// Disarms `node` — it exited, so nothing needs signalling.
    pub fn disarm(&mut self, node: &NodeId) -> bool {
        self.rungs.remove(node).is_some()
    }

    /// Disarms `node` only if the armed rung belongs to `generation`.
    pub fn disarm_generation(&mut self, node: &NodeId, generation: u64) -> bool {
        match self.rungs.get(node) {
            Some(rung) if rung.generation == generation => {
                self.rungs.remove(node);
                true
            }
            _ => false,
        }
    }

    /// Whether a ladder is armed for `node`.
    #[must_use]
    pub fn is_armed(&self, node: &NodeId) -> bool {
        self.rungs.contains_key(node)
    }

    /// How many ladders are armed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rungs.len()
    }

    /// Whether nothing is armed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rungs.is_empty()
    }

    /// The earliest rung, which is how long the event loop may sleep.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.rungs.values().map(|rung| rung.at).min()
    }

    /// Collects the rungs due at `now`, advancing each ladder.
    ///
    /// A `Terminate` that fires re-arms as a `Kill` after `kill_grace`; a
    /// `Kill` that fires ends the ladder — there is nothing above `SIGKILL`,
    /// and a process that survives it is a kernel problem, not a daemon one.
    pub fn expired(&mut self, now: Instant) -> Vec<Escalation> {
        let due: Vec<(NodeId, Rung)> = self
            .rungs
            .iter()
            .filter(|(_, rung)| rung.at <= now)
            .map(|(node, rung)| (node.clone(), *rung))
            .collect();

        let mut escalations = Vec::with_capacity(due.len());
        for (node, rung) in due {
            match rung.step {
                EscalationStep::Terminate => {
                    self.rungs.insert(
                        node.clone(),
                        Rung {
                            at: now + self.kill_grace,
                            generation: rung.generation,
                            step: EscalationStep::Kill,
                        },
                    );
                }
                EscalationStep::Kill => {
                    self.rungs.remove(&node);
                }
            }
            escalations.push(Escalation {
                node,
                generation: rung.generation,
                step: rung.step,
            });
        }
        escalations
    }

    /// Disarms everything.
    pub fn clear(&mut self) {
        self.rungs.clear();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn node(name: &str) -> NodeId {
        NodeId::new(name).unwrap()
    }

    #[test]
    fn an_empty_table_has_nothing_to_do() {
        let mut table: DeadlineTable<NodeId> = DeadlineTable::new();
        let now = Instant::now();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert!(table.next_deadline().is_none());
        assert!(table.time_to_next(now).is_none());
        assert!(table.expired(now).is_empty());
    }

    #[test]
    fn a_deadline_fires_once_at_its_instant() {
        let now = Instant::now();
        let mut table = DeadlineTable::new();
        table.arm_in(node("a"), 2, now, Duration::from_secs(5));

        assert!(table.is_armed(&node("a")));
        assert_eq!(
            table.time_to_next(now + Duration::from_secs(1)),
            Some(Duration::from_secs(4))
        );
        assert!(table.expired(now + Duration::from_secs(4)).is_empty());

        let fired = table.expired(now + Duration::from_secs(6));
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].key, node("a"));
        assert_eq!(fired[0].generation, 2);
        assert_eq!(fired[0].overdue, Duration::from_secs(1));
        assert!(table.is_empty(), "one-shot");
    }

    #[test]
    fn arming_again_replaces_the_previous_deadline() {
        let now = Instant::now();
        let mut table = DeadlineTable::new();
        table.arm_in(node("a"), 1, now, Duration::from_secs(5));
        table.arm_in(node("a"), 2, now, Duration::from_secs(50));
        assert_eq!(table.len(), 1);
        assert!(table.expired(now + Duration::from_secs(6)).is_empty());
        assert_eq!(table.get(&node("a")).map(|entry| entry.generation), Some(2));
    }

    #[test]
    fn a_generation_guarded_disarm_leaves_a_newer_deadline_alone() {
        let now = Instant::now();
        let mut table = DeadlineTable::new();
        table.arm_in(node("a"), 5, now, Duration::from_secs(5));

        assert!(
            !table.disarm_generation(&node("a"), 4),
            "generation 4 exiting must not disarm generation 5"
        );
        assert!(table.is_armed(&node("a")));
        assert!(table.disarm_generation(&node("a"), 5));
        assert!(table.is_empty());
    }

    #[test]
    fn an_unguarded_disarm_removes_whatever_is_armed() {
        let now = Instant::now();
        let mut table = DeadlineTable::new();
        table.arm_in(node("a"), 5, now, Duration::from_secs(5));
        assert!(table.disarm(&node("a")));
        assert!(!table.disarm(&node("a")));
    }

    #[test]
    fn the_earliest_deadline_is_the_one_to_sleep_until() {
        let now = Instant::now();
        let mut table = DeadlineTable::new();
        table.arm_in(node("late"), 0, now, Duration::from_secs(50));
        table.arm_in(node("soon"), 0, now, Duration::from_secs(5));
        assert_eq!(table.next_deadline(), Some(now + Duration::from_secs(5)));
    }

    #[test]
    fn several_deadlines_can_fire_together_in_key_order() {
        let now = Instant::now();
        let mut table = DeadlineTable::new();
        table.arm_in(node("b"), 0, now, Duration::from_secs(1));
        table.arm_in(node("a"), 0, now, Duration::from_secs(1));
        let fired = table.expired(now + Duration::from_secs(1));
        let keys: Vec<&str> = fired.iter().map(|expiry| expiry.key.as_str()).collect();
        assert_eq!(keys, ["a", "b"]);
    }

    #[test]
    fn clearing_disarms_everything() {
        let now = Instant::now();
        let mut table = DeadlineTable::new();
        table.arm_in(node("a"), 0, now, Duration::from_secs(1));
        table.clear();
        assert!(table.is_empty());
    }

    #[test]
    fn the_ladder_escalates_term_then_kill() {
        let now = Instant::now();
        let mut set = FinishWatchdogSet::new(Duration::from_secs(15), Duration::from_secs(5));
        set.arm(node("straggler"), 3, now);
        assert_eq!(set.grace(), Duration::from_secs(15));
        assert_eq!(set.kill_grace(), Duration::from_secs(5));
        assert_eq!(set.len(), 1);

        assert!(set.expired(now + Duration::from_secs(14)).is_empty());

        let first = set.expired(now + Duration::from_secs(15));
        assert_eq!(
            first,
            [Escalation {
                node: node("straggler"),
                generation: 3,
                step: EscalationStep::Terminate,
            }]
        );
        assert!(set.is_armed(&node("straggler")), "the kill rung is armed");

        assert!(set.expired(now + Duration::from_secs(19)).is_empty());
        let second = set.expired(now + Duration::from_secs(20));
        assert_eq!(second[0].step, EscalationStep::Kill);
        assert!(set.is_empty(), "nothing above SIGKILL");
    }

    #[test]
    fn a_node_that_exits_disarms_its_ladder() {
        let now = Instant::now();
        let mut set = FinishWatchdogSet::new(Duration::from_secs(1), Duration::from_secs(1));
        set.arm(node("polite"), 1, now);
        assert!(set.disarm(&node("polite")));
        assert!(set.expired(now + Duration::from_secs(10)).is_empty());
    }

    #[test]
    fn a_restart_underneath_a_ladder_does_not_disarm_it() {
        let now = Instant::now();
        let mut set = FinishWatchdogSet::new(Duration::from_secs(1), Duration::from_secs(1));
        set.arm(node("straggler"), 7, now);

        assert!(
            !set.disarm_generation(&node("straggler"), 6),
            "an older incarnation exiting is not this ladder's business"
        );
        let fired = set.expired(now + Duration::from_secs(1));
        assert_eq!(fired[0].generation, 7, "the signal is addressed to 7");
    }

    #[test]
    fn a_node_specific_grace_overrides_the_default() {
        let now = Instant::now();
        let mut set = FinishWatchdogSet::new(Duration::from_secs(60), Duration::from_secs(5));
        set.arm_with_grace(node("quick"), 1, now, Duration::from_secs(1));
        assert_eq!(set.next_deadline(), Some(now + Duration::from_secs(1)));
        assert_eq!(set.expired(now + Duration::from_secs(1)).len(), 1);
    }

    #[test]
    fn escalation_steps_map_to_signals_and_intents() {
        assert_eq!(EscalationStep::Terminate.to_string(), "SIGTERM");
        assert_eq!(EscalationStep::Kill.to_string(), "SIGKILL");
        assert_eq!(EscalationStep::Terminate.kind_name(), "terminate");
        assert_eq!(EscalationStep::Kill.kind_name(), "kill");
        assert_eq!(
            EscalationStep::Terminate.signal(),
            rustix::process::Signal::TERM
        );
        assert_eq!(EscalationStep::Kill.signal(), rustix::process::Signal::KILL);
        assert_eq!(
            EscalationStep::Kill.intent(),
            super::super::exit::ExitIntent::KilledByWatchdog
        );
    }

    #[test]
    fn clearing_a_watchdog_set_disarms_everything() {
        let now = Instant::now();
        let mut set = FinishWatchdogSet::new(Duration::from_secs(1), Duration::from_secs(1));
        set.arm(node("a"), 1, now);
        set.arm(node("b"), 1, now);
        assert_eq!(set.len(), 2);
        set.clear();
        assert!(set.is_empty());
        assert!(set.next_deadline().is_none());
    }
}
