//! Debug taps: `astrs topic echo | hz | info` (§13, §17).
//!
//! > *`astrs topic echo|hz|info`: daemon-side taps (explicitly enabled per
//! > dataflow with `debug: true`) stream frames to the CLI through the
//! > coordinator over the standard framing.*
//!
//! A tap is a *copy* of a local output on its way to a CLI subscriber. It is
//! opt-in per dataflow, and deliberately so: a tap forces the daemon to keep
//! seeing the bytes, which means an output with an active tap cannot be moved
//! onto the shared-memory plane ([`crate::shm::ShmRefusal::TapActive`]). Paying
//! that price silently, on every dataflow, to serve a debugging command nobody
//! ran would be exactly the wrong default.
//!
//! ```text
//!   node publishes ──► daemon fan-out ──┬──► consumers (the real path)
//!                                       │
//!                                       └──► TapRegistry::capture
//!                                                 │ DataFrame
//!                                                 ▼
//!                                    ReportSink ──► coordinator ──► CLI
//! ```
//!
//! # Rate shaping
//!
//! `astrs topic echo --hz 2` on a 200 Hz camera must not push 200 frames a
//! second through the control plane. A subscription may carry a maximum rate;
//! frames arriving inside the resulting interval are counted as *dropped*
//! rather than queued, and the count rides with the next frame that does go
//! out ([`astrs_wire::DaemonEvent::TopicTapData::dropped`]) so the operator can
//! see what they are not seeing.
//!
//! Dropping rather than queueing is the only honest choice: a queue in front of
//! a human reading a terminal is a queue that grows without bound.
//!
//! # Examples
//!
//! ```
//! use std::time::Instant;
//! use astrs_daemon::tap::TapRegistry;
//! use astrs_wire::{DataflowId, Metadata, PortRef, SubscriptionId};
//!
//! let dataflow = DataflowId::from_u128(1);
//! let source: PortRef = "camera/image".parse()?;
//! let subscription = SubscriptionId::new(1);
//!
//! let mut taps = TapRegistry::new();
//! taps.enable(dataflow);
//! taps.subscribe(subscription, dataflow, Some(source.clone()), None);
//!
//! let frames = taps.capture(dataflow, &source, &Metadata::default(), b"bytes", Instant::now());
//! assert_eq!(frames.len(), 1);
//! assert_eq!(frames[0].payload, b"bytes");
//! # Ok::<(), astrs_wire::IdError>(())
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use astrs_wire::{DataFrame, DataflowId, Metadata, PortRef, SubscriptionId};

/// One CLI subscriber's tap.
#[derive(Debug, Clone)]
pub struct TapSubscription {
    /// The subscription this frame stream belongs to.
    pub subscription: SubscriptionId,
    /// The dataflow being tapped.
    pub dataflow: DataflowId,
    /// The port being tapped, or [`None`] for every port of the dataflow.
    pub source: Option<PortRef>,
    /// The shortest interval between delivered frames, when a rate was asked
    /// for.
    pub min_interval: Option<Duration>,
    /// When a frame was last delivered.
    last_sent: Option<Instant>,
    /// How many frames were delivered.
    delivered: u64,
    /// How many were dropped by rate shaping.
    dropped: u64,
}

impl TapSubscription {
    /// Whether this subscription covers `source` in `dataflow`.
    #[must_use]
    pub fn covers(&self, dataflow: DataflowId, source: &PortRef) -> bool {
        self.dataflow == dataflow && self.source.as_ref().is_none_or(|wanted| wanted == source)
    }

    /// Whether a frame arriving at `now` is inside the shaping interval.
    #[must_use]
    pub fn is_too_soon(&self, now: Instant) -> bool {
        match (self.min_interval, self.last_sent) {
            (Some(interval), Some(last)) => now.saturating_duration_since(last) < interval,
            _ => false,
        }
    }

    /// How many frames have been delivered.
    #[must_use]
    pub const fn delivered(&self) -> u64 {
        self.delivered
    }

