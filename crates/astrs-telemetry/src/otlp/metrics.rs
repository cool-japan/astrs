//! Maps [`astrs_wire::MetricBatch`] onto the OTLP
//! `ExportMetricsServiceRequest` JSON shape.
//!
//! # Cumulative vs. per-bucket histogram counts
//!
//! [`astrs_wire::HistogramBucket::cumulative_count`] is, as the name
//! says, cumulative (`crate::metrics::Histogram::snapshot`'s doc example
//! shows this explicitly). OTLP's `HistogramDataPoint.bucketCounts` is
//! the opposite convention: **per-bucket**, non-cumulative counts, one
//! more entry than `explicitBounds` (the implicit `+Inf` bucket is the
//! last one). `histogram_data_point` (crate-private) does the subtraction.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use astrs_wire::{MetricBatch, MetricPoint, MetricValue};

use crate::otlp::common::{KeyValue, Resource, SCOPE_NAME, SCOPE_VERSION, Scope};

/// `AGGREGATION_TEMPORALITY_CUMULATIVE`: every [`crate::metrics::Counter`]
/// and [`crate::metrics::Histogram`] in this crate accumulates since
/// process start and never resets, which is exactly what "cumulative"
/// means in OTLP.
const AGGREGATION_TEMPORALITY_CUMULATIVE: u32 = 2;

/// The top-level OTLP/HTTP metrics export request: `POST /v1/metrics` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportMetricsServiceRequest {
    /// One entry per distinct resource — this crate always emits exactly
    /// one.
    #[serde(rename = "resourceMetrics")]
    pub resource_metrics: Vec<ResourceMetrics>,
}

/// Every metric produced by one resource (process).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceMetrics {
    /// The resource (process/service) these metrics describe.
    pub resource: Resource,
    /// Metrics grouped by instrumentation scope; this crate always emits
    /// exactly one scope.
    #[serde(rename = "scopeMetrics")]
    pub scope_metrics: Vec<ScopeMetrics>,
}

/// Every metric produced by one instrumentation scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopeMetrics {
    /// The instrumentation scope.
    pub scope: Scope,
    /// One entry per distinct metric name.
    pub metrics: Vec<Metric>,
}

/// One named metric: exactly one of `sum`/`gauge`/`histogram` is present,
/// matching which [`astrs_wire::MetricValue`] variant every point sharing
/// this name carried.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Metric {
    /// The metric name.
    pub name: String,
    /// Present for points that were [`MetricValue::Counter`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sum: Option<Sum>,
    /// Present for points that were [`MetricValue::Gauge`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gauge: Option<Gauge>,
    /// Present for points that were [`MetricValue::Histogram`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub histogram: Option<Histogram>,
}

/// An OTLP `Sum` metric: one or more monotonic counter readings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sum {
    /// One reading per label combination.
    #[serde(rename = "dataPoints")]
    pub data_points: Vec<NumberDataPoint>,
    /// `AGGREGATION_TEMPORALITY_CUMULATIVE` for everything this crate
    /// produces — see the crate-private `AGGREGATION_TEMPORALITY_CUMULATIVE`.
    #[serde(rename = "aggregationTemporality")]
    pub aggregation_temporality: u32,
    /// Always `true`: every [`crate::metrics::Counter`] this crate
    /// exports only ever increases.
    #[serde(rename = "isMonotonic")]
    pub is_monotonic: bool,
}

/// An OTLP `Gauge` metric: one or more instantaneous readings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gauge {
    /// One reading per label combination.
    #[serde(rename = "dataPoints")]
    pub data_points: Vec<NumberDataPoint>,
}

/// One number data point: an integer (counter) or double (gauge)
/// reading, plus the labels that distinguish it from sibling points of
/// the same metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NumberDataPoint {
    /// The reading, for a counter (`Sum`) point.
    #[serde(rename = "asInt", skip_serializing_if = "Option::is_none")]
    pub as_int: Option<String>,
    /// The reading, for a gauge point.
    #[serde(rename = "asDouble", skip_serializing_if = "Option::is_none")]
    pub as_double: Option<f64>,
    /// Nanoseconds since the UNIX epoch, as a decimal string.
    #[serde(rename = "timeUnixNano")]
    pub time_unix_nano: String,
    /// This point's labels, distinguishing it from sibling points of the
    /// same metric.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<KeyValue>,
}

/// An OTLP `Histogram` metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Histogram {
    /// One reading per label combination.
    #[serde(rename = "dataPoints")]
    pub data_points: Vec<HistogramDataPoint>,
    /// `AGGREGATION_TEMPORALITY_CUMULATIVE` for everything this crate
    /// produces.
    #[serde(rename = "aggregationTemporality")]
    pub aggregation_temporality: u32,
}

