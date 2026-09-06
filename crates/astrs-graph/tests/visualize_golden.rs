//! Golden-file tests for the mermaid/DOT emitters (blueprint §5.2, §17).
//!
//! The fixture graph below is built once and rendered with both
//! [`astrs_graph::to_mermaid`] and [`astrs_graph::to_dot`]; the output is
//! compared byte-for-byte against the checked-in files under
//! `tests/golden/`. A deliberate mismatch here is the signal that either
//! emitter changed its output shape — update the checked-in file in the
//! same change, never silently.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_graph::DataflowGraph;
use astrs_manifest::{Deploy, Input, Manifest, Node, Urn};

const EXPECTED_MERMAID: &str = include_str!("golden/sample.mmd");
const EXPECTED_DOT: &str = include_str!("golden/sample.dot");

/// camera (coordinator-local, typed `frames` output) -> detector
/// (coordinator-local, untyped `detections` output) -> planner
/// (`robot-1`), plus a virtual timer tick into planner — exercises
/// machine clustering, URN-annotated vs. bare edge labels, and virtual-
/// source styling in one small graph.
fn sample_graph() -> DataflowGraph {
    let mut camera = Node::with_path("camera", "./camera");
    camera.outputs = vec!["frames".to_string()];
    camera.output_types.insert(
        "frames".to_string(),
        Urn::new("std/media/v1/Image[pixel=rgb8]"),
    );

    let mut detector = Node::with_path("detector", "./detector");
    detector.outputs = vec!["detections".to_string()];
    detector
        .inputs
        .insert("frames".to_string(), Input::from_source("camera/frames"));

    let mut planner = Node::with_path("planner", "./planner");
    planner.deploy = Some(Deploy {
        machine: Some("robot-1".to_string()),
        ..Deploy::default()
    });
    planner.inputs.insert(
        "detections".to_string(),
        Input::from_source("detector/detections"),
    );
    planner
        .inputs
        .insert("tick".to_string(), Input::from_source("astrs/timer/hz/50"));

    let manifest = Manifest {
        nodes: vec![camera, detector, planner],
        ..Manifest::default()
    };
    DataflowGraph::from_manifest(&manifest).unwrap().0
}

#[test]
fn mermaid_output_matches_golden_file() {
    let actual = astrs_graph::to_mermaid(&sample_graph());
    assert_eq!(
        actual, EXPECTED_MERMAID,
        "mermaid output changed; update tests/golden/sample.mmd if this is intentional:\n{actual}"
    );
}

#[test]
fn dot_output_matches_golden_file() {
    let actual = astrs_graph::to_dot(&sample_graph());
    assert_eq!(
        actual, EXPECTED_DOT,
        "DOT output changed; update tests/golden/sample.dot if this is intentional:\n{actual}"
    );
}

#[test]
fn both_emitters_are_deterministic_across_repeated_calls() {
    let graph = sample_graph();
    assert_eq!(
        astrs_graph::to_mermaid(&graph),
        astrs_graph::to_mermaid(&graph)
    );
    assert_eq!(astrs_graph::to_dot(&graph), astrs_graph::to_dot(&graph));
}
