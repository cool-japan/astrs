//! Shared types and pure logic for `tf-broadcast` (blueprint §10.6, §5.4).
//!
//! ```text
//!   astrs/timer/millis/20 ──► [broadcaster] ──map_odom (once)───► [consumer]
//!                                    │        odom_base (every tick)
//!                                    └──────────────────────────────►
//! ```
//!
//! `broadcaster` publishes a **static** `map`->`odom` offset once — the
//! `/tf_static` convention, latched rather than repeated — and a
//! **dynamic** `odom`->`base_link` transform that slides along X on every
//! tick, the `/tf` convention. Both ride the curated registry type
//! [`astrs_node_api::message::Transform`] (`std/geometry/v1/Transform`).
//!
//! `consumer` inserts every received transform into its own
//! [`astrs_tf::TransformBuffer`] exactly as a real tf2 listener would, then
//! looks up `map`->`base_link` at a timestamp exactly **between** two
//! consecutive dynamic samples — proving the interpolated, time-travel
//! query blueprint §10.6 promises rather than merely "the latest value".
//! [`build_report`] is that lookup as a pure function: it takes the
//! timestamped samples a node loop collected and returns what the buffer
//! computed alongside what a linear motion at the exact midpoint *time*
//! between two real samples must equal — the arithmetic mean of their two
//! translations — so both binaries and this crate's own tests can predict
//! a run's outcome without a daemon.
//!
//! # `/tf` bridging
//!
//! This example broadcasts entirely inside the AstRS graph; it does not
//! bridge to a real ROS 2 `/tf`/`/tf_static` topic. That bridge exists —
//! `bins/astrs-ros2-bridge-node` hand-writes `tf2_msgs/msg/TFMessage`
//! support specifically for a `ros2: { topic: /tf }` block (blueprint
//! §10.3 leaves `tf2_msgs` out of the pre-generated `common_interfaces`
//! bundle, so the bridge resolves it like any other type it hand-rolls —
//! see that crate's `Cargo.toml` for the pointer) — but wiring it up is a
//! `ros2-talker-bridge`-shaped exercise in its own right, not this one.
//! `examples/ros2-talker-bridge` is the worked example of that shape, for
//! a different topic.

use astrs_node_api::message::{Quaternion, Transform, Vector3 as WireVector3};
use astrs_tf::math::{Isometry3, Vector3};
use astrs_tf::{TfError, TfStamp, TransformBuffer};
use serde::{Deserialize, Serialize};

/// The broadcaster's tick input, as named in `dataflow.yml`.
pub const TICK_PORT: &str = "tick";
/// The latched static `map`->`odom` output/input port.
pub const MAP_ODOM_PORT: &str = "map_odom";
/// The sliding dynamic `odom`->`base_link` output/input port.
pub const ODOM_BASE_PORT: &str = "odom_base";

/// The root frame's name.
pub const MAP_FRAME: &str = "map";
/// The static child frame's name.
pub const ODOM_FRAME: &str = "odom";
/// The dynamic child frame's name.
pub const BASE_FRAME: &str = "base_link";

/// Environment variable overriding how many ticks `broadcaster` runs for.
pub const ENV_TICKS: &str = "TF_BROADCAST_TICKS";
/// How many ticks `broadcaster` runs for by default.
pub const DEFAULT_TICKS: u64 = 20;

/// Environment variable naming the JSON file `consumer` writes its
/// [`ConsumerReport`] to.
pub const ENV_REPORT_PATH: &str = "TF_CONSUMER_REPORT";

/// The fixed `map`->`odom` offset, in metres on X — the same shape as
/// `astrs-tf`'s own quick-start doc example, so a reader who has read that
/// crate's docs recognises the numbers.
pub const STATIC_OFFSET: [f64; 3] = [10.0, 0.0, 0.0];

/// How far `base_link` moves on X per tick, in metres.
pub const STEP_METRES: f64 = 0.1;

/// How many ticks this run should broadcast for.
///
/// A value the manifest cannot express falls back to [`DEFAULT_TICKS`]
/// rather than failing the node — including zero, which would make
/// `exit_when_nodes_finish` fire before the graph had done anything.
#[must_use]
pub fn tick_budget(raw: Option<&str>) -> u64 {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|ticks| *ticks > 0)
        .unwrap_or(DEFAULT_TICKS)
}

