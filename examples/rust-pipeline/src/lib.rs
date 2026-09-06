//! Shared types for `rust-pipeline`, the canonical §8.1-shaped typed graph.
//!
//! ```text
//!   astrs/timer/hz/20 ──► [camera-sim] ──frames──► [detector-sim] ──detections──┐
//!                                          │                                    │
//!                                          └────────────frames──────────────────┴──► [recorder-sim]
//! ```
//!
//! Three nodes, two typed edges, one type per port — and the types are the
//! point. Every edge in this graph carries a **type URN** (§8.3), the manifest
//! declares them, `astrs validate` checks that the two ends agree before
//! anything is spawned, and the node code decodes into a Rust struct rather
//! than into a byte slice.
//!
//! # Two ways to have a typed message, both shown here
//!
//! | Port | Type | How it is defined |
//! |---|---|---|
//! | `camera-sim/frames` | [`astrs_node_api::message::Image`] | a **`std` registry type** (§24.3), already implemented in the node API |
//! | `detector-sim/detections` | [`Detections`] | **your own struct** with `#[derive(AstrsMessage)]` (§9.1) |
//!
//! The second is blueprint §9.1's own example, verbatim: a plain struct, one
//! attribute naming its URN, and the derive maps its fields onto the closed
//! columnar type set at compile time. Nothing here writes an encoder.
//!
//! `Image` is *not* an `AstrsMessage`: its layout depends on its URN parameter
//! (`[pixel=rgb8]` is a `UInt8` sample column, `[pixel=rgb32f]` a `Float32`
//! one), which a `const URN` cannot express. It carries the parameter as a
//! Rust value instead and exposes the same `to_record_batch`/`from_record_batch`
//! pair — so a camera publishes with [`astrs_node_api::RawOutput::send_batch`]
//! and a consumer reads it with [`astrs_node_api::Payload::view`] exactly as it
//! would any other typed message.

use astrs_data::{RecordBatch, Result as DataResult};
use astrs_node_api::message::{AstrsMessage, FromPayload};
use astrs_operator_macros::AstrsMessage as AstrsMessageDerive;
use serde::{Deserialize, Serialize};

/// The camera's output port, and the detector's input.
pub const FRAMES_PORT: &str = "frames";

/// The detector's output port.
pub const DETECTIONS_PORT: &str = "detections";

/// The recorder's input carrying frames.
pub const RECORDER_FRAMES_PORT: &str = "frames";

/// The recorder's input carrying detections.
pub const RECORDER_DETECTIONS_PORT: &str = "detections";

/// The camera's tick input.
pub const TICK_PORT: &str = "tick";

/// Environment variable naming the file the recorder writes its tally to.
pub const ENV_SUMMARY_PATH: &str = "PIPELINE_SUMMARY";

/// Environment variable overriding how many frames the camera publishes.
pub const ENV_FRAMES: &str = "PIPELINE_FRAMES";

/// How many frames the camera publishes by default.
pub const DEFAULT_FRAMES: u64 = 12;

/// The simulated camera's frame width, in pixels.
pub const FRAME_WIDTH: u32 = 32;

/// The simulated camera's frame height, in pixels.
pub const FRAME_HEIGHT: u32 = 24;

/// Where the recorder writes its tally when the manifest names no path.
#[must_use]
pub fn default_summary_path() -> std::path::PathBuf {
    std::env::temp_dir().join("astrs-rust-pipeline-summary.json")
}

/// The file the recorder writes its tally to.
#[must_use]
pub fn summary_path() -> std::path::PathBuf {
    std::env::var(ENV_SUMMARY_PATH)
        .ok()
        .filter(|value| !value.is_empty())
        .map_or_else(default_summary_path, std::path::PathBuf::from)
}

/// How many frames this run should publish.
#[must_use]
pub fn frame_budget() -> u64 {
    std::env::var(ENV_FRAMES)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_FRAMES)
}

/// Detected objects in one frame — blueprint §9.1's own example type.
///
/// The derive reads the `urn` attribute, maps each field onto the closed
/// columnar type set (§6.1) and generates `to_record_batch`/`from_record_batch`
/// that agree with the `std/vision/v1/Detections` layout in
/// `astrs-data`'s registry. A field whose Rust type has no columnar
/// counterpart is a compile error, not a runtime surprise.
#[derive(Debug, Clone, PartialEq, AstrsMessageDerive)]
#[astrs(urn = "std/vision/v1/Detections")]
pub struct Detections {
    /// One `[x, y, width, height]` box per detection, in pixels.
    pub boxes: Vec<[f32; 4]>,
    /// The confidence of each detection, in the same order.
    pub scores: Vec<f32>,
    /// The class label of each detection, in the same order.
    pub labels: Vec<u32>,
}

