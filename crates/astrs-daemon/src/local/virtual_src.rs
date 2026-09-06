//! Virtual inputs — `astrs/timer/*`, `astrs/logs/*`, `astrs/status` (§8.4).
//!
//! Three synthetic producers, all of them the daemon, all of them ordinary
//! routes in the [`crate::state::RouteTable`]:
//!
//! | Source | Produced by | Payload |
//! |---|---|---|
//! | `astrs/timer/hz/N`, `astrs/timer/millis/N`, `astrs/timer/secs/N` | the shared timer wheel | empty; the tick *is* the message |
//! | `astrs/logs[/level[/node]]` | log capture and the daemon's own records | a JSON [`astrs_log::LogRecord`] |
//! | `astrs/status` | the supervisor | the lifecycle [`astrs_wire::NodeEvent`] itself |
//!
//! # One wheel, many subscribers
//!
//! Every `astrs/timer/*` subscription in the daemon shares one
//! [`astrs_scheduler::TimerWheel`] — the blueprint's §11.1 design, and the
//! reason [`astrs_scheduler::TimerSpec::tag`] exists. [`TimerRegistry`] is the
//! side table that turns a fired tag back into "which dataflow, which
//! subscribers", so a hundred nodes ticking at 50 Hz cost one wheel entry per
//! *distinct interval*, not one per node.
//!
//! # Log filtering happens once
//!
//! `astrs/logs/warn/camera` is a filter, not a stream: [`LogSubscriptions`]
//! parses each subscribed path into an [`astrs_log::LogFilter`] at subscribe
//! time and evaluates it per record, so a record that nobody wants costs one
//! predicate per subscriber and no serialization at all.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::local::{TimerRegistry, LogSubscriptions};
//! use astrs_log::{HlcTimestamp, LogLevel, LogRecord};
//! use astrs_manifest::VirtualSource;
//! use astrs_wire::{DataId, DataflowId, NodeId};
//! use std::time::Instant;
//!
//! let mut timers = TimerRegistry::new(Instant::now());
//! timers.subscribe(
//!     DataflowId::from_u128(1),
//!     NodeId::new("planner")?,
//!     DataId::new("tick")?,
//!     VirtualSource::TimerHz(50),
//! )?;
//! assert_eq!(timers.len(), 1);
//!
//! let mut logs = LogSubscriptions::new();
//! logs.subscribe(
//!     DataflowId::from_u128(1),
//!     NodeId::new("watcher")?,
//!     DataId::new("errors")?,
//!     "astrs/logs/error",
//! )?;
//!
//! let record = LogRecord::new(HlcTimestamp::default(), LogLevel::Error, "camera", "boom");
//! assert_eq!(logs.matching(DataflowId::from_u128(1), &record).count(), 1);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use astrs_log::{LogFilter, LogRecord};
use astrs_manifest::VirtualSource;
use astrs_scheduler::{JitterStats, MissedTickPolicy, TimerFired, TimerId, TimerSpec, TimerWheel};
use astrs_time::TimerInterval;
use astrs_wire::{DataId, DataflowId, NodeId};

use crate::error::{DaemonError, DaemonResult};

/// One node's subscription to a virtual source.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct VirtualSubscriber {
    /// The dataflow the subscriber belongs to.
    pub dataflow: DataflowId,
    /// The subscribing node.
    pub node: NodeId,
    /// The input the ticks or records arrive on.
    pub input: DataId,
}

impl VirtualSubscriber {
    /// A subscriber record.
    #[must_use]
    pub const fn new(dataflow: DataflowId, node: NodeId, input: DataId) -> Self {
        Self {
            dataflow,
            node,
            input,
        }
    }
}

/// One interval's worth of timer subscribers.
#[derive(Debug, Clone)]
struct TimerGroup {
    /// The wheel entry serving them.
    id: TimerId,
    /// Everybody waiting on this interval.
    subscribers: Vec<VirtualSubscriber>,
}

/// Every `astrs/timer/*` subscription, sharing one wheel (§11.1).
#[derive(Debug)]
pub struct TimerRegistry {
    /// The shared wheel.
    wheel: TimerWheel,
    /// One group per distinct period, keyed by nanoseconds so two spellings of
    /// the same rate (`hz/50` and `millis/20`) share an entry.
    groups: BTreeMap<u64, TimerGroup>,
    /// Wheel tag → period, so a fired tick finds its group.
    by_tag: BTreeMap<u64, u64>,
    /// The next tag to hand out.
    next_tag: u64,
}

