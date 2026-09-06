//! [`HealthTable`] — post-registration liveness (§12).
//!
//! > *Health model: post-registration liveness pings (`health_check_timeout`);
//! > a node hanging **before** subscribe is caught by a separate
//! > spawn-deadline (dora's gap, closed).*
//!
//! The spawn deadline ([`crate::supervise::DeadlineTable`]) covers the window
//! from `fork` to `Register`. This table covers everything after it: a node
//! that registered, was accepted, and then stopped making progress — the
//! deadlock, the infinite loop, the blocked syscall. Its process is alive, its
//! socket is open, and nothing in the operating system will ever tell the
//! daemon that it has stopped working. Only a deadline can.
//!
//! # What counts as alive
//!
//! There is no daemon→node ping frame, and there deliberately never will be:
//! [`astrs_wire::NodeEvent`]'s variant indices are frozen at 0–14 by the §7.2
//! append-only rule and the protocol snapshot test, and a liveness probe that
//! needs a *new* message is a probe that cannot be added without a protocol
//! bump. So liveness is inferred from the traffic the node already generates,
//! which is strictly better: a reply to a ping proves only that the node's
//! socket reader runs, while the requests below prove that its *event loop*
//! runs.
//!
//! A node is live when either of these holds:
//!
//! | Signal | Meaning |
//! |---|---|
//! | Any [`astrs_wire::NodeRequest`] within its timeout | It ran round its loop |
//! | It is **parked** in [`astrs_wire::NodeRequest::NextEvent`] | It reached the top of its loop and is waiting for the daemon |
//!
//! The second is what keeps this table from murdering healthy consumers. A
//! node subscribed only to a 0.1 Hz sensor is silent for ten seconds at a time
//! and is *supposed* to be: it asked for the next event and the daemon has not
//! got one yet. Failing it for the daemon's own idleness would be a bug
//! disguised as a health check, so a parked node has no deadline at all —
//! [`HealthTable::park`] clears it, [`HealthTable::unpark`] restarts it.
//!
//! # Opting in
//!
//! A node is monitored only when its specification carries a
//! `health_check_timeout` (§8.3), or when the daemon was configured with a
//! default one. That is deliberate: a node that legitimately takes twenty
//! seconds to process one message — an expensive planner, a model load — is
//! healthy, and the manifest is the only place that knows so. The blueprint's
//! 5 s (§24.2) is the *checking* cadence, not a universal deadline.
//!
//! # Generations
//!
//! Every entry is stamped with the incarnation that armed it, exactly like
//! [`crate::supervise::DeadlineTable`]: a deadline armed for generation 3 that
//! fires after the node restarted as generation 4 is discarded rather than
//! killing the healthy replacement (§12).
//!
//! # Examples
//!
//! ```
//! use std::time::{Duration, Instant};
//! use astrs_daemon::health::{HealthTable, LivenessState};
//! use astrs_wire::{DataflowId, NodeId};
//!
//! let dataflow = DataflowId::from_u128(1);
//! let node = NodeId::new("planner")?;
//! let start = Instant::now();
//!
//! let mut health = HealthTable::new();
//! health.arm(dataflow, node.clone(), 0, Some(Duration::from_secs(5)), start);
//! assert_eq!(health.state(dataflow, &node, start), LivenessState::Active);
//!
//! // Silent past its deadline: overdue, and reported once.
//! let late = start + Duration::from_secs(6);
//! assert_eq!(health.state(dataflow, &node, late), LivenessState::Overdue);
//! assert_eq!(health.expired(late).len(), 1);
//! assert!(health.expired(late).is_empty(), "reported exactly once");
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use astrs_wire::{DataflowId, DurationMs, NodeExitCause, NodeId};

/// The key one monitored node is filed under.
pub type NodeKey = (DataflowId, NodeId);

