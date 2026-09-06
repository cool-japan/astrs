//! Fixture-based tests for `astrs migrate from-ros2`, against
//! representative ROS 2 launch files (blueprint §8.6, §10.5, §17).
//!
//! Every fixture under `tests/fixtures/ros2/` is a deliberate construction
//! exercising one specific mapping rule this crate's docs commit to --
//! see each fixture's own header comment for what it is testing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use astrs_manifest::ValidationErrorKind;
use astrs_migrate::{
    Ros2MigrateError, Ros2NoteSeverity, migrate_ros2_launch_file, migrate_ros2_launch_str,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ros2")
        .join(name)
}

#[test]
fn basic_node_matches_the_committed_golden_and_validates_clean_but_needs_attention() {
    let result = migrate_ros2_launch_file(fixture("basic.launch.xml")).unwrap();

    // The only note possible here is the always-present bridge scaffold
    // note -- message_type/direction are never inferable from a launch
    // file alone, so even this maximally "clean" fixture never returns
    // zero notes (see `MigrationResult::needs_attention_count`'s docs).
    assert_eq!(result.notes.len(), 1, "notes: {:?}", result.notes);
    assert_eq!(result.notes[0].severity, Ros2NoteSeverity::NeedsAttention);
    assert_eq!(result.notes[0].node.as_deref(), Some("chatter_demo_talker"));

    let manifest = astrs_manifest::Manifest::from_yaml_str(&result.yaml).unwrap();
    manifest.validate().unwrap();
    assert_eq!(manifest.nodes.len(), 1);
    let node = &manifest.nodes[0];
    assert_eq!(
        node.id, "chatter_demo_talker",
        "the namespace prefixes the id; node_name (checked below) keeps the bare local name"
    );
    assert_eq!(node.args, vec!["--ros-args", "-p", "use_sim_time:=true"]);
    assert_eq!(
        node.env.get("rate"),
        Some(&astrs_manifest::EnvValue::String("10".to_string()))
    );
    assert_eq!(
        node.env.get("TALKER_LOG_LEVEL"),
        Some(&astrs_manifest::EnvValue::String("debug".to_string()))
    );
    let ros2 = node.ros2.as_ref().unwrap();
    assert_eq!(ros2.compat, astrs_manifest::RosCompat::Humble);
    assert_eq!(ros2.namespace.as_deref(), Some("/chatter_demo"));
    assert_eq!(ros2.node_name.as_deref(), Some("talker"));
    assert_eq!(ros2.topic.as_deref(), Some("/chatter"));
    assert_eq!(ros2.message_type, None);
    assert_eq!(ros2.direction, None);

    let golden = std::fs::read_to_string(fixture("basic.expected.yaml")).unwrap();
    assert_eq!(
        result.yaml, golden,
        "from-ros2 output drifted from the committed golden"
    );
}

#[test]
fn output_is_stable_across_repeated_runs() {
    let first = migrate_ros2_launch_file(fixture("basic.launch.xml")).unwrap();
    let second = migrate_ros2_launch_file(fixture("basic.launch.xml")).unwrap();
    assert_eq!(first.yaml, second.yaml);
}

#[test]
fn namespace_composition_resolves_ids_and_remap_targets() {
    let result = migrate_ros2_launch_file(fixture("namespaces.launch.xml")).unwrap();
    let manifest = astrs_manifest::Manifest::from_yaml_str(&result.yaml).unwrap();
    manifest.validate().unwrap();
    assert_eq!(manifest.nodes.len(), 4);

    let find = |id: &str| manifest.nodes.iter().find(|n| n.id == id).unwrap();

    let robot1_driver = find("robot1_driver");
    assert_eq!(
        robot1_driver.ros2.as_ref().unwrap().namespace.as_deref(),
        Some("/robot1")
    );
    assert_eq!(
        robot1_driver.ros2.as_ref().unwrap().topic.as_deref(),
        Some("/robot1/cmd_vel"),
        "a relative remap target resolves against the ambient (pushed) namespace"
    );

    let lidar = find("robot1_sensors_lidar");
    assert_eq!(
        lidar.ros2.as_ref().unwrap().namespace.as_deref(),
        Some("/robot1/sensors"),
        "a node's own namespace= composes with the ambient push_ros_namespace"
    );
    assert_eq!(
        lidar.ros2.as_ref().unwrap().topic.as_deref(),
        Some("/robot1/sensors/lidar/scan_raw"),
        "a private ~/... remap target expands against <namespace>/<local name>"
    );

    let robot2_driver = find("robot2_driver");
    assert_eq!(
        robot2_driver.ros2.as_ref().unwrap().namespace.as_deref(),
        Some("/robot2"),
        "push-ros-namespace (hyphen spelling) with a leading / is absolute"
    );
    assert_eq!(
        robot2_driver.ros2.as_ref().unwrap().topic.as_deref(),
        Some("/emergency_stop"),
        "an absolute remap target is never rewritten"
    );

    let root_driver = find("driver");
    assert_eq!(
        root_driver.ros2.as_ref().unwrap().namespace,
        None,
        "a push_ros_namespace inside one group must not leak to a sibling outside it"
    );
}