/// Where `consumer` writes its report when the manifest names no path.
#[must_use]
pub fn default_report_path() -> std::path::PathBuf {
    std::env::temp_dir().join("astrs-tf-broadcast-report.json")
}

/// The file this run's consumer writes its report to.
#[must_use]
pub fn report_path() -> std::path::PathBuf {
    std::env::var(ENV_REPORT_PATH)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default_report_path, std::path::PathBuf::from)
}

/// `base_link`'s translation relative to `odom` at `tick`, in metres.
///
/// A pure function of the tick index, deterministic and independent of wall
/// clock jitter, so both `broadcaster`'s sends and this crate's own tests
/// can predict it exactly.
#[must_use]
pub fn base_link_translation(tick: u64) -> [f64; 3] {
    [STEP_METRES * tick as f64, 0.0, 0.0]
}

/// The wire [`Transform`] for the latched `map`->`odom` offset.
#[must_use]
pub fn map_odom_transform() -> Transform {
    Transform::new(
        WireVector3::new(STATIC_OFFSET[0], STATIC_OFFSET[1], STATIC_OFFSET[2]),
        Quaternion::IDENTITY,
    )
}

/// The wire [`Transform`] for `odom`->`base_link` at `tick`.
#[must_use]
pub fn odom_base_transform(tick: u64) -> Transform {
    let [x, y, z] = base_link_translation(tick);
    Transform::new(WireVector3::new(x, y, z), Quaternion::IDENTITY)
}

/// Converts a wire [`Transform`] into the SE(3) type `astrs-tf` buffers.
///
/// Identity rotations throughout this example, so the conversion is exact —
/// no quaternion renormalisation loses anything here.
#[must_use]
pub fn to_isometry(transform: &Transform) -> Isometry3 {
    let t = transform.translation;
    let r = transform.rotation;
    Isometry3::new(
        Vector3::new(t.x, t.y, t.z),
        astrs_tf::math::Quaternion::new(r.x, r.y, r.z, r.w),
    )
}

/// The nanosecond count exactly halfway between two [`TfStamp`]s.
///
/// Computed as `a + (b - a) / 2` rather than `(a + b) / 2` so two stamps far
/// from the epoch never overflow `i64` on the way to their sum.
#[must_use]
pub fn midpoint_stamp(a: TfStamp, b: TfStamp) -> TfStamp {
    TfStamp::from_nanos(a.as_nanos() + (b.as_nanos() - a.as_nanos()) / 2)
}

/// The component-wise arithmetic mean of two translations.
#[must_use]
pub fn midpoint_translation(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        (a[0] + b[0]) / 2.0,
        (a[1] + b[1]) / 2.0,
        (a[2] + b[2]) / 2.0,
    ]
}

/// How far apart two translations may be and still count as "the same",
/// after nothing worse than `f64` rounding.
pub const TOLERANCE_METRES: f64 = 1e-9;

/// Whether `a` and `b` agree within [`TOLERANCE_METRES`] on every axis.
#[must_use]
pub fn translations_agree(a: [f64; 3], b: [f64; 3]) -> bool {
    (0..3).all(|axis| (a[axis] - b[axis]).abs() <= TOLERANCE_METRES)
}

/// What `consumer` observed and computed, written as JSON when the run ends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsumerReport {
    /// Whether the static `map`->`odom` frame was ever received.
    pub static_received: bool,
    /// How many dynamic `odom`->`base_link` samples were received.
    pub dynamic_samples: usize,
    /// The nanosecond timestamp the lookup queried — exactly halfway
    /// between two consecutive received samples.
    pub lookup_stamp_ns: i64,
    /// `map`->`base_link`'s translation as `astrs-tf` computed it by
    /// interpolation, at [`Self::lookup_stamp_ns`].
    pub looked_up_translation: [f64; 3],
    /// The translation a linear motion sampled at exactly that midpoint
    /// *time* must equal: the static offset plus the arithmetic mean of the
    /// two bracketing dynamic samples' translations.
    pub expected_translation: [f64; 3],
    /// Whether the two agree within [`TOLERANCE_METRES`].
    pub interpolation_matches: bool,
}

