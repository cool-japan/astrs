//! Parsing and validating the blueprint's canonical §8.1 example manifest.
//!
//! This is the acceptance gate for the whole crate: every field shape the
//! blueprint documents (git+path combined source, short- and long-form
//! inputs side by side, `record:` sugar, a `deploy:` override, a timer
//! virtual source, a bracketed-param URN, an integer `env:` value) appears
//! in this one document, and it must parse *and* validate clean.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_manifest::{EnvValue, Manifest, QueuePolicy, RestartPolicy};

const PERCEPTION_DEMO: &str = include_str!("fixtures/valid/perception_demo.yaml");

#[test]
fn canonical_example_parses() {
    let manifest = Manifest::from_yaml_str(PERCEPTION_DEMO).expect("must parse");
    assert_eq!(manifest.astrs, "1");
    assert_eq!(manifest.name.as_deref(), Some("perception-demo"));
    assert_eq!(manifest.health_check_interval, 5.0);
    assert!(manifest.exit_when_nodes_finish);
    assert_eq!(manifest.nodes.len(), 4);
}

#[test]
fn canonical_example_validates_clean() {
    let manifest = Manifest::from_yaml_str(PERCEPTION_DEMO).expect("must parse");
    let result = manifest.validate();
    assert!(
        result.is_ok(),
        "expected the canonical §8.1 example to validate clean, got: {:#?}",
        result.err().map(|e| e.errors().to_vec())
    );
}

#[test]
fn camera_node_shape() {
    let manifest = Manifest::from_yaml_str(PERCEPTION_DEMO).unwrap();
    let camera = manifest.nodes.iter().find(|n| n.id == "camera").unwrap();
    assert_eq!(camera.path.as_deref(), Some("./target/release/camera-node"));
    assert_eq!(
        camera.build.as_deref(),
        Some("cargo build --release -p camera-node")
    );
    assert_eq!(camera.outputs, vec!["frames".to_string()]);
    assert_eq!(
        camera.output_types.get("frames").map(|u| u.as_str()),
        Some("std/media/v1/Image[pixel=rgb8]")
    );
    assert!(camera.output_types.get("frames").unwrap().is_valid());
    assert_eq!(camera.env.get("CAMERA_INDEX"), Some(&EnvValue::Int(0)));
}

#[test]
fn detector_node_combines_git_and_path() {
    let manifest = Manifest::from_yaml_str(PERCEPTION_DEMO).unwrap();
    let detector = manifest.nodes.iter().find(|n| n.id == "detector").unwrap();
    assert_eq!(
        detector.git.as_deref(),
        Some("https://github.com/cool-japan/astrs-yolo")
    );
    assert_eq!(detector.tag.as_deref(), Some("v0.3.1"));
    assert!(detector.branch.is_none());
    assert!(detector.rev.is_none());
    assert_eq!(detector.path.as_deref(), Some("target/release/yolo-node"));
    assert_eq!(detector.build.as_deref(), Some("cargo build --release"));

    let frames_input = detector.inputs.get("frames").unwrap();
    assert_eq!(frames_input.source, "camera/frames");
    assert_eq!(frames_input.queue_size, 2);
    assert_eq!(frames_input.queue_policy, QueuePolicy::DropOldest);

    assert_eq!(detector.restart_policy, Some(RestartPolicy::OnFailure));
    assert_eq!(detector.max_restarts, Some(5));
    assert_eq!(
        detector.effective_restart_policy(),
        RestartPolicy::OnFailure
    );
}

#[test]
fn recorder_node_uses_record_sugar() {
    let manifest = Manifest::from_yaml_str(PERCEPTION_DEMO).unwrap();
    let recorder = manifest.nodes.iter().find(|n| n.id == "recorder").unwrap();
    assert_eq!(
        recorder.record,
        Some(vec![
            "camera/frames".to_string(),
            "detector/detections".to_string(),
        ])
    );
    // The recorder node itself declares no source-competing fields.
    assert!(recorder.path.is_none());
    assert!(recorder.git.is_none());
}

#[test]
fn planner_node_mixes_short_form_inputs_and_deploy() {
    let manifest = Manifest::from_yaml_str(PERCEPTION_DEMO).unwrap();
    let planner = manifest.nodes.iter().find(|n| n.id == "planner").unwrap();
    assert_eq!(planner.path.as_deref(), Some("./planner"));
    assert_eq!(
        planner.deploy.as_ref().and_then(|d| d.machine.as_deref()),
        Some("robot-1")
    );

    let detections = planner.inputs.get("detections").unwrap();
    assert_eq!(detections.source, "detector/detections");
    assert!(detections.is_short_form());

    let tick = planner.inputs.get("tick").unwrap();
    assert_eq!(tick.source, "astrs/timer/hz/50");
    assert!(tick.is_short_form());
}

#[test]
fn from_yaml_file_reads_the_same_fixture_from_temp_dir() {
    // Exercises `from_yaml_file` without any hardcoded absolute path: the
    // fixture content is copied into `std::env::temp_dir()` first.
    let dir =
        std::env::temp_dir().join(format!("astrs-manifest-parse-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("perception-demo.yaml");
    std::fs::write(&path, PERCEPTION_DEMO).expect("write fixture");

    let manifest = Manifest::from_yaml_file(&path).expect("must parse from file");
    assert_eq!(manifest.nodes.len(), 4);
    assert!(manifest.validate().is_ok());

    let _ = std::fs::remove_dir_all(&dir);
}