impl TimerRegistry {
    /// An empty registry whose wheel starts at `epoch`.
    #[must_use]
    pub fn new(epoch: Instant) -> Self {
        Self {
            wheel: TimerWheel::new(epoch),
            groups: BTreeMap::new(),
            by_tag: BTreeMap::new(),
            next_tag: 0,
        }
    }

    /// Subscribes `node`'s `input` to `source`.
    ///
    /// # Errors
    ///
    /// [`DaemonError::Manifest`] if `source` is not a timer, or its period is
    /// not representable — a `hz/0` the manifest validator already refuses.
    pub fn subscribe(
        &mut self,
        dataflow: DataflowId,
        node: NodeId,
        input: DataId,
        source: VirtualSource,
    ) -> DaemonResult<()> {
        self.subscribe_at(dataflow, node, input, source, Instant::now())
    }

    /// [`TimerRegistry::subscribe`] with an explicit clock reading.
    pub fn subscribe_at(
        &mut self,
        dataflow: DataflowId,
        node: NodeId,
        input: DataId,
        source: VirtualSource,
        now: Instant,
    ) -> DaemonResult<()> {
        let interval = timer_interval(&source)?;
        let period = interval.period();
        let nanos = u64::try_from(period.as_nanos()).unwrap_or(u64::MAX);
        let subscriber = VirtualSubscriber::new(dataflow, node, input);

        match self.groups.get_mut(&nanos) {
            Some(group) => {
                if !group.subscribers.contains(&subscriber) {
                    group.subscribers.push(subscriber);
                }
            }
            None => {
                let tag = self.next_tag;
                self.next_tag = self.next_tag.saturating_add(1);
                // Anchor the drift-free grid on the moment the daemon armed
                // it. `TimerInterval::from_*` anchors on its own
                // construction instant, which for a caller that captured
                // `now` earlier — a test driving a `ManualClock`, an event
                // loop batching several subscribes — would put the first
                // tick fractionally in the past and fire it immediately.
                let spec =
                    TimerSpec::new(interval.rebase(now), MissedTickPolicy::Skip).with_tag(tag);
                let id = self.wheel.insert(spec, now);
                self.by_tag.insert(tag, nanos);
                self.groups.insert(
                    nanos,
                    TimerGroup {
                        id,
                        subscribers: vec![subscriber],
                    },
                );
            }
        }
        Ok(())
    }

    /// Removes one subscription, cancelling the wheel entry if it was the last.
    pub fn unsubscribe(&mut self, dataflow: DataflowId, node: &NodeId, input: &DataId) -> bool {
        let mut removed = false;
        let mut empty = Vec::new();
        for (nanos, group) in &mut self.groups {
            let before = group.subscribers.len();
            group.subscribers.retain(|subscriber| {
                !(subscriber.dataflow == dataflow
                    && subscriber.node == *node
                    && subscriber.input == *input)
            });
            removed |= group.subscribers.len() != before;
            if group.subscribers.is_empty() {
                empty.push((*nanos, group.id));
            }
        }
        for (nanos, id) in empty {
            self.wheel.cancel(id);
            self.groups.remove(&nanos);
            self.by_tag.retain(|_, group_nanos| *group_nanos != nanos);
        }
        removed
    }

    /// Removes every subscription belonging to `node`.
    pub fn unsubscribe_node(&mut self, dataflow: DataflowId, node: &NodeId) -> usize {
        let inputs: Vec<DataId> = self
            .groups
            .values()
            .flat_map(|group| group.subscribers.iter())
            .filter(|subscriber| subscriber.dataflow == dataflow && subscriber.node == *node)
            .map(|subscriber| subscriber.input.clone())
            .collect();
        let mut removed = 0;
        for input in inputs {
            if self.unsubscribe(dataflow, node, &input) {
                removed += 1;
            }
        }
        removed
    }

    /// Removes every subscription belonging to `dataflow`.
    pub fn unsubscribe_dataflow(&mut self, dataflow: DataflowId) -> usize {
        let nodes: Vec<NodeId> = self
            .groups
            .values()
            .flat_map(|group| group.subscribers.iter())
            .filter(|subscriber| subscriber.dataflow == dataflow)
            .map(|subscriber| subscriber.node.clone())
            .collect();
        let mut removed = 0;
        for node in nodes {
            removed += self.unsubscribe_node(dataflow, &node);
        }
        removed
    }