/// Buffers `static_transform` and every entry of `dynamic_samples` into a
/// fresh [`TransformBuffer`], then looks `map`->`base_link` up at the exact
/// midpoint *time* between two consecutive dynamic samples.
///
/// `dynamic_samples` must already be in the order they were received (=
/// increasing stamp), which is what a node loop that inserts each as it
/// arrives naturally produces.
///
/// Returns `None` when fewer than two dynamic samples were collected — a
/// lookup needs a pair to interpolate between, and this is not an error, a
/// run just too short to prove the interpolated case.
///
/// # Errors
///
/// A [`TfError`] from [`TransformBuffer::set_transform`] or
/// [`TransformBuffer::lookup_transform`] — unreachable for the finite,
/// non-degenerate, strictly increasing samples both binaries produce, but a
/// real `Result` regardless: this function's contract does not get to
/// assume its caller obeys that.
pub fn build_report(
    static_transform: &Transform,
    dynamic_samples: &[(TfStamp, Transform)],
) -> Result<Option<ConsumerReport>, TfError> {
    let mut buffer = TransformBuffer::new();
    buffer.set_transform(
        MAP_FRAME,
        ODOM_FRAME,
        to_isometry(static_transform),
        TfStamp::EPOCH,
        true,
    )?;
    for (stamp, transform) in dynamic_samples {
        buffer.set_transform(
            ODOM_FRAME,
            BASE_FRAME,
            to_isometry(transform),
            *stamp,
            false,
        )?;
    }

    if dynamic_samples.len() < 2 {
        return Ok(None);
    }
    let mid_index = dynamic_samples.len() / 2;
    let (stamp_a, transform_a) = &dynamic_samples[mid_index - 1];
    let (stamp_b, transform_b) = &dynamic_samples[mid_index];
    let query_stamp = midpoint_stamp(*stamp_a, *stamp_b);

    let looked_up =
        buffer.lookup_transform(MAP_FRAME, BASE_FRAME, astrs_tf::TimePoint::At(query_stamp))?;
    let dynamic_midpoint = midpoint_translation(
        [
            transform_a.translation.x,
            transform_a.translation.y,
            transform_a.translation.z,
        ],
        [
            transform_b.translation.x,
            transform_b.translation.y,
            transform_b.translation.z,
        ],
    );
    let expected = [
        STATIC_OFFSET[0] + dynamic_midpoint[0],
        STATIC_OFFSET[1] + dynamic_midpoint[1],
        STATIC_OFFSET[2] + dynamic_midpoint[2],
    ];
    let looked_up_translation = [
        looked_up.translation.x,
        looked_up.translation.y,
        looked_up.translation.z,
    ];

    Ok(Some(ConsumerReport {
        static_received: true,
        dynamic_samples: dynamic_samples.len(),
        lookup_stamp_ns: query_stamp.as_nanos(),
        looked_up_translation,
        expected_translation: expected,
        interpolation_matches: translations_agree(looked_up_translation, expected),
    }))
}

impl ConsumerReport {
    /// Renders the report as pretty JSON.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if it cannot be serialised, which its field
    /// types make impossible in practice.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// `base_link_translation` moves linearly and deterministically.
    #[test]
    fn base_link_translation_is_linear_in_the_tick_index() {
        assert_eq!(base_link_translation(0), [0.0, 0.0, 0.0]);
        assert_eq!(base_link_translation(10), [1.0, 0.0, 0.0]);
        assert_eq!(base_link_translation(20), [2.0, 0.0, 0.0]);
    }

    /// The wire transforms carry exactly what `base_link_translation` says,
    /// with an identity rotation.
    #[test]
    fn the_wire_transforms_match_the_pure_function() {
        let odom_base = odom_base_transform(7);
        let [x, y, z] = base_link_translation(7);
        assert_eq!(odom_base.translation, WireVector3::new(x, y, z));
        assert_eq!(odom_base.rotation, Quaternion::IDENTITY);

        let map_odom = map_odom_transform();
        assert_eq!(
            map_odom.translation,
            WireVector3::new(STATIC_OFFSET[0], STATIC_OFFSET[1], STATIC_OFFSET[2])
        );
    }