// `FromPayload` is what `Payload::view::<T>()` needs. There is deliberately no
// blanket `impl<T: AstrsMessage>` in the node API (its `message` module
// explains why: the read-only view types could not have one), so a message of
// your own writes this three-line bridge.
impl FromPayload for Detections {
    fn from_batch(batch: &RecordBatch) -> DataResult<Self> {
        <Self as AstrsMessage>::from_record_batch(batch)
    }
}

impl Detections {
    /// How many detections this message carries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.boxes.len()
    }

    /// Whether the message carries no detections at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.boxes.is_empty()
    }
}

/// What the recorder saw, written as JSON when the graph finishes.
///
/// A file rather than a log line because it is what the M1 conformance test
/// reads: a test that grepped the terminal would be asserting on formatting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineSummary {
    /// How many frames reached the recorder.
    pub frames: u64,
    /// How many detection messages reached the recorder.
    pub detections: u64,
    /// How many individual boxes those messages carried.
    pub boxes: u64,
    /// Whether every input closed cleanly rather than the run being cut short.
    pub inputs_closed: bool,
}

impl PipelineSummary {
    /// Renders the summary as pretty JSON.
    ///
    /// # Errors
    ///
    /// [`serde_json::Error`] if the summary cannot be serialised, which its
    /// field types make impossible in practice.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// The detections a deterministic "detector" reports for frame `index`.
///
/// Deterministic on purpose: a conformance test can assert on the numbers, and
/// a reader can see that the detector's output is a pure function of its
/// input rather than of the clock.
#[must_use]
pub fn detections_for(index: u64, width: u32, height: u32) -> Detections {
    let count = (index % 3) + 1;
    let mut boxes = Vec::with_capacity(count as usize);
    let mut scores = Vec::with_capacity(count as usize);
    let mut labels = Vec::with_capacity(count as usize);
    for slot in 0..count {
        let offset = (index + slot) as f32;
        boxes.push([
            offset % width as f32,
            offset % height as f32,
            (width as f32 / 4.0).max(1.0),
            (height as f32 / 4.0).max(1.0),
        ]);
        scores.push(0.5 + (slot as f32) / 10.0);
        labels.push(u32::try_from(slot).unwrap_or(0));
    }
    Detections {
        boxes,
        scores,
        labels,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_data::urn::layout_of_str;

    /// The derived layout is the one the `std` registry publishes for the URN
    /// — which is what makes the port declaration in `dataflow.yml` true.
    #[test]
    fn the_derived_layout_matches_the_std_registry() {
        let derived = <Detections as AstrsMessage>::data_type();
        let registered = layout_of_str(Detections::URN).unwrap();
        assert!(
            derived.layout_eq(&registered),
            "derived {derived} vs registered {registered}"
        );
    }

    /// A message survives the columnar round trip the wire performs.
    #[test]
    fn detections_round_trip_through_a_record_batch() {
        let value = detections_for(4, FRAME_WIDTH, FRAME_HEIGHT);
        let batch = value.to_record_batch().unwrap();
        let decoded = Detections::from_batch(&batch).unwrap();
        assert_eq!(decoded, value);
        assert_eq!(decoded.len(), 2);
        assert!(!decoded.is_empty());
    }

    /// The detector is a pure function of the frame index.
    #[test]
    fn detections_are_deterministic() {
        assert_eq!(
            detections_for(7, FRAME_WIDTH, FRAME_HEIGHT),
            detections_for(7, FRAME_WIDTH, FRAME_HEIGHT)
        );
        assert_ne!(
            detections_for(7, FRAME_WIDTH, FRAME_HEIGHT),
            detections_for(8, FRAME_WIDTH, FRAME_HEIGHT)
        );
    }

    /// The summary is JSON the conformance test can read back.
    #[test]
    fn the_summary_round_trips_as_json() {
        let summary = PipelineSummary {
            frames: 12,
            detections: 12,
            boxes: 24,
            inputs_closed: true,
        };
        let json = summary.to_json().unwrap();
        let parsed: PipelineSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, summary);
    }

    /// The tally path is overridable, so two runs never collide.
    #[test]
    fn the_summary_path_defaults_under_the_temp_dir() {
        assert!(default_summary_path().starts_with(std::env::temp_dir()));
    }
}
