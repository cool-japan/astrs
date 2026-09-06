//! Round-trip stability: `parse -> to_yaml -> parse` must be a fixed
//! point of the *parsed structure*, even where the re-emitted YAML text
//! differs from the input (canonicalized durations, short-vs-long-form
//! inputs, omitted defaults). See [`astrs_manifest::Manifest::to_yaml`]'s
//! docs for why textual equality is not the right invariant here.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_manifest::Manifest;

const PERCEPTION_DEMO: &str = include_str!("fixtures/valid/perception_demo.yaml");

fn assert_round_trips(yaml: &str) {
    let original = Manifest::from_yaml_str(yaml).expect("must parse");
    let re_emitted = original.to_yaml().expect("must serialize");
    let reparsed = Manifest::from_yaml_str(&re_emitted).unwrap_or_else(|e| {
        panic!("re-emitted YAML failed to reparse: {e}\n--- re-emitted ---\n{re_emitted}")
    });
    assert_eq!(
        original, reparsed,
        "\n--- original ---\n{yaml}\n--- re-emitted ---\n{re_emitted}"
    );

    // A second round trip must be a genuine fixed point: re-emitting the
    // already-canonicalized form must not drift further.
    let twice_emitted = reparsed.to_yaml().expect("must serialize again");
    assert_eq!(re_emitted, twice_emitted);
}

#[test]
fn canonical_example_round_trips() {
    assert_round_trips(PERCEPTION_DEMO);
}

#[test]
fn minimal_manifest_round_trips() {
    assert_round_trips("nodes:\n  - id: solo\n    path: ./solo\n");
}

#[test]
fn explicit_default_value_still_reparses_to_the_default_after_being_dropped() {
    // `to_yaml` omits fields already at their default (documented on
    // `Manifest::to_yaml`) — so a manifest that *explicitly* writes
    // `health_check_interval: 5.0` (the default) must still round-trip
    // correctly even though that line does not survive re-emission: its
    // *absence* in the re-emitted YAML must reparse back to 5.0, not to
    // some other value or a missing field.
    let yaml = "nodes:\n  - id: solo\n    path: ./solo\nhealth_check_interval: 5.0\n";
    let original = Manifest::from_yaml_str(yaml).unwrap();
    assert_eq!(original.health_check_interval, 5.0);

    let emitted = original.to_yaml().unwrap();
    assert!(
        !emitted.contains("health_check_interval"),
        "the explicit-but-default value should be dropped on re-emit, emitted was: {emitted}"
    );

    let reparsed = Manifest::from_yaml_str(&emitted).unwrap();
    assert_eq!(
        reparsed.health_check_interval, 5.0,
        "absence must still resolve to the default"
    );
    assert_eq!(original, reparsed);
}

#[test]
fn every_node_source_kind_round_trips() {
    let yaml = "\
nodes:
  - id: by-path
    path: ./by-path
  - id: by-git
    git: https://example.com/repo.git
    tag: v1.0.0
    path: target/release/node
  - id: by-module
    module: ./sub-graph.yaml
  - id: by-ros2
    ros2:
      compat: jazzy
      topic: /scan
      message_type: sensor_msgs/msg/LaserScan
      direction: to_astrs
  - id: by-record
    record: [by-path/out]
  - id: by-operators
    operators:
      - id: op1
        operator: CropOperator
";
    assert_round_trips(yaml);
}

#[test]
fn duration_string_canonicalizes_to_a_number_but_stays_semantically_equal() {
    let yaml = "\
nodes:
  - id: flaky
    path: ./flaky
    restart_policy: on_failure
    restart_delay: \"500ms\"
    max_restart_delay: \"1m\"
";
    let original = Manifest::from_yaml_str(yaml).unwrap();
    let emitted = original.to_yaml().unwrap();

    // The blueprint leaves the exact duration representation to the
    // implementer; this crate picks "always canonicalize to a plain
    // number of seconds on output" (see `DurationSecs`'s docs) — so the
    // humantime-style input string must NOT survive re-emission verbatim...
    assert!(!emitted.contains("500ms"), "emitted was: {emitted}");
    assert!(
        emitted.contains("restart_delay: 0.5"),
        "emitted was: {emitted}"
    );
    assert!(
        emitted.contains("max_restart_delay: 60"),
        "emitted was: {emitted}"
    );

    // ...while remaining the exact same duration once reparsed.
    let reparsed = Manifest::from_yaml_str(&emitted).unwrap();
    assert_eq!(original, reparsed);
    assert_eq!(original.nodes[0].restart_delay.unwrap().as_secs_f64(), 0.5);
    assert_eq!(reparsed.nodes[0].restart_delay.unwrap().as_secs_f64(), 0.5);
}

#[test]
fn short_form_input_stays_short_form_after_round_trip() {
    let yaml = "\
nodes:
  - id: consumer
    path: ./consumer
    inputs:
      tick: astrs/timer/hz/50
";
    let original = Manifest::from_yaml_str(yaml).unwrap();
    let emitted = original.to_yaml().unwrap();
    assert!(
        emitted.contains("tick: astrs/timer/hz/50"),
        "expected the short form to survive verbatim, emitted was: {emitted}"
    );
}

#[test]
fn long_form_input_with_only_source_canonicalizes_to_short_form() {
    let yaml = "\
nodes:
  - id: consumer
    path: ./consumer
    inputs:
      frames:
        source: producer/frames
";
    let original = Manifest::from_yaml_str(yaml).unwrap();
    let emitted = original.to_yaml().unwrap();
    assert!(
        emitted.contains("frames: producer/frames"),
        "expected canonicalization to the short form, emitted was: {emitted}"
    );
}