    /// `midpoint_stamp` lands exactly between two stamps, and does not
    /// overflow for stamps far from the epoch.
    #[test]
    fn midpoint_stamp_is_the_arithmetic_mean() {
        let a = TfStamp::from_nanos(1_000);
        let b = TfStamp::from_nanos(2_000);
        assert_eq!(midpoint_stamp(a, b).as_nanos(), 1_500);

        let far_a = TfStamp::from_nanos(1_700_000_000_000_000_000);
        let far_b = TfStamp::from_nanos(1_700_000_000_000_002_000);
        assert_eq!(
            midpoint_stamp(far_a, far_b).as_nanos(),
            1_700_000_000_000_001_000
        );
    }

    /// A lookup exactly between two consecutive real samples equals the
    /// static offset plus the arithmetic mean of their two translations —
    /// true regardless of how evenly the real ticks were actually spaced,
    /// because the query time is derived from the *same* two real stamps.
    #[test]
    fn the_lookup_matches_the_midpoint_of_two_consecutive_samples() {
        let static_transform = map_odom_transform();
        // Deliberately uneven real-world stamps: nothing here assumes a
        // clean, evenly spaced tick schedule.
        let samples = vec![
            (TfStamp::from_nanos(0), odom_base_transform(0)),
            (TfStamp::from_nanos(19_000_000), odom_base_transform(1)),
            (TfStamp::from_nanos(41_000_000), odom_base_transform(2)),
            (TfStamp::from_nanos(58_000_000), odom_base_transform(3)),
        ];
        let report = build_report(&static_transform, &samples)
            .unwrap()
            .expect("at least two samples were collected");

        assert!(report.static_received);
        assert_eq!(report.dynamic_samples, 4);
        // mid_index = 4 / 2 = 2, so the pair is samples[1]/samples[2].
        assert_eq!(report.lookup_stamp_ns, (19_000_000 + 41_000_000) / 2);
        let expected_dynamic =
            midpoint_translation(base_link_translation(1), base_link_translation(2));
        assert_eq!(
            report.expected_translation,
            [
                STATIC_OFFSET[0] + expected_dynamic[0],
                STATIC_OFFSET[1] + expected_dynamic[1],
                STATIC_OFFSET[2] + expected_dynamic[2],
            ]
        );
        assert!(
            translations_agree(report.looked_up_translation, report.expected_translation),
            "{report:?}"
        );
        assert!(report.interpolation_matches, "{report:?}");
    }

    /// Fewer than two dynamic samples cannot interpolate, and that is
    /// reported as `None` rather than an error or a made-up answer.
    #[test]
    fn one_sample_is_not_enough_to_interpolate() {
        let static_transform = map_odom_transform();
        let samples = vec![(TfStamp::from_nanos(0), odom_base_transform(0))];
        assert!(build_report(&static_transform, &samples).unwrap().is_none());
        assert!(build_report(&static_transform, &[]).unwrap().is_none());
    }

    /// The report round-trips as JSON.
    #[test]
    fn a_report_round_trips_as_json() {
        let report = ConsumerReport {
            static_received: true,
            dynamic_samples: 4,
            lookup_stamp_ns: 30_000_000,
            looked_up_translation: [10.15, 0.0, 0.0],
            expected_translation: [10.15, 0.0, 0.0],
            interpolation_matches: true,
        };
        let json = report.to_json().unwrap();
        let parsed: ConsumerReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }

    /// A value the manifest cannot express falls back to the default.
    #[test]
    fn the_tick_budget_falls_back_to_the_default() {
        assert_eq!(tick_budget(Some("5")), 5);
        for raw in [None, Some(""), Some("lots"), Some("0"), Some("-1")] {
            assert_eq!(tick_budget(raw), DEFAULT_TICKS, "{raw:?}");
        }
    }

    /// The report path defaults under the temporary directory.
    #[test]
    fn the_report_path_defaults_under_the_temp_dir() {
        assert!(default_report_path().starts_with(std::env::temp_dir()));
    }

    /// The committed manifest parses, validates, wires both typed edges and
    /// names this dataflow.
    #[test]
    fn the_committed_manifest_parses_and_validates() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dataflow.yml");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("std/geometry/v1/Transform"), "{text}");
        let manifest = astrs_manifest::Manifest::from_yaml_file(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        manifest
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(manifest.name.as_deref(), Some("tf-broadcast"));
        assert_eq!(manifest.nodes.len(), 2);
    }
}
