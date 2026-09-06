//! [`MetricFamily`] — a named metric with a bounded set of label-value
//! series, all sharing one [`LabelInterner`].

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use astrs_wire::{MetricPoint, MetricValue};

use crate::metrics::counter::Counter;
use crate::metrics::gauge::Gauge;
use crate::metrics::histogram::Histogram;
use crate::metrics::interner::LabelInterner;

/// The label value substituted for every declared label key on the one
/// synthetic [`MetricPoint`] that represents everything routed to the
/// overflow series (see [`MetricFamily::snapshot_into`]).
///
/// Keeping every point of a family sharing the same label *key* set
/// (rather than emitting an unlabeled point only for the overflow case)
/// matches how Prometheus-shaped consumers expect one metric name to
/// behave, and makes the overflow row self-explanatory in raw output —
/// `{node="__label_overflow__"}` reads as what it is.
pub const OVERFLOW_LABEL_VALUE: &str = "__label_overflow__";

/// What a metric handle needs to be able to turn itself into a wire
/// [`MetricValue`]. Implemented for [`Counter`], [`Gauge`] and
/// [`Histogram`] so [`MetricFamily`] and [`crate::metrics::MetricRegistry`]
/// can snapshot any of the three uniformly.
pub trait MetricSeries {
    /// The current reading, in the wire's value shape.
    fn to_metric_value(&self) -> MetricValue;
}

impl MetricSeries for Counter {
    fn to_metric_value(&self) -> MetricValue {
        MetricValue::Counter(self.get())
    }
}

impl MetricSeries for Gauge {
    fn to_metric_value(&self) -> MetricValue {
        MetricValue::Gauge(self.get())
    }
}

impl MetricSeries for Histogram {
    fn to_metric_value(&self) -> MetricValue {
        let (count, sum, buckets) = self.snapshot();
        MetricValue::Histogram {
            count,
            sum,
            buckets,
        }
    }
}

/// A named metric, pre-registered once, with a bounded set of
/// label-value series.
///
/// A "plain" (unlabelled) metric is not a special case: it is a family
/// with an empty `label_names` and a capacity of exactly one series (see
/// [`crate::metrics::MetricRegistry::register_counter`] and friends), so the
/// bounded-cardinality machinery is exercised uniformly everywhere.
///
/// # Examples
///
/// ```
/// use astrs_telemetry::metrics::{Counter, MetricFamily};
///
/// let family = MetricFamily::new("frames_total", &["node"], 4, Counter::new);
/// let camera = family.get_or_create(&["camera"]);
/// let lidar = family.get_or_create(&["lidar"]);
/// camera.inc_by(3);
/// lidar.inc();
///
/// // Resolving the same labels again returns the same handle.
/// assert_eq!(family.get_or_create(&["camera"]).get(), 3);
/// assert_eq!(lidar.get(), 1);
/// ```
#[derive(Debug)]
pub struct MetricFamily<M> {
    name: &'static str,
    label_names: &'static [&'static str],
    interner: LabelInterner,
    /// Length `interner.capacity() + 1`; the last slot is the shared
    /// overflow series.
    series: Vec<Arc<M>>,
    /// How many [`MetricFamily::get_or_create`] calls passed the wrong
    /// number of label values. Distinct from
    /// [`LabelInterner::overflow_total`] (cardinality exhaustion): this
    /// counts a caller-side usage mistake instead.
    arity_mismatch_total: AtomicU64,
}

impl<M> MetricFamily<M> {
    /// Builds a family, eagerly constructing `capacity + 1` series (the
    /// declared capacity plus the shared overflow slot) via `make`.
    ///
    /// Eager construction means every series a hot path might touch
    /// already exists as of this call — [`MetricFamily::get_or_create`]
    /// never allocates a new [`Counter`]/[`Gauge`]/[`Histogram`], only
    /// resolves an index into `series`.
    #[must_use]
    pub fn new(
        name: &'static str,
        label_names: &'static [&'static str],
        capacity: u32,
        make: impl Fn() -> M,
    ) -> Self {
        let series = (0..=capacity).map(|_| Arc::new(make())).collect();
        Self {
            name,
            label_names,
            interner: LabelInterner::new(capacity),
            series,
            arity_mismatch_total: AtomicU64::new(0),
        }
    }

    /// The metric name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The declared label keys, in the order [`MetricFamily::get_or_create`]
    /// expects their values.
    #[must_use]
    pub const fn label_names(&self) -> &'static [&'static str] {
        self.label_names
    }

    /// Resolves `values` (one per [`MetricFamily::label_names`], in
    /// order) to a metric handle, creating no new state — every possible
    /// handle already exists as of [`MetricFamily::new`].
    ///
    /// A `values` slice of the wrong length is routed to the shared
    /// overflow series and counted in
    /// [`MetricFamily::arity_mismatch_total`] rather than panicking: a
    /// mislabeled call site is a bug this crate surfaces as data, not as
    /// a crash in whatever robot process called it (blueprint §3.5,
    /// crash-first design is about resource cleanup, not about a metrics
    /// call being allowed to take the process down).
    #[must_use]
    pub fn get_or_create(&self, values: &[&str]) -> Arc<M> {
        if values.len() != self.label_names.len() {
            self.arity_mismatch_total.fetch_add(1, Ordering::Relaxed);
            return Arc::clone(&self.series[self.overflow_index()]);
        }
        let id = self.interner.intern(values);
        Arc::clone(&self.series[id as usize])
    }

    /// The index of the shared overflow series within
    /// [`MetricFamily::series`]-equivalent storage: always the last one.
    fn overflow_index(&self) -> usize {
        self.series.len() - 1
    }

    /// How many distinct label-value combinations were routed to the
    /// shared overflow series because [`LabelInterner::capacity`] was
    /// already exhausted.
    #[must_use]
    pub fn overflow_total(&self) -> u64 {
        self.interner.overflow_total()
    }

    /// How many [`MetricFamily::get_or_create`] calls passed the wrong
    /// number of label values.
    #[must_use]
    pub fn arity_mismatch_total(&self) -> u64 {
        self.arity_mismatch_total.load(Ordering::Relaxed)
    }
}

