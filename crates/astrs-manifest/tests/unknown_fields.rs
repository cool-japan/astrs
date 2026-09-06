//! `deny_unknown_fields` rejection at every nesting level the blueprint
//! requires it: root, node, long-form input, `deploy:`, `ros2:`/`qos:`,
//! `type_rules:` entries, and `operators:` entries.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_manifest::Manifest;

fn assert_rejected(yaml: &str, context: &str) {
    let result = Manifest::from_yaml_str(yaml);
    assert!(
        result.is_err(),
        "{context}: expected an unknown-field rejection, but it parsed as {result:#?}"
    );
}

#[test]
fn root_level_unknown_field_is_rejected() {
    assert_rejected(
        "nodes:\n  - id: x\n    path: ./x\nbogus_root_field: true\n",
        "root",
    );
}

#[test]
fn node_level_unknown_field_is_rejected() {
    assert_rejected(
        "nodes:\n  - id: x\n    path: ./x\n    bogus_node_field: 1\n",
        "node",
    );
}

#[test]
fn long_form_input_unknown_field_is_rejected() {
    let yaml = "\
nodes:
  - id: x
    path: ./x
    inputs:
      frames:
        source: y/out
        bogus_input_field: 1
";
    assert_rejected(yaml, "long-form input");
}

#[test]
fn deploy_unknown_field_is_rejected() {
    let yaml = "\
nodes:
  - id: x
    path: ./x
    deploy: { machine: robot-1, bogus_deploy_field: true }
";
    assert_rejected(yaml, "deploy");
}

#[test]
fn ros2_block_unknown_field_is_rejected() {
    let yaml = "\
nodes:
  - id: bridge
    ros2:
      compat: humble
      topic: /scan
      message_type: sensor_msgs/msg/LaserScan
      direction: to_astrs
      bogus_ros2_field: true
    outputs: [scan]
";
    assert_rejected(yaml, "ros2 block");
}

#[test]
fn qos_block_unknown_field_is_rejected() {
    let yaml = "\
nodes:
  - id: bridge
    ros2:
      compat: humble
      topic: /scan
      message_type: sensor_msgs/msg/LaserScan
      direction: to_astrs
      qos: { reliable: true, bogus_qos_field: 1 }
    outputs: [scan]
";
    assert_rejected(yaml, "qos block");
}

#[test]
fn type_rule_unknown_field_is_rejected() {
    let yaml = "\
nodes:
  - id: x
    path: ./x
type_rules:
  - from: std/core/v1/Float32
    to: std/core/v1/Float64
    bogus_type_rule_field: true
";
    assert_rejected(yaml, "type_rules entry");
}

#[test]
fn operator_entry_unknown_field_is_rejected() {
    let yaml = "\
nodes:
  - id: x
    operators:
      - id: op1
        operator: SomeOperator
        bogus_operator_field: true
";
    assert_rejected(yaml, "operators entry");
}

#[test]
fn output_framing_field_is_always_rejected() {
    // Blueprint §2.2: "output_framing is dead config ... Field does not
    // exist" — must be an unknown-field rejection, at both node and root
    // scope, never silently accepted or ignored.
    assert_rejected(
        "nodes:\n  - id: x\n    path: ./x\n    output_framing: arrow-ipc\n",
        "node output_framing",
    );
    assert_rejected(
        "nodes:\n  - id: x\n    path: ./x\noutput_framing: arrow-ipc\n",
        "root output_framing",
    );
}

#[test]
fn rt_block_unknown_field_is_rejected() {
    assert_rejected(
        "nodes:\n  - id: x\n    path: ./x\n    rt: { policy: fifo, priority: 80, nice: -5 }\n",
        "rt block",
    );
}

#[test]
fn hub_long_form_unknown_field_is_rejected() {
    assert_rejected(
        "nodes:\n  - id: x\n    hub: { name: yolo, rev: v1, registry: other }\n",
        "hub long form",
    );
}