    /// How many distinct intervals the wheel is serving.
    #[must_use]
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// Whether nothing is subscribed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// How many subscribers there are in total.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.groups
            .values()
            .map(|group| group.subscribers.len())
            .sum()
    }

    /// When the wheel next has something to do.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.wheel.next_deadline()
    }

    /// Advances the wheel and returns the subscribers whose timers fired.
    ///
    /// One entry per `(subscriber, tick)` pair: a tick that coalesced several
    /// missed periods appears once, because [`MissedTickPolicy::Skip`] is what
    /// a control loop wants — three late ticks delivered at once are three
    /// stale commands, not a catch-up.
    pub fn advance(&mut self, now: Instant) -> Vec<(VirtualSubscriber, TimerFired)> {
        let fired = self.wheel.advance(now);
        let mut ticks = Vec::new();
        for tick in fired {
            let Some(nanos) = self.by_tag.get(&tick.tag).copied() else {
                continue;
            };
            let Some(group) = self.groups.get(&nanos) else {
                continue;
            };
            for subscriber in &group.subscribers {
                ticks.push((subscriber.clone(), tick));
            }
        }
        ticks
    }

    /// `node`'s current 99th-percentile timer jitter within `dataflow`, in
    /// whole microseconds (§11.1: `timer_jitter_us`, exported per node on
    /// [`astrs_wire::NodeMetricsSample::timer_jitter_p99_us`]).
    ///
    /// A node may subscribe more than one `astrs/timer/*` input at
    /// different rates, each served by its own wheel entry (`TimerGroup`);
    /// this reports the worst (largest) of them, since that is the rate an
    /// operator investigating a jittery node needs to see. `0` for a node
    /// with no timer subscription in `dataflow`, or one whose wheel
    /// entry/entries have not yet delivered a tick to estimate from
    /// ([`JitterStats::p99`] is `None` before the first observation) —
    /// indistinguishable from "no jitter measured yet", which is the
    /// correct reading for a brand-new subscription.
    #[must_use]
    pub fn jitter_p99_us(&self, dataflow: DataflowId, node: &NodeId) -> u64 {
        self.groups
            .values()
            .filter(|group| {
                group
                    .subscribers
                    .iter()
                    .any(|subscriber| subscriber.dataflow == dataflow && subscriber.node == *node)
            })
            .filter_map(|group| self.wheel.jitter_stats(group.id))
            .filter_map(JitterStats::p99)
            .map(micros)
            .max()
            .unwrap_or(0)
    }

    /// The worst 99th-percentile timer jitter across every distinct period
    /// the shared wheel currently serves, in whole microseconds — the
    /// daemon-wide reading published as
    /// [`crate::metrics::names::TIMER_JITTER_P99_US`] (§11.1).
    ///
    /// `0` if nothing is subscribed, or nothing has ticked yet — see
    /// [`TimerRegistry::jitter_p99_us`]'s docs on why that is the correct
    /// reading rather than a sentinel.
    #[must_use]
    pub fn overall_jitter_p99_us(&self) -> u64 {
        self.groups
            .values()
            .filter_map(|group| self.wheel.jitter_stats(group.id))
            .filter_map(JitterStats::p99)
            .map(micros)
            .max()
            .unwrap_or(0)
    }
}

/// A [`Duration`] as whole microseconds, saturating rather than panicking or
/// wrapping on a value beyond `u64`'s range (never reachable in practice —
/// the wheel's own addressable range is ~12.4 days, [`Duration::as_micros`]
/// would need a duration around 584,942 years to overflow — but a saturating
/// conversion costs nothing and needs no justification per call site).
fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// The [`TimerInterval`] a virtual source describes.
///
/// # Errors
///
/// [`DaemonError::Manifest`] for a non-timer source, or a period the interval
/// type refuses (zero, or beyond its range).
pub fn timer_interval(source: &VirtualSource) -> DaemonResult<TimerInterval> {
    let result = match source {
        VirtualSource::TimerMillis(millis) => {
            TimerInterval::from_period(Duration::from_millis(*millis))
        }
        VirtualSource::TimerSecs(secs) => TimerInterval::from_period(Duration::from_secs(*secs)),
        #[allow(clippy::cast_precision_loss)]
        VirtualSource::TimerHz(hz) => TimerInterval::from_hz(*hz as f64),
        other => {
            return Err(DaemonError::Manifest(format!(
                "{other:?} is not a timer source"
            )));
        }
    };
    result.map_err(|error| DaemonError::Manifest(format!("timer source: {error}")))
}