/// One histogram data point, in OTLP's per-bucket (non-cumulative)
/// convention — see the module docs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HistogramDataPoint {
    /// The total number of observations, as a decimal string.
    pub count: String,
    /// The sum of every observed value.
    pub sum: f64,
    /// Per-bucket (non-cumulative) counts; one more entry than
    /// `explicit_bounds` (the implicit `+Inf` bucket is the last one).
    #[serde(rename = "bucketCounts")]
    pub bucket_counts: Vec<String>,
    /// The finite bucket upper bounds, ascending.
    #[serde(rename = "explicitBounds")]
    pub explicit_bounds: Vec<f64>,
    /// Nanoseconds since the UNIX epoch, as a decimal string.
    #[serde(rename = "timeUnixNano")]
    pub time_unix_nano: String,
    /// This point's labels, distinguishing it from sibling points of the
    /// same metric.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<KeyValue>,
}

/// Converts a [`MetricPoint`]'s labels to OTLP attributes, in key order
/// (the labels map is already a `BTreeMap`, so this is deterministic).
fn labels_to_attributes(labels: &BTreeMap<String, String>) -> Vec<KeyValue> {
    labels
        .iter()
        .map(|(k, v)| KeyValue::string(k.clone(), v.clone()))
        .collect()
}

/// Builds the OTLP histogram data point for a [`MetricValue::Histogram`]
/// point, converting `astrs-wire`'s cumulative bucket counts to OTLP's
/// per-bucket convention (see the module docs).
fn histogram_data_point(
    point: &MetricPoint,
    count: u64,
    sum: f64,
    buckets: &[astrs_wire::HistogramBucket],
    time_unix_nano: &str,
) -> HistogramDataPoint {
    let mut bucket_counts = Vec::with_capacity(buckets.len());
    let mut explicit_bounds = Vec::with_capacity(buckets.len().saturating_sub(1));
    let mut previous_cumulative = 0u64;
    let last_index = buckets.len().saturating_sub(1);
    for (index, bucket) in buckets.iter().enumerate() {
        let per_bucket = bucket.cumulative_count.saturating_sub(previous_cumulative);
        bucket_counts.push(per_bucket.to_string());
        previous_cumulative = bucket.cumulative_count;
        if index != last_index {
            explicit_bounds.push(bucket.upper_bound);
        }
    }
    HistogramDataPoint {
        count: count.to_string(),
        sum,
        bucket_counts,
        explicit_bounds,
        time_unix_nano: time_unix_nano.to_owned(),
        attributes: labels_to_attributes(&point.labels),
    }
}

/// Groups `batch.points` by metric name (sorted, for a deterministic
/// output regardless of the input order) and builds one OTLP `Metric`
/// per group.
///
/// Every point sharing a name is expected to carry the same
/// [`MetricValue`] variant (a family's series are all the same kind by
/// construction — see [`crate::metrics::MetricFamily`]); a point whose
/// variant does not match the first one seen for its name is dropped
/// rather than corrupting the group's shape, since OTLP has no way to
/// mix a `sum` and a `gauge` under one metric name.
#[must_use]
pub fn build_metrics_request(
    batch: &MetricBatch,
    resource: Resource,
) -> ExportMetricsServiceRequest {
    let time_unix_nano = batch.timestamp.physical_ns().to_string();
    let mut grouped: BTreeMap<&str, Vec<&MetricPoint>> = BTreeMap::new();
    for point in &batch.points {
        grouped.entry(point.name.as_str()).or_default().push(point);
    }

    let metrics = grouped
        .into_iter()
        .map(|(name, points)| build_metric(name, &points, &time_unix_nano))
        .collect();

    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource,
            scope_metrics: vec![ScopeMetrics {
                scope: Scope::new(SCOPE_NAME, SCOPE_VERSION),
                metrics,
            }],
        }],
    }
}