impl<M: MetricSeries> MetricFamily<M> {
    /// Appends one [`MetricPoint`] per interned label combination, plus
    /// one synthetic point for the shared overflow series (labeled with
    /// [`OVERFLOW_LABEL_VALUE`] under every declared key), to `out`.
    ///
    /// The overflow point is always emitted, even when nothing has
    /// overflowed (in which case it simply reads zero/empty) — a metrics
    /// consumer sees a stable set of series for this family across
    /// scrapes once cardinality has actually been exceeded at least once
    /// — but is omitted entirely otherwise, which matters most for an
    /// unlabelled metric (`label_names: &[]`, `capacity: 1`, as built by
    /// [`crate::metrics::MetricRegistry::register_counter`] and its
    /// siblings): such a family's single call shape can never overflow,
    /// so unconditionally emitting a second, always-zero point would
    /// silently double every "plain" counter/gauge/histogram in every
    /// snapshot.
    pub fn snapshot_into(&self, out: &mut Vec<MetricPoint>) {
        for (values, id) in self.interner.entries() {
            let mut labels = std::collections::BTreeMap::new();
            for (key, value) in self.label_names.iter().zip(values.iter()) {
                labels.insert((*key).to_owned(), value.clone());
            }
            out.push(MetricPoint {
                name: self.name.to_owned(),
                value: self.series[id as usize].to_metric_value(),
                labels,
            });
        }
        if self.overflow_total() > 0 || self.arity_mismatch_total() > 0 {
            let overflow_labels = self
                .label_names
                .iter()
                .map(|key| ((*key).to_owned(), OVERFLOW_LABEL_VALUE.to_owned()))
                .collect();
            out.push(MetricPoint {
                name: self.name.to_owned(),
                value: self.series[self.overflow_index()].to_metric_value(),
                labels: overflow_labels,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn resolving_the_same_labels_returns_the_same_handle() {
        let family = MetricFamily::new("frames_total", &["node"], 4, Counter::new);
        family.get_or_create(&["camera"]).inc();
        family.get_or_create(&["camera"]).inc();
        assert_eq!(family.get_or_create(&["camera"]).get(), 2);
    }

    #[test]
    fn distinct_labels_are_independent() {
        let family = MetricFamily::new("frames_total", &["node"], 4, Counter::new);
        family.get_or_create(&["camera"]).inc();
        family.get_or_create(&["lidar"]).inc_by(5);
        assert_eq!(family.get_or_create(&["camera"]).get(), 1);
        assert_eq!(family.get_or_create(&["lidar"]).get(), 5);
    }

    #[test]
    fn wrong_arity_routes_to_overflow_and_is_counted() {
        let family = MetricFamily::new("x", &["node", "input"], 4, Counter::new);
        family.get_or_create(&["only-one"]).inc();
        family.get_or_create(&[]).inc();
        assert_eq!(family.arity_mismatch_total(), 2);
    }

    #[test]
    fn cardinality_overflow_shares_one_series() {
        let family = MetricFamily::new("x", &["node"], 1, Counter::new);
        family.get_or_create(&["a"]).inc();
        family.get_or_create(&["b"]).inc(); // overflow: capacity is 1
        family.get_or_create(&["c"]).inc(); // overflow too
        assert_eq!(family.get_or_create(&["a"]).get(), 1);
        assert_eq!(family.overflow_total(), 2);
    }

    #[test]
    fn snapshot_emits_one_point_per_series_plus_the_overflow_point() {
        let family = MetricFamily::new("frames_total", &["node"], 2, Counter::new);
        family.get_or_create(&["camera"]).inc_by(3);
        family.get_or_create(&["lidar"]).inc_by(7);
        family.get_or_create(&["overflow-me"]).inc(); // capacity 2, third combo overflows

        let mut points = Vec::new();
        family.snapshot_into(&mut points);
        assert_eq!(points.len(), 3, "camera, lidar, and the overflow point");

        let overflow = points
            .iter()
            .find(|p| p.label("node") == Some(OVERFLOW_LABEL_VALUE))
            .expect("overflow point present");
        assert_eq!(overflow.value, MetricValue::Counter(1));

        let camera = points
            .iter()
            .find(|p| p.label("node") == Some("camera"))
            .expect("camera point present");
        assert_eq!(camera.value, MetricValue::Counter(3));
    }

    #[test]
    fn a_family_with_no_labels_behaves_as_one_plain_series() {
        let family = MetricFamily::new("uptime_total", &[], 1, Counter::new);
        family.get_or_create(&[]).inc_by(42);
        let mut points = Vec::new();
        family.snapshot_into(&mut points);
        // Exactly one point: the single label-less series never
        // overflows (there is only ever one possible call shape), so no
        // overflow point is emitted alongside it.
        assert_eq!(points.len(), 1);
        assert!(
            points
                .iter()
                .any(|p| p.value == MetricValue::Counter(42) && p.labels.is_empty())
        );
    }
}
