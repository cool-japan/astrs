//! Integration test: the blueprint §8.1 canonical example manifest, taken
//! through the full `parse -> validate -> DataflowGraph::from_manifest`
//! pipeline exactly as `astrs validate`/`astrs run` would use it.
//!
//! This is deliberately the *same* text as astrs.md §8.1 (embedded rather
//! than read from a file — a `tests/fixtures/` directory belongs to
//! whichever crate owns the fixture; `astrs-manifest` already has its own
//! copy for its own tests, and this crate should not reach across a
//! sibling crate's test directory for it) so that any future edit to the
//! canonical example is a single, visible diff away from being caught
//! here if it stops producing the graph shape this test expects.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_graph::{DataflowGraph, EdgeKey, MachineId, NodeId, PortName, plan_placement};
use astrs_manifest::Manifest;

const CANONICAL_MANIFEST: &str = r#"
astrs: "1"
name: perception-demo
health_check_interval: 5.0
exit_when_nodes_finish: true

nodes:
  - id: camera
    path: ./target/release/camera-node
    build: cargo build --release -p camera-node
    outputs: [frames]
    output_types: { frames: "std/media/v1/Image[pixel=rgb8]" }
    env: { CAMERA_INDEX: 0 }

  - id: detector
    git: https://github.com/cool-japan/astrs-yolo
    tag: v0.3.1
    build: cargo build --release
    path: target/release/yolo-node
    inputs:
      frames:
        source: camera/frames
        queue_size: 2
        queue_policy: drop_oldest
    outputs: [detections]
    output_types: { detections: "std/vision/v1/Detections" }
    restart_policy: on_failure
    max_restarts: 5

  - id: recorder
    record: [camera/frames, detector/detections]

  - id: planner
    path: ./planner
    deploy: { machine: robot-1 }
    inputs:
      detections: detector/detections
      tick: astrs/timer/hz/50
"#;

fn build() -> DataflowGraph {
    let manifest = Manifest::from_yaml_str(CANONICAL_MANIFEST).expect("manifest parses");
    manifest.validate().expect("manifest validates");
    let (graph, construction_diagnostics) =
        DataflowGraph::from_manifest(&manifest).expect("graph builds");
    assert!(
        construction_diagnostics.is_empty(),
        "unexpected construction diagnostics: {construction_diagnostics:?}"
    );
    graph
}

#[test]
fn canonical_manifest_produces_the_expected_node_and_edge_shape() {
    let graph = build();

    assert_eq!(graph.node_count(), 4);
    // camera->detector, detector->planner, timer->planner, plus the two
    // record-sugar edges on `recorder`.
    assert_eq!(graph.edge_count(), 5);

    let camera = graph.node(&NodeId::new("camera")).expect("camera exists");
    assert_eq!(
        camera
            .output_type(&PortName::new("frames"))
            .map(astrs_manifest::Urn::as_str),
        Some("std/media/v1/Image[pixel=rgb8]")
    );
    assert!(camera.spawns_process);

    let planner = graph.node(&NodeId::new("planner")).expect("planner exists");
    assert_eq!(planner.machine, MachineId::Named("robot-1".to_string()));

    let recorder = graph
        .node(&NodeId::new("recorder"))
        .expect("recorder exists");
    assert!(recorder.spawns_process); // `record:` sugar still spawns astrs-record-node
    assert_eq!(recorder.inputs.len(), 2);
}

#[test]
fn canonical_manifest_has_no_diagnostics() {
    let graph = build();
    let diagnostics = graph.diagnostics();
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
}

#[test]
fn canonical_manifest_has_no_cycles() {
    let graph = build();
    let cyclic: Vec<_> = graph
        .sccs()
        .into_iter()
        .filter(|s| s.nodes.len() > 1)
        .collect();
    assert!(cyclic.is_empty(), "unexpected cycles: {cyclic:?}");
}

#[test]
fn canonical_manifest_record_sugar_wires_both_captured_outputs() {
    let graph = build();
    let recorder_edges: Vec<_> = graph.edges_into(&NodeId::new("recorder")).collect();
    assert_eq!(recorder_edges.len(), 2);
    let producers: std::collections::BTreeSet<_> = recorder_edges
        .iter()
        .filter_map(|(_, edge)| edge.from.producer_node())
        .cloned()
        .collect();
    assert!(producers.contains(&NodeId::new("camera")));
    assert!(producers.contains(&NodeId::new("detector")));
}

/// The 2-machine placement fixture the task asks for: `camera`/`detector`/
/// `recorder` default to the coordinator, `planner` is pinned to
/// `robot-1`, and exactly one edge (`detector/detections -> planner`)
/// crosses that boundary — the virtual timer tick never does.
#[test]
fn canonical_manifest_placement_plan_splits_across_two_machines() {
    let graph = build();
    let plan = plan_placement(&graph);

    assert_eq!(plan.machines.len(), 2);
    let coordinator = plan
        .machine(&MachineId::CoordinatorLocal)
        .expect("coordinator machine present");
    assert_eq!(
        coordinator.spawns,
        vec![
            NodeId::new("camera"),
            NodeId::new("detector"),
            NodeId::new("recorder"),
        ]
    );

    let robot1 = plan
        .machine(&MachineId::Named("robot-1".to_string()))
        .expect("robot-1 machine present");
    assert_eq!(robot1.spawns, vec![NodeId::new("planner")]);

    assert_eq!(plan.total_spawns(), 4);

    assert_eq!(plan.cross_machine_routes.len(), 1);
    let route = &plan.cross_machine_routes[0];
    assert_eq!(route.producer_machine, MachineId::CoordinatorLocal);
    assert_eq!(
        route.consumer_machine,
        MachineId::Named("robot-1".to_string())
    );
    assert_eq!(
        route.edge,
        EdgeKey::new(NodeId::new("planner"), PortName::new("detections"))
    );
}
