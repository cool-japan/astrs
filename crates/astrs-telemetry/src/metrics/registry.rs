//! [`MetricRegistry`] — the process-global home for every AstRS metric.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use astrs_time::HlcTimestamp;
use astrs_wire::MetricBatch;

use crate::metrics::counter::Counter;
use crate::metrics::family::MetricFamily;
use crate::metrics::gauge::Gauge;
use crate::metrics::histogram::Histogram;
use crate::metrics::interner::DEFAULT_MAX_SERIES;

/// A lightweight, process-global registry of [`Counter`], [`Gauge`] and
/// [`Histogram`] families.
///
/// Blueprint §13: metrics are pre-registered once (typically at node or
/// daemon startup) and updated via atomics from then on — the returned
/// handles ([`Counter`], [`Gauge`], `Arc<Histogram>`) are exactly that:
/// cheap to hold, allocation-free to update. Registration itself
/// (`register_*`) takes a lock and may allocate; it is meant to run once
/// per distinct metric, not per message.
///
/// # Examples
///
/// ```
/// use astrs_telemetry::metrics::MetricRegistry;
/// use astrs_time::HlcTimestamp;
///
/// let registry = MetricRegistry::new();
/// let frames = registry.register_counter("frames_total");
/// frames.inc();
/// frames.inc();
///
/// let batch = registry.snapshot(HlcTimestamp::new(1, 0), "astrs_daemon");
/// assert_eq!(batch.points.len(), 1);
/// assert_eq!(batch.points[0].value.as_f64(), 2.0);
/// ```
#[derive(Debug, Default)]
pub struct MetricRegistry {
    counters: RwLock<HashMap<&'static str, Arc<MetricFamily<Counter>>>>,
    gauges: RwLock<HashMap<&'static str, Arc<MetricFamily<Gauge>>>>,
    histograms: RwLock<HashMap<&'static str, Arc<MetricFamily<Histogram>>>>,
}

impl MetricRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers (or returns the already-registered) unlabelled counter
    /// named `name`.
    ///
    /// Equivalent to `self.register_counter_family(name, &[],
    /// 1).get_or_create(&[])` — an unlabelled metric is a family with no
    /// label keys and room for exactly one series.
    #[must_use]
    pub fn register_counter(&self, name: &'static str) -> Arc<Counter> {
        self.register_counter_family(name, &[], 1)
            .get_or_create(&[])
    }

    /// Registers (or returns the already-registered) unlabelled gauge
    /// named `name`.
    #[must_use]
    pub fn register_gauge(&self, name: &'static str) -> Arc<Gauge> {
        self.register_gauge_family(name, &[], 1).get_or_create(&[])
    }

    /// Registers (or returns the already-registered) unlabelled histogram
    /// named `name` with the given finite bucket bounds.
    #[must_use]
    pub fn register_histogram(&self, name: &'static str, bounds: Vec<f64>) -> Arc<Histogram> {
        self.register_histogram_family(name, &[], 1, bounds)
            .get_or_create(&[])
    }

    /// Registers (or returns the already-registered) counter family named
    /// `name` with the given label keys and cardinality cap.
    ///
    /// Idempotent by name: a second call with the same `name` returns the
    /// family created by the *first* call, ignoring `label_names` and
    /// `capacity` on that second call — re-registering the same name with
    /// a different shape is a programming error this crate surfaces by
    /// silently keeping the original definition rather than by panicking
    /// or corrupting the first family's state.
    #[must_use]
    pub fn register_counter_family(
        &self,
        name: &'static str,
        label_names: &'static [&'static str],
        capacity: u32,
    ) -> Arc<MetricFamily<Counter>> {
        register_family(&self.counters, name, || {
            MetricFamily::new(name, label_names, capacity, Counter::new)
        })
    }

    /// Registers (or returns the already-registered) gauge family. See
    /// [`MetricRegistry::register_counter_family`] for the idempotence
    /// contract.
    #[must_use]
    pub fn register_gauge_family(
        &self,
        name: &'static str,
        label_names: &'static [&'static str],
        capacity: u32,
    ) -> Arc<MetricFamily<Gauge>> {
        register_family(&self.gauges, name, || {
            MetricFamily::new(name, label_names, capacity, Gauge::new)
        })
    }

    /// Registers (or returns the already-registered) histogram family.
    /// See [`MetricRegistry::register_counter_family`] for the
    /// idempotence contract.
    #[must_use]
    pub fn register_histogram_family(
        &self,
        name: &'static str,
        label_names: &'static [&'static str],
        capacity: u32,
        bounds: Vec<f64>,
    ) -> Arc<MetricFamily<Histogram>> {
        register_family(&self.histograms, name, || {
            MetricFamily::new(name, label_names, capacity, move || {
                Histogram::new(bounds.clone())
            })
        })
    }

    /// The default cardinality cap ([`DEFAULT_MAX_SERIES`]) used by
    /// nothing in this module directly, but re-exported here so a caller
    /// choosing a capacity for `register_*_family` has a documented
    /// starting point instead of picking a number out of the air.
    #[must_use]
    pub const fn default_family_capacity() -> u32 {
        DEFAULT_MAX_SERIES
    }

    /// Snapshots every registered family into one [`MetricBatch`],
    /// stamped at `timestamp` and attributed to `scope` (blueprint §13's
    /// "instrumentation scope — usually the crate that registered the
    /// metrics", e.g. `"astrs_daemon"`).
    ///
    /// Not itself allocation-free (a snapshot walks every family and
    /// builds a fresh `Vec<MetricPoint>`) — this runs at the export
    /// cadence (every few seconds, driven by
    /// [`crate::export::OtlpExporter`] or a daemon's own sampling loop),
    /// not on any per-message path.
    #[must_use]
    pub fn snapshot(&self, timestamp: HlcTimestamp, scope: impl Into<String>) -> MetricBatch {
        let mut points = Vec::new();
        if let Ok(counters) = self.counters.read() {
            for family in counters.values() {
                family.snapshot_into(&mut points);
            }
        }
        if let Ok(gauges) = self.gauges.read() {
            for family in gauges.values() {
                family.snapshot_into(&mut points);
            }
        }
        if let Ok(histograms) = self.histograms.read() {
            for family in histograms.values() {
                family.snapshot_into(&mut points);
            }
        }
        // `HashMap` iteration order is unspecified and varies between
        // runs; sorting here (name, then labels) makes the snapshot
        // deterministic regardless of registration or hashing order,
        // which matters for byte-stable OTLP export payloads and for
        // this crate's own golden-file tests.
        points.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.labels.cmp(&b.labels)));
        MetricBatch {
            timestamp,
            scope: scope.into(),
            dataflow: None,
            points,
        }
    }
}