/// What the table believes about one node right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LivenessState {
    /// The daemon is not monitoring this node.
    Unmonitored,
    /// It has spoken within its timeout.
    Active,
    /// It is waiting on the daemon for its next event, which is progress.
    Parked,
    /// It has been silent longer than its timeout.
    Overdue,
    /// It has already been reported overdue; the failure is in flight.
    Reported,
}

impl LivenessState {
    /// A stable, lower-case name for logs and metric labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unmonitored => "unmonitored",
            Self::Active => "active",
            Self::Parked => "parked",
            Self::Overdue => "overdue",
            Self::Reported => "reported",
        }
    }

    /// Whether this state means the node is making progress.
    #[must_use]
    pub const fn is_healthy(self) -> bool {
        matches!(self, Self::Active | Self::Parked)
    }
}

/// One node the daemon is watching.
#[derive(Debug, Clone)]
struct HealthEntry {
    /// The incarnation this entry belongs to (§12).
    generation: u64,
    /// How long the node may be silent and unparked.
    timeout: Duration,
    /// When it last proved it was running.
    last_seen: Instant,
    /// Whether it is waiting on the daemon for its next event.
    parked: bool,
    /// Whether its failure has already been handed to the caller.
    reported: bool,
}

impl HealthEntry {
    /// When this entry expires, or [`None`] while it cannot.
    fn deadline(&self) -> Option<Instant> {
        if self.parked || self.reported {
            None
        } else {
            Some(self.expiry())
        }
    }

    /// The raw expiry instant, ignoring parking.
    fn expiry(&self) -> Instant {
        self.last_seen
            .checked_add(self.timeout)
            .unwrap_or(self.last_seen)
    }
}

/// One node that missed its liveness deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthExpiry {
    /// The dataflow the node belongs to.
    pub dataflow: DataflowId,
    /// The node that went silent.
    pub node: NodeId,
    /// The incarnation that went silent.
    pub generation: u64,
    /// How long it was silent for — the deadline, not the overshoot, so the
    /// reported cause names the budget the node blew rather than how late the
    /// loop happened to notice.
    pub after: Duration,
}

impl HealthExpiry {
    /// The typed exit cause this expiry produces (§12).
    #[must_use]
    pub fn cause(&self) -> NodeExitCause {
        NodeExitCause::HealthCheckTimeout {
            after: DurationMs::from_duration(self.after),
        }
    }
}

/// Every node the daemon watches for liveness.
#[derive(Debug, Default, Clone)]
pub struct HealthTable {
    /// One entry per monitored node.
    entries: BTreeMap<NodeKey, HealthEntry>,
    /// The timeout applied to a node whose specification names none.
    default_timeout: Option<Duration>,
}