/// Every `astrs/logs*` subscription, as parsed filters.
#[derive(Debug, Default)]
pub struct LogSubscriptions {
    /// The subscribers and the filter each one asked for.
    entries: Vec<(VirtualSubscriber, LogFilter)>,
}

impl LogSubscriptions {
    /// An empty set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Subscribes `node`'s `input` to the `astrs/logs…` path `source`.
    ///
    /// # Errors
    ///
    /// [`DaemonError::Manifest`] if the path is not a legal
    /// `astrs/logs[/level[/node]]` filter.
    pub fn subscribe(
        &mut self,
        dataflow: DataflowId,
        node: NodeId,
        input: DataId,
        source: &str,
    ) -> DaemonResult<()> {
        let filter: LogFilter = source
            .parse()
            .map_err(|error| DaemonError::Manifest(format!("{source}: {error}")))?;
        let subscriber = VirtualSubscriber::new(dataflow, node, input);
        match self
            .entries
            .iter_mut()
            .find(|(existing, _)| *existing == subscriber)
        {
            Some(entry) => entry.1 = filter,
            None => self.entries.push((subscriber, filter)),
        }
        Ok(())
    }

    /// Removes one subscription.
    pub fn unsubscribe(&mut self, dataflow: DataflowId, node: &NodeId, input: &DataId) -> bool {
        let before = self.entries.len();
        self.entries.retain(|(subscriber, _)| {
            !(subscriber.dataflow == dataflow
                && subscriber.node == *node
                && subscriber.input == *input)
        });
        self.entries.len() != before
    }

    /// Removes every subscription belonging to `node`.
    pub fn unsubscribe_node(&mut self, dataflow: DataflowId, node: &NodeId) -> usize {
        let before = self.entries.len();
        self.entries.retain(|(subscriber, _)| {
            !(subscriber.dataflow == dataflow && subscriber.node == *node)
        });
        before - self.entries.len()
    }