#[test]
fn unmapped_constructs_are_never_silently_dropped() {
    let result = migrate_ros2_launch_file(fixture("unmapped_constructs.launch.xml")).unwrap();

    let manifest = astrs_manifest::Manifest::from_yaml_str(&result.yaml).unwrap();
    // `worker` (the fallback id) and `worker` (the renamed collision) both
    // resolve, so the manifest itself is structurally valid even though
    // it needs a lot of human attention.
    manifest.validate().unwrap();

    let by_node = |id: &str| -> Vec<&astrs_migrate::Ros2MigrationNote> {
        result
            .notes
            .iter()
            .filter(|n| n.node.as_deref() == Some(id))
            .collect()
    };

    assert!(manifest.nodes.iter().any(|n| n.id == "worker"));
    assert!(
        manifest.nodes.iter().any(|n| n.id == "worker_2"),
        "the second <node name=\"worker\"> collides with the fallback id and is renamed"
    );
    assert!(
        by_node("worker_2")
            .iter()
            .any(|n| n.message.contains("collided")),
    );
    assert!(
        by_node("worker")
            .iter()
            .any(|n| n.message.contains("no `name=` attribute")),
        "the executable-fallback node must explain itself"
    );

    let cfg_notes = by_node("cfg_node");
    assert!(
        cfg_notes
            .iter()
            .any(|n| n.message.contains("settings.yaml"))
    );

    let cond_notes = by_node("cond_node");
    assert!(
        cond_notes
            .iter()
            .any(|n| n.message.contains("conditional") && n.message.contains("use_cond"))
    );

    let respawn_notes = by_node("respawn_node");
    assert!(respawn_notes.iter().any(|n| n.message.contains("respawn=")));
    assert!(respawn_notes.iter().any(|n| n.message.contains("output=")));

    let gdb_notes = by_node("gdb_node");
    assert!(
        gdb_notes
            .iter()
            .any(|n| n.message.contains("launch-prefix") && n.message.contains("gdb --args")),
        "an unrecognized <node> attribute must be quoted back, not silently dropped"
    );

    assert!(
        manifest.nodes.iter().any(|n| n.id == "managed_node"),
        "a <lifecycle_node> must still be discovered as a manifest node"
    );
    let lifecycle_notes = by_node("managed_node");
    assert!(
        lifecycle_notes
            .iter()
            .any(|n| n.message.contains("lifecycle_node") && n.message.contains("Active")),
        "a <lifecycle_node> must be flagged for its managed state machine, which the bridge \
         scaffold has no equivalent for"
    );

    assert!(
        manifest.nodes.iter().any(|n| n.id == "delayed_node"),
        "a <node> nested inside an unrecognized <timer> wrapper must still be discovered, not \
         lost along with the wrapper"
    );

    // The composable node container never becomes a manifest node at all
    // -- its note is root-level, quoting both composable node names.
    assert!(!manifest.nodes.iter().any(|n| n.id.contains("container")));
    let root_notes: Vec<_> = result.notes.iter().filter(|n| n.node.is_none()).collect();
    assert!(root_notes.iter().any(|n| n.message.contains("container1")
        && n.message.contains("rectify")
        && n.message.contains("resize")));
    assert!(
        root_notes.iter().any(|n| n.message.contains("<let>")),
        "an unrecognized launch tag must still be quoted back, not silently vanish"
    );
    assert!(
        root_notes.iter().any(|n| n.message.contains("<timer>")),
        "the unrecognized <timer> wrapper itself must still be noted, even though its nested \
         <node> is now also discovered"
    );

    // Every note is quoted somewhere in the rendered YAML as a comment --
    // this is the crux of "never silently drop".
    for note in &result.notes {
        let needle = if note.severity == Ros2NoteSeverity::Dropped {
            "TODO(astrs migrate) [dropped]"
        } else {
            "TODO(astrs migrate)"
        };
        assert!(result.yaml.contains(needle));
    }
}