    /// How many frames rate shaping discarded.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// Every debug tap this daemon serves.
#[derive(Debug, Default, Clone)]
pub struct TapRegistry {
    /// Dataflows whose manifest allows tapping (`debug: true`).
    enabled: BTreeSet<DataflowId>,
    /// One entry per CLI subscription.
    taps: BTreeMap<SubscriptionId, TapSubscription>,
}

impl TapRegistry {
    /// A registry with nothing enabled and nothing subscribed.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            enabled: BTreeSet::new(),
            taps: BTreeMap::new(),
        }
    }

    /// Allows tapping one dataflow (§13 `debug: true`).
    pub fn enable(&mut self, dataflow: DataflowId) -> bool {
        self.enabled.insert(dataflow)
    }

    /// Forbids tapping one dataflow, dropping its subscriptions.
    pub fn disable(&mut self, dataflow: DataflowId) -> usize {
        self.enabled.remove(&dataflow);
        let before = self.taps.len();
        self.taps.retain(|_, tap| tap.dataflow != dataflow);
        before - self.taps.len()
    }

    /// Whether a dataflow may be tapped.
    #[must_use]
    pub fn is_enabled(&self, dataflow: DataflowId) -> bool {
        self.enabled.contains(&dataflow)
    }

    /// Adds a subscription, if its dataflow was [`Self::enable`]d.
    ///
    /// `max_hz` shapes the delivered rate; `None` delivers every frame.
    /// Returns whether the subscription was added — `false` for a dataflow
    /// this registry was never told to enable.
    ///
    /// This is a real gate against whatever [`Self::enable`]/[`Self::disable`]
    /// last recorded for `dataflow`, but it is not where the manifest's
    /// `debug: true` gets enforced: this daemon never sees the manifest (a
    /// coordinator `Spawn` carries an already-expanded `NodeSpawnSpec`, no
    /// `debug` field on the wire at all), so that check happens exactly
    /// once, upstream, in `astrs-coordinator`'s
    /// `handlers::logs::topic_subscribe` — and that same function's own
    /// caller (`Daemon::handle_coordinator_frame`'s `TopicTapStart` arm)
    /// calls [`Self::enable`] unconditionally immediately before ever
    /// reaching here, so in the one wired call path this gate always
    /// passes. What it actually guards is a direct call to this method from
    /// anywhere else in this daemon that skipped that arm, and it is what
    /// lets [`Self::taps_output`] answer "no" again once
    /// [`Self::disable`] runs.
    pub fn subscribe(
        &mut self,
        subscription: SubscriptionId,
        dataflow: DataflowId,
        source: Option<PortRef>,
        max_hz: Option<f64>,
    ) -> bool {
        if !self.is_enabled(dataflow) {
            return false;
        }
        let min_interval = max_hz
            .filter(|hz| hz.is_finite() && *hz > 0.0)
            .map(|hz| Duration::from_secs_f64(1.0 / hz));
        self.taps.insert(
            subscription,
            TapSubscription {
                subscription,
                dataflow,
                source,
                min_interval,
                last_sent: None,
                delivered: 0,
                dropped: 0,
            },
        );
        true
    }

    /// Removes a subscription.
    pub fn unsubscribe(&mut self, subscription: SubscriptionId) -> bool {
        self.taps.remove(&subscription).is_some()
    }

    /// One subscription.
    #[must_use]
    pub fn subscription(&self, subscription: SubscriptionId) -> Option<&TapSubscription> {
        self.taps.get(&subscription)
    }

    /// Whether anything taps `source` in `dataflow`.
    ///
    /// The question [`crate::shm::ShmPolicy`] asks before offering an upgrade:
    /// an output the daemon must keep seeing cannot leave the daemon path.
    #[must_use]
    pub fn taps_output(&self, dataflow: DataflowId, source: &PortRef) -> bool {
        self.taps.values().any(|tap| tap.covers(dataflow, source))
    }

    /// Whether anything taps any output of `dataflow`.
    #[must_use]
    pub fn taps_dataflow(&self, dataflow: DataflowId) -> bool {
        self.taps.values().any(|tap| tap.dataflow == dataflow)
    }

    /// Copies one published message to every subscription that wants it.
    ///
    /// Returns one [`DataFrame`] per subscription that is due a frame; the
    /// rest are counted as dropped. The payload is cloned per subscriber
    /// because each frame is owned by the sink it goes to — which is the price
    /// of a tap, and the reason a tap is opt-in.
    pub fn capture(
        &mut self,
        dataflow: DataflowId,
        source: &PortRef,
        metadata: &Metadata,
        payload: &[u8],
        now: Instant,
    ) -> Vec<DataFrame> {
        let mut frames = Vec::new();
        for tap in self.taps.values_mut() {
            if !tap.covers(dataflow, source) {
                continue;
            }
            if tap.is_too_soon(now) {
                tap.dropped = tap.dropped.saturating_add(1);
                continue;
            }
            tap.last_sent = Some(now);
            tap.delivered = tap.delivered.saturating_add(1);
            frames.push(DataFrame::new(
                tap.subscription,
                dataflow,
                source.clone(),
                metadata.clone(),
                payload.to_vec(),
            ));
        }
        frames
    }

    /// How many frames one subscription has dropped to rate shaping.
    #[must_use]
    pub fn dropped(&self, subscription: SubscriptionId) -> u64 {
        self.taps
            .get(&subscription)
            .map_or(0, TapSubscription::dropped)
    }

    /// How many frames one subscription has been sent.
    #[must_use]
    pub fn delivered(&self, subscription: SubscriptionId) -> u64 {
        self.taps
            .get(&subscription)
            .map_or(0, TapSubscription::delivered)
    }

    /// How many frames every subscription has dropped.
    #[must_use]
    pub fn dropped_total(&self) -> u64 {
        self.taps.values().map(TapSubscription::dropped).sum()
    }

    /// How many subscriptions there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.taps.len()
    }

    /// Whether nothing is subscribed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.taps.is_empty()
    }

    /// Every subscription, in id order.
    pub fn subscriptions(&self) -> impl Iterator<Item = &TapSubscription> {
        self.taps.values()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A fresh subscription handle.
    fn next_subscription() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn dataflow() -> DataflowId {
        DataflowId::from_u128(1)
    }

    fn source(node: &str) -> PortRef {
        PortRef::from_parts(node, "image").expect("a port")
    }

    fn registry() -> (TapRegistry, SubscriptionId) {
        let mut taps = TapRegistry::new();
        taps.enable(dataflow());
        let subscription = SubscriptionId::new(next_subscription());
        assert!(taps.subscribe(subscription, dataflow(), Some(source("camera")), None));
        (taps, subscription)
    }

    #[test]
    fn a_fresh_registry_taps_nothing() {
        let registry = TapRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(!registry.is_enabled(dataflow()));
        assert!(!registry.taps_dataflow(dataflow()));
        assert!(!registry.taps_output(dataflow(), &source("camera")));
        assert_eq!(registry.dropped_total(), 0);
    }

    #[test]
    fn a_dataflow_that_never_opted_in_cannot_be_tapped() {
        let mut taps = TapRegistry::new();
        assert!(!taps.subscribe(
            SubscriptionId::new(next_subscription()),
            dataflow(),
            None,
            None
        ));
        assert!(taps.is_empty());
    }

    #[test]
    fn enabling_is_idempotent() {
        let mut taps = TapRegistry::new();
        assert!(taps.enable(dataflow()));
        assert!(!taps.enable(dataflow()));
        assert!(taps.is_enabled(dataflow()));
    }

    #[test]
    fn a_tap_copies_the_message_it_covers() {
        let (mut taps, subscription) = registry();
        let mut metadata = Metadata::default();
        metadata.insert("seq", 4i64).expect("a legal key");

        let frames = taps.capture(
            dataflow(),
            &source("camera"),
            &metadata,
            b"an arrow batch",
            Instant::now(),
        );
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].subscription, subscription);
        assert_eq!(frames[0].source, source("camera"));
        assert_eq!(frames[0].payload, b"an arrow batch");
        assert_eq!(frames[0].payload_len(), 14);
        assert_eq!(taps.delivered(subscription), 1);
    }

    #[test]
    fn a_tap_ignores_a_port_it_does_not_cover() {
        let (mut taps, _) = registry();
        assert!(
            taps.capture(
                dataflow(),
                &source("lidar"),
                &Metadata::default(),
                b"x",
                Instant::now()
            )
            .is_empty()
        );
        assert!(taps.taps_output(dataflow(), &source("camera")));
        assert!(!taps.taps_output(dataflow(), &source("lidar")));
    }

    #[test]
    fn a_wildcard_tap_covers_every_port_of_its_dataflow() {
        let mut taps = TapRegistry::new();
        taps.enable(dataflow());
        taps.subscribe(
            SubscriptionId::new(next_subscription()),
            dataflow(),
            None,
            None,
        );

        assert!(taps.taps_output(dataflow(), &source("camera")));
        assert!(taps.taps_output(dataflow(), &source("lidar")));
        assert!(!taps.taps_output(DataflowId::from_u128(2), &source("camera")));
    }

    #[test]
    fn a_tap_never_crosses_dataflows() {
        let (mut taps, _) = registry();
        assert!(
            taps.capture(
                DataflowId::from_u128(9),
                &source("camera"),
                &Metadata::default(),
                b"x",
                Instant::now()
            )
            .is_empty()
        );
    }

    #[test]
    fn rate_shaping_drops_rather_than_queues() {
        let mut taps = TapRegistry::new();
        taps.enable(dataflow());
        let subscription = SubscriptionId::new(next_subscription());
        taps.subscribe(subscription, dataflow(), Some(source("camera")), Some(2.0));

        let start = Instant::now();
        assert_eq!(
            taps.capture(
                dataflow(),
                &source("camera"),
                &Metadata::default(),
                b"a",
                start
            )
            .len(),
            1
        );
        assert!(
            taps.capture(
                dataflow(),
                &source("camera"),
                &Metadata::default(),
                b"b",
                start + Duration::from_millis(100)
            )
            .is_empty(),
            "inside the 500 ms interval"
        );
        assert_eq!(taps.dropped(subscription), 1);

        assert_eq!(
            taps.capture(
                dataflow(),
                &source("camera"),
                &Metadata::default(),
                b"c",
                start + Duration::from_millis(600)
            )
            .len(),
            1
        );
        assert_eq!(taps.delivered(subscription), 2);
        assert_eq!(taps.dropped_total(), 1);
    }

    #[test]
    fn an_impossible_rate_shapes_nothing() {
        let mut taps = TapRegistry::new();
        taps.enable(dataflow());
        let subscription = SubscriptionId::new(next_subscription());
        taps.subscribe(subscription, dataflow(), None, Some(0.0));
        assert!(
            taps.subscription(subscription)
                .expect("present")
                .min_interval
                .is_none()
        );

        taps.subscribe(subscription, dataflow(), None, Some(f64::NAN));
        assert!(
            taps.subscription(subscription)
                .expect("present")
                .min_interval
                .is_none()
        );
    }

    #[test]
    fn several_subscribers_each_get_a_copy() {
        let mut taps = TapRegistry::new();
        taps.enable(dataflow());
        let first = SubscriptionId::new(next_subscription());
        let second = SubscriptionId::new(next_subscription());
        taps.subscribe(first, dataflow(), None, None);
        taps.subscribe(second, dataflow(), Some(source("camera")), None);

        let frames = taps.capture(
            dataflow(),
            &source("camera"),
            &Metadata::default(),
            b"x",
            Instant::now(),
        );
        assert_eq!(frames.len(), 2);
        assert_eq!(taps.subscriptions().count(), 2);
    }

    #[test]
    fn unsubscribing_removes_one_tap() {
        let (mut taps, subscription) = registry();
        assert!(taps.unsubscribe(subscription));
        assert!(!taps.unsubscribe(subscription));
        assert!(taps.is_empty());
        assert_eq!(taps.dropped(subscription), 0);
        assert_eq!(taps.delivered(subscription), 0);
    }

    #[test]
    fn disabling_a_dataflow_drops_its_subscriptions() {
        let (mut taps, _) = registry();
        let other = DataflowId::from_u128(2);
        taps.enable(other);
        taps.subscribe(SubscriptionId::new(next_subscription()), other, None, None);

        assert_eq!(taps.disable(dataflow()), 1);
        assert!(!taps.is_enabled(dataflow()));
        assert_eq!(taps.len(), 1, "the other dataflow's tap survives");
    }

    #[test]
    fn a_second_subscribe_replaces_the_first() {
        let (mut taps, subscription) = registry();
        taps.subscribe(subscription, dataflow(), None, Some(10.0));
        assert_eq!(taps.len(), 1);
        assert!(
            taps.subscription(subscription)
                .expect("present")
                .source
                .is_none()
        );
    }
}