/// Shared idempotent-lookup-or-insert logic for the three `register_*_family`
/// methods: take the write lock only on a miss, so the common
/// already-registered case only needs a read lock.
fn register_family<M>(
    map: &RwLock<HashMap<&'static str, Arc<MetricFamily<M>>>>,
    name: &'static str,
    build: impl FnOnce() -> MetricFamily<M>,
) -> Arc<MetricFamily<M>> {
    if let Ok(existing) = map.read()
        && let Some(family) = existing.get(name)
    {
        return Arc::clone(family);
    }
    let mut guard = match map.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    Arc::clone(guard.entry(name).or_insert_with(|| Arc::new(build())))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::MetricValue;

    #[test]
    fn plain_counter_round_trips_through_a_snapshot() {
        let registry = MetricRegistry::new();
        let counter = registry.register_counter("frames_total");
        counter.inc_by(5);

        let batch = registry.snapshot(HlcTimestamp::new(1, 0), "test");
        assert_eq!(batch.timestamp, HlcTimestamp::new(1, 0));
        assert_eq!(batch.scope, "test");
        assert_eq!(batch.points.len(), 1);
        assert_eq!(batch.points[0].name, "frames_total");
        assert_eq!(batch.points[0].value, MetricValue::Counter(5));
    }

    #[test]
    fn registering_the_same_name_twice_returns_the_same_handle() {
        let registry = MetricRegistry::new();
        let a = registry.register_counter("x");
        let b = registry.register_counter("x");
        a.inc();
        assert_eq!(b.get(), 1, "must be the same underlying atomic");
    }

    #[test]
    fn re_registering_with_a_different_shape_keeps_the_first_definition() {
        let registry = MetricRegistry::new();
        let first = registry.register_counter_family("x", &["node"], 4);
        let second = registry.register_counter_family("x", &["node", "input"], 8);
        assert_eq!(first.label_names(), second.label_names());
        assert_eq!(second.label_names(), &["node"]);
    }

    #[test]
    fn a_gauge_and_a_histogram_both_snapshot() {
        let registry = MetricRegistry::new();
        registry.register_gauge("queue_depth").set(3.0);
        registry
            .register_histogram("latency_seconds", vec![0.1, 1.0])
            .observe(0.5);

        let batch = registry.snapshot(HlcTimestamp::EPOCH, "s");
        assert_eq!(batch.points.len(), 2);
        assert!(
            batch
                .points
                .iter()
                .any(|p| p.name == "queue_depth" && p.value == MetricValue::Gauge(3.0))
        );
        assert!(batch.points.iter().any(|p| p.name == "latency_seconds"
            && matches!(p.value, MetricValue::Histogram { count: 1, .. })));
    }

    #[test]
    fn labelled_families_within_capacity_snapshot_with_no_overflow_point() {
        let registry = MetricRegistry::new();
        let family = registry.register_counter_family("io_bytes_total", &["direction"], 4);
        family.get_or_create(&["rx"]).inc_by(10);
        family.get_or_create(&["tx"]).inc_by(20);

        let batch = registry.snapshot(HlcTimestamp::EPOCH, "s");
        // Two real series, capacity 4: nothing overflowed, so no
        // synthetic overflow point is emitted alongside them.
        assert_eq!(batch.points.len(), 2);
    }

    #[test]
    fn labelled_families_beyond_capacity_snapshot_with_an_overflow_point() {
        let registry = MetricRegistry::new();
        let family = registry.register_counter_family("io_bytes_total", &["direction"], 2);
        family.get_or_create(&["rx"]).inc_by(10);
        family.get_or_create(&["tx"]).inc_by(20);
        family.get_or_create(&["broadcast"]).inc_by(1); // capacity 2: this overflows.

        let batch = registry.snapshot(HlcTimestamp::EPOCH, "s");
        // Two real series + one overflow point for this one family.
        assert_eq!(batch.points.len(), 3);
    }

    #[test]
    fn an_empty_registry_snapshots_to_no_points() {
        let registry = MetricRegistry::new();
        let batch = registry.snapshot(HlcTimestamp::EPOCH, "s");
        assert!(batch.points.is_empty());
        assert!(batch.within_limits());
    }

    #[test]
    fn default_family_capacity_matches_the_documented_constant() {
        assert_eq!(
            MetricRegistry::default_family_capacity(),
            DEFAULT_MAX_SERIES
        );
    }
}