/// The sharpest case of "never guess silently" (blueprint §8.6): a node
/// name parameterized by an unresolved `$(var ...)` launch substitution
/// is carried through **verbatim** into the manifest id, rather than this
/// importer inventing a resolved value that would validate cleanly but
/// mean nothing. `astrs validate` then honestly flags it -- proving the
/// scaffold is honest, not merely superficially valid.
#[test]
fn substitution_in_name_survives_into_id_and_fails_validation_honestly() {
    let result = migrate_ros2_launch_file(fixture("substitution_name.launch.xml")).unwrap();

    let expected_id = "$(var robot_name)_controller";
    assert!(
        result
            .yaml
            .lines()
            .any(|l| l.trim() == format!("- id: {expected_id}")),
        "the unresolved substitution must survive verbatim (and unquoted -- astrs_yaml does not \
         quote `$`/`(`/`)`) into the rendered YAML's id line: {}",
        result.yaml
    );

    let manifest = astrs_manifest::Manifest::from_yaml_str(&result.yaml).unwrap();
    assert_eq!(manifest.nodes.len(), 1);
    assert_eq!(manifest.nodes[0].id, expected_id);

    let errors = manifest.validate().unwrap_err();
    assert_eq!(
        errors.errors(),
        &[astrs_manifest::ValidationError {
            path: "nodes[0].id".to_string(),
            kind: ValidationErrorKind::InvalidIdCharset {
                id: expected_id.to_string(),
            },
        }],
        "the id-charset violation must be the *only* validation error -- a manifest this \
         wrong-looking must still be otherwise structurally sound"
    );
}

#[test]
fn include_with_substitution_is_noted_not_followed() {
    let result = migrate_ros2_launch_file(fixture("include_substitution.launch.xml")).unwrap();
    let manifest = astrs_manifest::Manifest::from_yaml_str(&result.yaml).unwrap();
    manifest.validate().unwrap();
    assert_eq!(
        manifest.nodes.len(),
        1,
        "the include itself is never followed"
    );
    assert!(
        result
            .notes
            .iter()
            .any(|n| n.message.contains("substitution") && n.message.contains("find-pkg-share"))
    );
}

#[test]
fn include_resolves_relative_path_and_both_nodes_appear() {
    let result = migrate_ros2_launch_file(fixture("includes/root.launch.xml")).unwrap();
    let manifest = astrs_manifest::Manifest::from_yaml_str(&result.yaml).unwrap();
    manifest.validate().unwrap();
    assert_eq!(manifest.nodes.len(), 2);
    assert!(manifest.nodes.iter().any(|n| n.id == "root_node"));
    assert!(manifest.nodes.iter().any(|n| n.id == "child_node"));
}

#[test]
fn migrate_str_never_follows_includes_even_for_the_same_content() {
    let content = std::fs::read_to_string(fixture("includes/root.launch.xml")).unwrap();
    let result = migrate_ros2_launch_str(&content).unwrap();
    let manifest = astrs_manifest::Manifest::from_yaml_str(&result.yaml).unwrap();
    assert_eq!(
        manifest.nodes.len(),
        1,
        "migrate_ros2_launch_str has no base directory, so the <include> must not be followed"
    );
    assert!(
        result
            .notes
            .iter()
            .any(|n| n.message.contains("no base directory"))
    );
}

#[test]
fn malformed_xml_is_a_hard_error() {
    let err = migrate_ros2_launch_file(fixture("malformed.launch.xml")).unwrap_err();
    assert!(matches!(err, Ros2MigrateError::Xml { .. }));
}

#[test]
fn missing_top_level_file_is_an_io_error() {
    let missing = std::env::temp_dir().join("astrs-migrate-ros2-integration-does-not-exist.xml");
    let err = migrate_ros2_launch_file(&missing).unwrap_err();
    assert!(matches!(err, Ros2MigrateError::Io { .. }));
}

#[test]
fn python_launch_file_yields_a_skeleton_with_unverified_candidates() {
    let result = migrate_ros2_launch_file(fixture("talker.launch.py")).unwrap();

    assert!(result.yaml.starts_with("# Skeleton scaffold"));
    let manifest = astrs_manifest::Manifest::from_yaml_str(&result.yaml).unwrap();
    manifest.validate().unwrap();
    assert!(
        manifest.nodes.is_empty(),
        "a Python candidate is a comment only, never a live manifest node"
    );

    assert!(
        result
            .notes
            .iter()
            .any(|n| n.message.contains("does not execute or parse"))
    );
    let candidate_notes: Vec<_> = result
        .notes
        .iter()
        .filter(|n| n.message.contains("UNVERIFIED candidate"))
        .collect();
    assert_eq!(candidate_notes.len(), 1, "notes: {:?}", result.notes);
    assert!(candidate_notes[0].message.contains("demo_nodes_cpp"));
    assert!(candidate_notes[0].message.contains("- id: talker"));
    assert!(
        result.yaml.contains("- id: talker"),
        "the suggestion must be quoted into the YAML as a comment"
    );
    assert!(
        !result.yaml.lines().any(|l| l.trim() == "- id: talker"),
        "the suggestion must render as a comment, never as a live node entry"
    );
}