/// Builds one `Metric` from every point sharing `name`, dispatching on
/// the first point's [`MetricValue`] variant.
fn build_metric(name: &str, points: &[&MetricPoint], time_unix_nano: &str) -> Metric {
    let mut metric = Metric {
        name: name.to_owned(),
        ..Metric::default()
    };
    let Some(first) = points.first() else {
        return metric;
    };
    match &first.value {
        MetricValue::Counter(_) => {
            let data_points = points
                .iter()
                .filter_map(|point| match &point.value {
                    MetricValue::Counter(value) => Some(NumberDataPoint {
                        as_int: Some(value.to_string()),
                        time_unix_nano: time_unix_nano.to_owned(),
                        attributes: labels_to_attributes(&point.labels),
                        ..NumberDataPoint::default()
                    }),
                    _ => None,
                })
                .collect();
            metric.sum = Some(Sum {
                data_points,
                aggregation_temporality: AGGREGATION_TEMPORALITY_CUMULATIVE,
                is_monotonic: true,
            });
        }
        MetricValue::Gauge(_) => {
            let data_points = points
                .iter()
                .filter_map(|point| match &point.value {
                    MetricValue::Gauge(value) => Some(NumberDataPoint {
                        as_double: Some(*value),
                        time_unix_nano: time_unix_nano.to_owned(),
                        attributes: labels_to_attributes(&point.labels),
                        ..NumberDataPoint::default()
                    }),
                    _ => None,
                })
                .collect();
            metric.gauge = Some(Gauge { data_points });
        }
        MetricValue::Histogram { .. } => {
            let data_points = points
                .iter()
                .filter_map(|point| match &point.value {
                    MetricValue::Histogram {
                        count,
                        sum,
                        buckets,
                    } => Some(histogram_data_point(
                        point,
                        *count,
                        *sum,
                        buckets,
                        time_unix_nano,
                    )),
                    _ => None,
                })
                .collect();
            metric.histogram = Some(Histogram {
                data_points,
                aggregation_temporality: AGGREGATION_TEMPORALITY_CUMULATIVE,
            });
        }
        // `MetricValue` is `#[non_exhaustive]`: a variant this build does
        // not know about yet produces a `Metric` with just a name and no
        // populated shape, rather than failing the whole export.
        _ => {}
    }
    metric
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_time::HlcTimestamp;
    use astrs_wire::HistogramBucket;

    #[test]
    fn counter_becomes_a_monotonic_sum() {
        let batch = MetricBatch::new(HlcTimestamp::new(5, 0), "s")
            .with_point(MetricPoint::counter("frames_total", 7));
        let request = build_metrics_request(&batch, Resource::default());
        let metric = &request.resource_metrics[0].scope_metrics[0].metrics[0];
        assert_eq!(metric.name, "frames_total");
        let sum = metric.sum.as_ref().unwrap();
        assert!(sum.is_monotonic);
        assert_eq!(
            sum.aggregation_temporality,
            AGGREGATION_TEMPORALITY_CUMULATIVE
        );
        assert_eq!(sum.data_points[0].as_int.as_deref(), Some("7"));
        assert_eq!(sum.data_points[0].time_unix_nano, "5");
    }

    #[test]
    fn gauge_becomes_a_gauge_with_a_double() {
        let batch = MetricBatch::new(HlcTimestamp::EPOCH, "s")
            .with_point(MetricPoint::gauge("queue_depth", 3.5));
        let request = build_metrics_request(&batch, Resource::default());
        let metric = &request.resource_metrics[0].scope_metrics[0].metrics[0];
        assert_eq!(
            metric.gauge.as_ref().unwrap().data_points[0].as_double,
            Some(3.5)
        );
    }

    #[test]
    fn histogram_bucket_counts_are_converted_from_cumulative_to_per_bucket() {
        let point = MetricPoint {
            name: "latency_seconds".to_owned(),
            value: MetricValue::Histogram {
                count: 10,
                sum: 4.0,
                buckets: vec![
                    HistogramBucket {
                        upper_bound: 1.0,
                        cumulative_count: 2,
                    },
                    HistogramBucket {
                        upper_bound: 5.0,
                        cumulative_count: 9,
                    },
                    HistogramBucket {
                        upper_bound: f64::INFINITY,
                        cumulative_count: 10,
                    },
                ],
            },
            labels: BTreeMap::new(),
        };
        let batch = MetricBatch::new(HlcTimestamp::EPOCH, "s").with_point(point);
        let request = build_metrics_request(&batch, Resource::default());
        let histogram = request.resource_metrics[0].scope_metrics[0].metrics[0]
            .histogram
            .as_ref()
            .unwrap();
        let dp = &histogram.data_points[0];
        assert_eq!(dp.count, "10");
        assert_eq!(dp.sum, 4.0);
        // Per-bucket: 2, (9-2)=7, (10-9)=1.
        assert_eq!(dp.bucket_counts, vec!["2", "7", "1"]);
        assert_eq!(dp.explicit_bounds, vec![1.0, 5.0]);
    }

    #[test]
    fn points_sharing_a_name_become_multiple_data_points_on_one_metric() {
        let batch = MetricBatch::new(HlcTimestamp::EPOCH, "s")
            .with_point(MetricPoint::counter("io_bytes_total", 1).with_label("direction", "rx"))
            .with_point(MetricPoint::counter("io_bytes_total", 2).with_label("direction", "tx"));
        let request = build_metrics_request(&batch, Resource::default());
        let metrics = &request.resource_metrics[0].scope_metrics[0].metrics;
        assert_eq!(metrics.len(), 1, "one metric, two data points");
        assert_eq!(metrics[0].sum.as_ref().unwrap().data_points.len(), 2);
    }

    #[test]
    fn metric_ordering_is_deterministic_regardless_of_point_order() {
        let forward = MetricBatch::new(HlcTimestamp::EPOCH, "s")
            .with_point(MetricPoint::counter("b", 1))
            .with_point(MetricPoint::counter("a", 1));
        let backward = MetricBatch::new(HlcTimestamp::EPOCH, "s")
            .with_point(MetricPoint::counter("a", 1))
            .with_point(MetricPoint::counter("b", 1));
        let names = |batch: &MetricBatch| {
            build_metrics_request(batch, Resource::default()).resource_metrics[0].scope_metrics[0]
                .metrics
                .iter()
                .map(|m| m.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&forward), names(&backward));
        assert_eq!(names(&forward), vec!["a", "b"]);
    }

    #[test]
    fn an_empty_batch_produces_no_metrics() {
        let batch = MetricBatch::new(HlcTimestamp::EPOCH, "s");
        let request = build_metrics_request(&batch, Resource::default());
        assert!(
            request.resource_metrics[0].scope_metrics[0]
                .metrics
                .is_empty()
        );
    }
}