    /// Removes every subscription belonging to `dataflow`.
    pub fn unsubscribe_dataflow(&mut self, dataflow: DataflowId) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|(subscriber, _)| subscriber.dataflow != dataflow);
        before - self.entries.len()
    }

    /// How many subscriptions there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is subscribed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The subscribers within `dataflow` whose filter `record` passes.
    ///
    /// Log fan-out is dataflow-scoped: a node in one dataflow never sees
    /// another's logs, whatever its filter says.
    pub fn matching<'a>(
        &'a self,
        dataflow: DataflowId,
        record: &'a LogRecord,
    ) -> impl Iterator<Item = &'a VirtualSubscriber> {
        self.entries
            .iter()
            .filter(move |(subscriber, filter)| {
                subscriber.dataflow == dataflow && filter.matches(record)
            })
            .map(|(subscriber, _)| subscriber)
    }

    /// Whether anybody in `dataflow` would take `record` — the cheap
    /// pre-check before serializing it.
    #[must_use]
    pub fn any_match(&self, dataflow: DataflowId, record: &LogRecord) -> bool {
        self.matching(dataflow, record).next().is_some()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use astrs_log::{HlcTimestamp, LogLevel};

    use super::*;

    fn dataflow() -> DataflowId {
        DataflowId::from_u128(1)
    }

    fn node(name: &str) -> NodeId {
        NodeId::new(name).unwrap()
    }

    fn data(name: &str) -> DataId {
        DataId::new(name).unwrap()
    }

    fn record(level: LogLevel, node_name: Option<&str>) -> LogRecord {
        let record = LogRecord::new(HlcTimestamp::default(), level, "test", "message");
        match node_name {
            Some(name) => record.with_node(name),
            None => record,
        }
    }

    #[test]
    fn an_empty_registry_has_no_deadline() {
        let registry = TimerRegistry::new(Instant::now());
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert_eq!(registry.subscriber_count(), 0);
        assert!(registry.next_deadline().is_none());
    }

    #[test]
    fn subscribing_arms_the_wheel() {
        let start = Instant::now();
        let mut registry = TimerRegistry::new(start);
        registry
            .subscribe_at(
                dataflow(),
                node("planner"),
                data("tick"),
                VirtualSource::TimerMillis(10),
                start,
            )
            .unwrap();
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.subscriber_count(), 1);
        assert!(registry.next_deadline().is_some());
    }

    #[test]
    fn one_wheel_entry_serves_every_subscriber_at_the_same_rate() {
        let start = Instant::now();
        let mut registry = TimerRegistry::new(start);
        for name in ["a", "b", "c"] {
            registry
                .subscribe_at(
                    dataflow(),
                    node(name),
                    data("tick"),
                    VirtualSource::TimerMillis(10),
                    start,
                )
                .unwrap();
        }
        assert_eq!(registry.len(), 1, "one interval, one wheel entry");
        assert_eq!(registry.subscriber_count(), 3);

        let ticks = registry.advance(start + Duration::from_millis(10));
        assert_eq!(ticks.len(), 3, "every subscriber gets the tick");
    }

    #[test]
    fn two_spellings_of_one_rate_share_an_entry() {
        let start = Instant::now();
        let mut registry = TimerRegistry::new(start);
        registry
            .subscribe_at(
                dataflow(),
                node("a"),
                data("tick"),
                VirtualSource::TimerHz(50),
                start,
            )
            .unwrap();
        registry
            .subscribe_at(
                dataflow(),
                node("b"),
                data("tick"),
                VirtualSource::TimerMillis(20),
                start,
            )
            .unwrap();
        assert_eq!(registry.len(), 1, "50 Hz and 20 ms are the same period");
        assert_eq!(registry.subscriber_count(), 2);
    }

    #[test]
    fn different_rates_get_different_entries() {
        let start = Instant::now();
        let mut registry = TimerRegistry::new(start);
        registry
            .subscribe_at(
                dataflow(),
                node("fast"),
                data("tick"),
                VirtualSource::TimerMillis(10),
                start,
            )
            .unwrap();
        registry
            .subscribe_at(
                dataflow(),
                node("slow"),
                data("tick"),
                VirtualSource::TimerSecs(1),
                start,
            )
            .unwrap();
        assert_eq!(registry.len(), 2);

        let ticks = registry.advance(start + Duration::from_millis(10));
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[0].0.node, node("fast"));
    }

    #[test]
    fn a_fresh_registry_reports_no_jitter_anywhere() {
        let registry = TimerRegistry::new(Instant::now());
        assert_eq!(registry.jitter_p99_us(dataflow(), &node("nobody")), 0);
        assert_eq!(registry.overall_jitter_p99_us(), 0);
    }

    #[test]
    fn an_unsubscribed_node_reports_no_jitter_even_when_others_have_some() {
        let start = Instant::now();
        let mut registry = TimerRegistry::new(start);
        registry
            .subscribe_at(
                dataflow(),
                node("ticking"),
                data("tick"),
                VirtualSource::TimerMillis(10),
                start,
            )
            .unwrap();
        // Deliver late enough to guarantee a nonzero jitter reading below.
        let _ = registry.advance(start + Duration::from_millis(15));

        assert_eq!(registry.jitter_p99_us(dataflow(), &node("idle")), 0);
    }

    #[test]
    fn a_late_delivery_produces_a_nonzero_jitter_reading_for_the_subscribed_node() {
        let start = Instant::now();
        let mut registry = TimerRegistry::new(start);
        registry
            .subscribe_at(
                dataflow(),
                node("planner"),
                data("tick"),
                VirtualSource::TimerMillis(10),
                start,
            )
            .unwrap();

        // On-grid, then a few deliveries several ms late — real jitter, not
        // the zero a perfectly on-time tick would report.
        let _ = registry.advance(start + Duration::from_millis(10));
        let _ = registry.advance(start + Duration::from_millis(25));
        let _ = registry.advance(start + Duration::from_millis(38));

        let per_node = registry.jitter_p99_us(dataflow(), &node("planner"));
        assert!(per_node > 0, "expected nonzero jitter, got {per_node}");
        assert_eq!(
            registry.overall_jitter_p99_us(),
            per_node,
            "the only subscriber is the whole wheel's worst reading"
        );

        // A different dataflow, or a node this one never subscribed, is
        // unaffected by another dataflow's jitter.
        assert_eq!(
            registry.jitter_p99_us(DataflowId::from_u128(2), &node("planner")),
            0
        );
    }

    #[test]
    fn the_overall_reading_is_the_worst_of_several_groups() {
        let start = Instant::now();
        let mut registry = TimerRegistry::new(start);
        // `quiet`'s 10s period is nowhere near due within this test, so its
        // wheel entry never observes a tick at all — deliberately, to prove
        // an unobserved group contributes nothing rather than a spurious
        // zero that could mask a real reading from a group that has ticked.
        registry
            .subscribe_at(
                dataflow(),
                node("quiet"),
                data("tick"),
                VirtualSource::TimerSecs(10),
                start,
            )
            .unwrap();
        registry
            .subscribe_at(
                dataflow(),
                node("jittery"),
                data("tick"),
                VirtualSource::TimerMillis(10),
                start,
            )
            .unwrap();

        // One single-period-late delivery for `jittery`; `quiet` is not due
        // for another ~9.99 seconds and does not fire at all.
        let _ = registry.advance(start + Duration::from_millis(13));

        assert_eq!(
            registry.jitter_p99_us(dataflow(), &node("quiet")),
            0,
            "an unobserved wheel entry reports no jitter, not a fabricated one"
        );
        let jittery = registry.jitter_p99_us(dataflow(), &node("jittery"));
        assert!(jittery > 0, "expected nonzero jitter, got {jittery}");
        assert_eq!(registry.overall_jitter_p99_us(), jittery);
    }

    #[test]
    fn subscribing_the_same_input_twice_is_idempotent() {
        let start = Instant::now();
        let mut registry = TimerRegistry::new(start);
        for _ in 0..3 {
            registry
                .subscribe_at(
                    dataflow(),
                    node("a"),
                    data("tick"),
                    VirtualSource::TimerMillis(10),
                    start,
                )
                .unwrap();
        }
        assert_eq!(registry.subscriber_count(), 1);
    }

    #[test]
    fn the_last_unsubscribe_cancels_the_wheel_entry() {
        let start = Instant::now();
        let mut registry = TimerRegistry::new(start);
        registry
            .subscribe_at(
                dataflow(),
                node("a"),
                data("tick"),
                VirtualSource::TimerMillis(10),
                start,
            )
            .unwrap();
        registry
            .subscribe_at(
                dataflow(),
                node("b"),
                data("tick"),
                VirtualSource::TimerMillis(10),
                start,
            )
            .unwrap();

        assert!(registry.unsubscribe(dataflow(), &node("a"), &data("tick")));
        assert_eq!(registry.len(), 1, "b still wants it");

        assert!(registry.unsubscribe(dataflow(), &node("b"), &data("tick")));
        assert!(registry.is_empty());
        assert!(registry.next_deadline().is_none());
        assert!(registry.advance(start + Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn unsubscribing_something_absent_is_not_an_error() {
        let mut registry = TimerRegistry::new(Instant::now());
        assert!(!registry.unsubscribe(dataflow(), &node("nobody"), &data("tick")));
    }

    #[test]
    fn a_node_and_a_dataflow_can_be_unsubscribed_wholesale() {
        let start = Instant::now();
        let mut registry = TimerRegistry::new(start);
        for (name, input) in [("a", "t1"), ("a", "t2"), ("b", "t1")] {
            registry
                .subscribe_at(
                    dataflow(),
                    node(name),
                    data(input),
                    VirtualSource::TimerMillis(10),
                    start,
                )
                .unwrap();
        }
        assert_eq!(registry.unsubscribe_node(dataflow(), &node("a")), 2);
        assert_eq!(registry.subscriber_count(), 1);
        assert_eq!(registry.unsubscribe_dataflow(dataflow()), 1);
        assert!(registry.is_empty());
    }

    #[test]
    fn a_non_timer_source_is_refused() {
        let mut registry = TimerRegistry::new(Instant::now());
        let error = registry
            .subscribe(dataflow(), node("a"), data("x"), VirtualSource::Status)
            .unwrap_err();
        assert!(matches!(error, DaemonError::Manifest(_)), "{error}");
        assert!(
            timer_interval(&VirtualSource::Logs {
                level: None,
                node: None
            })
            .is_err()
        );
    }

    #[test]
    fn the_interval_matches_the_source_it_came_from() {
        assert_eq!(
            timer_interval(&VirtualSource::TimerMillis(20))
                .unwrap()
                .period(),
            Duration::from_millis(20)
        );
        assert_eq!(
            timer_interval(&VirtualSource::TimerSecs(2))
                .unwrap()
                .period(),
            Duration::from_secs(2)
        );
        assert_eq!(
            timer_interval(&VirtualSource::TimerHz(50))
                .unwrap()
                .period(),
            Duration::from_millis(20)
        );
    }

    #[test]
    fn an_empty_log_subscription_set_matches_nothing() {
        let logs = LogSubscriptions::new();
        assert!(logs.is_empty());
        assert_eq!(logs.len(), 0);
        assert!(!logs.any_match(dataflow(), &record(LogLevel::Error, None)));
    }

    #[test]
    fn a_level_filter_selects_by_severity() {
        let mut logs = LogSubscriptions::new();
        logs.subscribe(
            dataflow(),
            node("watcher"),
            data("errors"),
            "astrs/logs/error",
        )
        .unwrap();

        assert!(logs.any_match(dataflow(), &record(LogLevel::Error, None)));
        assert!(
            !logs.any_match(dataflow(), &record(LogLevel::Info, None)),
            "info is below error"
        );
    }

    #[test]
    fn a_node_filter_selects_by_producer() {
        let mut logs = LogSubscriptions::new();
        logs.subscribe(
            dataflow(),
            node("watcher"),
            data("cam"),
            "astrs/logs/warn/camera",
        )
        .unwrap();

        assert!(logs.any_match(dataflow(), &record(LogLevel::Error, Some("camera"))));
        assert!(!logs.any_match(dataflow(), &record(LogLevel::Error, Some("detect"))));
    }

    #[test]
    fn the_bare_path_takes_everything() {
        let mut logs = LogSubscriptions::new();
        logs.subscribe(dataflow(), node("watcher"), data("all"), "astrs/logs")
            .unwrap();
        for level in [LogLevel::Trace, LogLevel::Info, LogLevel::Error] {
            assert!(logs.any_match(dataflow(), &record(level, Some("anyone"))));
        }
    }

    #[test]
    fn log_fan_out_is_dataflow_scoped() {
        let mut logs = LogSubscriptions::new();
        logs.subscribe(dataflow(), node("watcher"), data("all"), "astrs/logs")
            .unwrap();
        assert!(
            !logs.any_match(DataflowId::from_u128(2), &record(LogLevel::Error, None)),
            "another dataflow's records are not this node's business"
        );
    }

    #[test]
    fn a_malformed_log_path_is_refused() {
        let mut logs = LogSubscriptions::new();
        let error = logs
            .subscribe(dataflow(), node("w"), data("x"), "astrs/logs/nonsense")
            .unwrap_err();
        assert!(matches!(error, DaemonError::Manifest(_)), "{error}");
        assert!(logs.is_empty());
    }

    #[test]
    fn resubscribing_replaces_the_filter() {
        let mut logs = LogSubscriptions::new();
        logs.subscribe(dataflow(), node("w"), data("x"), "astrs/logs/error")
            .unwrap();
        logs.subscribe(dataflow(), node("w"), data("x"), "astrs/logs/trace")
            .unwrap();
        assert_eq!(logs.len(), 1);
        assert!(logs.any_match(dataflow(), &record(LogLevel::Debug, None)));
    }

    #[test]
    fn log_subscriptions_can_be_removed_at_every_granularity() {
        let mut logs = LogSubscriptions::new();
        logs.subscribe(dataflow(), node("a"), data("x"), "astrs/logs")
            .unwrap();
        logs.subscribe(dataflow(), node("a"), data("y"), "astrs/logs")
            .unwrap();
        logs.subscribe(dataflow(), node("b"), data("x"), "astrs/logs")
            .unwrap();

        assert!(logs.unsubscribe(dataflow(), &node("a"), &data("x")));
        assert!(!logs.unsubscribe(dataflow(), &node("a"), &data("x")));
        assert_eq!(logs.unsubscribe_node(dataflow(), &node("a")), 1);
        assert_eq!(logs.unsubscribe_dataflow(dataflow()), 1);
        assert!(logs.is_empty());
    }

    #[test]
    fn matching_lists_every_interested_subscriber() {
        let mut logs = LogSubscriptions::new();
        logs.subscribe(dataflow(), node("a"), data("x"), "astrs/logs")
            .unwrap();
        logs.subscribe(dataflow(), node("b"), data("y"), "astrs/logs/error")
            .unwrap();

        let error = record(LogLevel::Error, None);
        assert_eq!(logs.matching(dataflow(), &error).count(), 2);
        let info = record(LogLevel::Info, None);
        assert_eq!(logs.matching(dataflow(), &info).count(), 1);
    }
}