impl HealthTable {
    /// A table that monitors only the nodes whose specification asks for it.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            default_timeout: None,
        }
    }

    /// A table that also applies `timeout` to nodes with no timeout of their
    /// own.
    #[must_use]
    pub const fn with_default_timeout(timeout: Duration) -> Self {
        Self {
            entries: BTreeMap::new(),
            default_timeout: Some(timeout),
        }
    }

    /// The fallback timeout, if one is configured.
    #[must_use]
    pub const fn default_timeout(&self) -> Option<Duration> {
        self.default_timeout
    }

    /// Sets the fallback timeout applied to nodes that name none.
    pub const fn set_default_timeout(&mut self, timeout: Option<Duration>) {
        self.default_timeout = timeout;
    }

    /// Starts watching a node that has just registered.
    ///
    /// `timeout` is the node's own `health_check_timeout`; [`None`] falls back
    /// to the table's default, and a table with no default simply does not
    /// monitor the node. Returns whether the node is now monitored.
    pub fn arm(
        &mut self,
        dataflow: DataflowId,
        node: NodeId,
        generation: u64,
        timeout: Option<Duration>,
        now: Instant,
    ) -> bool {
        let Some(timeout) = timeout.or(self.default_timeout) else {
            // Not monitored: drop any entry a previous incarnation left, so a
            // node whose specification was replaced does not keep an
            // inherited deadline.
            self.entries.remove(&(dataflow, node));
            return false;
        };
        if timeout.is_zero() {
            // A zero timeout would expire the instant it was armed, which is
            // never what an operator means by it; treat it as "unmonitored"
            // rather than as "kill immediately".
            self.entries.remove(&(dataflow, node));
            return false;
        }
        self.entries.insert(
            (dataflow, node),
            HealthEntry {
                generation,
                timeout,
                last_seen: now,
                parked: false,
                reported: false,
            },
        );
        true
    }

    /// Records that a node spoke.
    ///
    /// Returns whether the node was being monitored — a caller can ignore the
    /// answer, and the event loop does: touching an unmonitored node is the
    /// common case, not an error.
    pub fn touch(&mut self, dataflow: DataflowId, node: &NodeId, now: Instant) -> bool {
        match self.entries.get_mut(&(dataflow, node.clone())) {
            Some(entry) => {
                entry.last_seen = now;
                entry.parked = false;
                entry.reported = false;
                true
            }
            None => false,
        }
    }

    /// Records that a node is waiting on the daemon for its next event.
    ///
    /// A parked node has no deadline: it has demonstrably reached the top of
    /// its loop, and the silence that follows is the daemon's, not the node's.
    pub fn park(&mut self, dataflow: DataflowId, node: &NodeId, now: Instant) -> bool {
        match self.entries.get_mut(&(dataflow, node.clone())) {
            Some(entry) => {
                entry.last_seen = now;
                entry.parked = true;
                entry.reported = false;
                true
            }
            None => false,
        }
    }

    /// Records that the daemon served a parked node, restarting its deadline.
    pub fn unpark(&mut self, dataflow: DataflowId, node: &NodeId, now: Instant) -> bool {
        match self.entries.get_mut(&(dataflow, node.clone())) {
            Some(entry) => {
                entry.last_seen = now;
                entry.parked = false;
                true
            }
            None => false,
        }
    }

    /// Stops watching a node.
    pub fn disarm(&mut self, dataflow: DataflowId, node: &NodeId) -> bool {
        self.entries.remove(&(dataflow, node.clone())).is_some()
    }

    /// Stops watching a node, but only if the entry belongs to `generation`.
    ///
    /// The disarm a restart uses: an entry armed for a replaced incarnation
    /// must go, while one armed for the *new* incarnation — which may already
    /// have registered by the time the old one's exit is processed — must not.
    pub fn disarm_generation(
        &mut self,
        dataflow: DataflowId,
        node: &NodeId,
        generation: u64,
    ) -> bool {
        let key = (dataflow, node.clone());
        match self.entries.get(&key) {
            Some(entry) if entry.generation == generation => {
                self.entries.remove(&key);
                true
            }
            _ => false,
        }
    }

    /// Stops watching every node of one dataflow.
    pub fn disarm_dataflow(&mut self, dataflow: DataflowId) -> usize {
        let before = self.entries.len();
        self.entries.retain(|(id, _), _| *id != dataflow);
        before - self.entries.len()
    }

    /// Every node whose deadline has passed, marked reported so a second call
    /// at the same instant returns nothing.
    ///
    /// Reporting once is what keeps the event loop from failing the same node
    /// on every tick while its exit is still being processed. A node that
    /// speaks again — which a hung node by definition will not — clears the
    /// mark through [`HealthTable::touch`].
    pub fn expired(&mut self, now: Instant) -> Vec<HealthExpiry> {
        let mut expiries = Vec::new();
        for ((dataflow, node), entry) in &mut self.entries {
            let Some(deadline) = entry.deadline() else {
                continue;
            };
            if deadline > now {
                continue;
            }
            entry.reported = true;
            expiries.push(HealthExpiry {
                dataflow: *dataflow,
                node: node.clone(),
                generation: entry.generation,
                after: entry.timeout,
            });
        }
        expiries
    }

    /// The earliest deadline in the table.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.entries
            .values()
            .filter_map(HealthEntry::deadline)
            .min()
    }

    /// What the table believes about one node at `now`.
    #[must_use]
    pub fn state(&self, dataflow: DataflowId, node: &NodeId, now: Instant) -> LivenessState {
        match self.entries.get(&(dataflow, node.clone())) {
            None => LivenessState::Unmonitored,
            Some(entry) if entry.reported => LivenessState::Reported,
            Some(entry) if entry.parked => LivenessState::Parked,
            Some(entry) if entry.expiry() <= now => LivenessState::Overdue,
            Some(_) => LivenessState::Active,
        }
    }

    /// Whether a node is being watched.
    #[must_use]
    pub fn is_armed(&self, dataflow: DataflowId, node: &NodeId) -> bool {
        self.entries.contains_key(&(dataflow, node.clone()))
    }

    /// The incarnation an entry was armed for.
    #[must_use]
    pub fn generation_of(&self, dataflow: DataflowId, node: &NodeId) -> Option<u64> {
        self.entries
            .get(&(dataflow, node.clone()))
            .map(|entry| entry.generation)
    }

    /// The timeout applied to one node.
    #[must_use]
    pub fn timeout_of(&self, dataflow: DataflowId, node: &NodeId) -> Option<Duration> {
        self.entries
            .get(&(dataflow, node.clone()))
            .map(|entry| entry.timeout)
    }

    /// How many nodes are watched.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is watched.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every watched node, in key order.
    pub fn keys(&self) -> impl Iterator<Item = &NodeKey> {
        self.entries.keys()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn dataflow() -> DataflowId {
        DataflowId::from_u128(7)
    }

    fn node(name: &str) -> NodeId {
        NodeId::new(name).unwrap()
    }

    fn armed(timeout: Duration) -> (HealthTable, Instant) {
        let start = Instant::now();
        let mut table = HealthTable::new();
        assert!(table.arm(dataflow(), node("planner"), 0, Some(timeout), start));
        (table, start)
    }

    #[test]
    fn an_empty_table_watches_nothing() {
        let table = HealthTable::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert!(table.next_deadline().is_none());
        assert_eq!(
            table.state(dataflow(), &node("planner"), Instant::now()),
            LivenessState::Unmonitored
        );
    }

    #[test]
    fn a_node_without_a_timeout_is_not_monitored() {
        let mut table = HealthTable::new();
        assert!(!table.arm(dataflow(), node("planner"), 0, None, Instant::now()));
        assert!(table.is_empty());
    }

    #[test]
    fn a_default_timeout_monitors_a_node_that_names_none() {
        let mut table = HealthTable::with_default_timeout(Duration::from_secs(5));
        assert_eq!(table.default_timeout(), Some(Duration::from_secs(5)));
        assert!(table.arm(dataflow(), node("planner"), 0, None, Instant::now()));
        assert_eq!(
            table.timeout_of(dataflow(), &node("planner")),
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn a_nodes_own_timeout_wins_over_the_default() {
        let mut table = HealthTable::with_default_timeout(Duration::from_secs(5));
        table.arm(
            dataflow(),
            node("planner"),
            0,
            Some(Duration::from_millis(200)),
            Instant::now(),
        );
        assert_eq!(
            table.timeout_of(dataflow(), &node("planner")),
            Some(Duration::from_millis(200))
        );
    }

    #[test]
    fn a_zero_timeout_means_unmonitored_rather_than_instant_death() {
        let mut table = HealthTable::new();
        assert!(!table.arm(
            dataflow(),
            node("p"),
            0,
            Some(Duration::ZERO),
            Instant::now()
        ));
        assert!(table.is_empty());
    }

    #[test]
    fn arming_without_a_timeout_clears_an_inherited_entry() {
        let (mut table, start) = armed(Duration::from_secs(1));
        assert!(!table.arm(dataflow(), node("planner"), 1, None, start));
        assert!(!table.is_armed(dataflow(), &node("planner")));
    }

    #[test]
    fn silence_past_the_deadline_expires_once() {
        let (mut table, start) = armed(Duration::from_secs(2));
        assert!(table.expired(start + Duration::from_secs(1)).is_empty());

        let late = start + Duration::from_secs(3);
        let expiries = table.expired(late);
        assert_eq!(expiries.len(), 1);
        assert_eq!(expiries[0].node, node("planner"));
        assert_eq!(expiries[0].generation, 0);
        assert_eq!(expiries[0].after, Duration::from_secs(2));
        assert!(table.expired(late).is_empty(), "reported exactly once");
        assert_eq!(
            table.state(dataflow(), &node("planner"), late),
            LivenessState::Reported
        );
    }

    #[test]
    fn the_expiry_carries_a_typed_cause() {
        let (mut table, start) = armed(Duration::from_millis(500));
        let expiries = table.expired(start + Duration::from_secs(1));
        match expiries[0].cause() {
            NodeExitCause::HealthCheckTimeout { after } => {
                assert_eq!(after.to_duration(), Duration::from_millis(500));
            }
            other => panic!("unexpected cause {other:?}"),
        }
    }

    #[test]
    fn a_deadline_is_exactly_inclusive() {
        let (mut table, start) = armed(Duration::from_secs(2));
        // One nanosecond early is not yet due; the deadline itself is.
        assert!(
            table
                .expired(start + Duration::from_secs(2) - Duration::from_nanos(1))
                .is_empty()
        );
        assert_eq!(table.expired(start + Duration::from_secs(2)).len(), 1);
    }

    #[test]
    fn speaking_resets_the_deadline() {
        let (mut table, start) = armed(Duration::from_secs(2));
        assert!(table.touch(dataflow(), &node("planner"), start + Duration::from_secs(1)));
        assert!(table.expired(start + Duration::from_secs(2)).is_empty());
        assert_eq!(table.expired(start + Duration::from_secs(4)).len(), 1);
    }

    #[test]
    fn touching_an_unmonitored_node_is_a_no_op() {
        let mut table = HealthTable::new();
        assert!(!table.touch(dataflow(), &node("ghost"), Instant::now()));
        assert!(!table.park(dataflow(), &node("ghost"), Instant::now()));
        assert!(!table.unpark(dataflow(), &node("ghost"), Instant::now()));
    }

    #[test]
    fn a_parked_node_never_expires() {
        let (mut table, start) = armed(Duration::from_millis(50));
        assert!(table.park(dataflow(), &node("planner"), start));
        assert_eq!(
            table.state(
                dataflow(),
                &node("planner"),
                start + Duration::from_secs(600)
            ),
            LivenessState::Parked
        );
        assert!(table.expired(start + Duration::from_secs(600)).is_empty());
        assert!(table.next_deadline().is_none());
    }

    #[test]
    fn unparking_restarts_the_deadline() {
        let (mut table, start) = armed(Duration::from_secs(2));
        table.park(dataflow(), &node("planner"), start);
        let served = start + Duration::from_secs(100);
        assert!(table.unpark(dataflow(), &node("planner"), served));
        assert!(table.expired(served + Duration::from_secs(1)).is_empty());
        assert_eq!(table.expired(served + Duration::from_secs(3)).len(), 1);
    }

    #[test]
    fn speaking_after_a_report_clears_the_mark() {
        let (mut table, start) = armed(Duration::from_secs(1));
        assert_eq!(table.expired(start + Duration::from_secs(2)).len(), 1);
        table.touch(dataflow(), &node("planner"), start + Duration::from_secs(2));
        assert_eq!(
            table.state(dataflow(), &node("planner"), start + Duration::from_secs(2)),
            LivenessState::Active
        );
        assert_eq!(table.expired(start + Duration::from_secs(4)).len(), 1);
    }

    #[test]
    fn a_stale_generation_disarm_leaves_the_replacement_alone() {
        let (mut table, start) = armed(Duration::from_secs(1));
        table.arm(
            dataflow(),
            node("planner"),
            1,
            Some(Duration::from_secs(1)),
            start,
        );
        assert!(!table.disarm_generation(dataflow(), &node("planner"), 0));
        assert!(table.is_armed(dataflow(), &node("planner")));
        assert!(table.disarm_generation(dataflow(), &node("planner"), 1));
        assert!(!table.is_armed(dataflow(), &node("planner")));
    }

    #[test]
    fn disarming_removes_one_node() {
        let (mut table, _) = armed(Duration::from_secs(1));
        assert!(table.disarm(dataflow(), &node("planner")));
        assert!(!table.disarm(dataflow(), &node("planner")));
        assert!(table.is_empty());
    }

    #[test]
    fn disarming_a_dataflow_removes_only_its_nodes() {
        let start = Instant::now();
        let mut table = HealthTable::with_default_timeout(Duration::from_secs(1));
        table.arm(dataflow(), node("a"), 0, None, start);
        table.arm(dataflow(), node("b"), 0, None, start);
        table.arm(DataflowId::from_u128(9), node("c"), 0, None, start);

        assert_eq!(table.disarm_dataflow(dataflow()), 2);
        assert_eq!(table.len(), 1);
        assert!(table.is_armed(DataflowId::from_u128(9), &node("c")));
    }

    #[test]
    fn the_next_deadline_is_the_earliest_unparked_one() {
        let start = Instant::now();
        let mut table = HealthTable::new();
        table.arm(
            dataflow(),
            node("slow"),
            0,
            Some(Duration::from_secs(10)),
            start,
        );
        table.arm(
            dataflow(),
            node("fast"),
            0,
            Some(Duration::from_secs(2)),
            start,
        );

        assert_eq!(table.next_deadline(), Some(start + Duration::from_secs(2)));
        table.park(dataflow(), &node("fast"), start);
        assert_eq!(table.next_deadline(), Some(start + Duration::from_secs(10)));
    }

    #[test]
    fn several_nodes_expire_together() {
        let start = Instant::now();
        let mut table = HealthTable::with_default_timeout(Duration::from_millis(10));
        for name in ["a", "b", "c"] {
            table.arm(dataflow(), node(name), 2, None, start);
        }
        let expiries = table.expired(start + Duration::from_millis(20));
        assert_eq!(expiries.len(), 3);
        assert!(expiries.iter().all(|expiry| expiry.generation == 2));
        assert_eq!(table.keys().count(), 3, "expiry reports, it does not evict");
    }

    #[test]
    fn states_have_distinct_labels() {
        let states = [
            LivenessState::Unmonitored,
            LivenessState::Active,
            LivenessState::Parked,
            LivenessState::Overdue,
            LivenessState::Reported,
        ];
        let mut labels: Vec<&str> = states.iter().map(|state| state.as_str()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), states.len());
        assert!(LivenessState::Active.is_healthy());
        assert!(LivenessState::Parked.is_healthy());
        assert!(!LivenessState::Overdue.is_healthy());
    }

    #[test]
    fn the_default_timeout_can_be_changed_afterwards() {
        let mut table = HealthTable::new();
        table.set_default_timeout(Some(Duration::from_secs(3)));
        assert!(table.arm(dataflow(), node("p"), 0, None, Instant::now()));
        table.set_default_timeout(None);
        assert!(!table.arm(dataflow(), node("q"), 0, None, Instant::now()));
    }

    #[test]
    fn the_generation_is_readable() {
        let start = Instant::now();
        let mut table = HealthTable::new();
        table.arm(
            dataflow(),
            node("p"),
            4,
            Some(Duration::from_secs(1)),
            start,
        );
        assert_eq!(table.generation_of(dataflow(), &node("p")), Some(4));
        assert_eq!(table.generation_of(dataflow(), &node("missing")), None);
    }
}
