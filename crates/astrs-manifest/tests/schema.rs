//! `emit_schema()` produces a JSON Schema document that actually describes
//! the manifest shape the rest of this crate's tests exercise.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_manifest::emit_schema;

#[test]
fn schema_is_valid_json_with_expected_top_level_shape() {
    let schema = emit_schema();
    let value: serde_json::Value = serde_json::from_str(&schema).expect("must be valid JSON");
    let obj = value
        .as_object()
        .expect("schema root must be a JSON object");
    assert!(obj.contains_key("properties") || obj.contains_key("$ref"));
}

#[test]
fn schema_mentions_key_manifest_fields() {
    let schema = emit_schema();
    for expected in [
        "nodes",
        "health_check_interval",
        "restart_policy",
        "queue_size",
    ] {
        assert!(
            schema.contains(expected),
            "schema missing `{expected}`: {schema}"
        );
    }
}

#[test]
fn schema_never_mentions_output_framing() {
    let schema = emit_schema();
    assert!(!schema.contains("output_framing"));
}

#[test]
fn deny_unknown_fields_propagates_as_additional_properties_false() {
    // `schema_never_mentions_output_framing` above only proves the string
    // "output_framing" is absent, which is also true of a schema that
    // permits *any* extra property — it would not catch, say, a future
    // regression that dropped `#[serde(deny_unknown_fields)]` from
    // `Manifest` or `Node`. This test checks the actual contract those
    // attributes are supposed to produce: `additionalProperties: false`,
    // schemars' encoding of "no stray keys allowed" (blueprint §2.2/§8:
    // config must not silently accept fields it does not read).
    let schema = emit_schema();
    let value: serde_json::Value = serde_json::from_str(&schema).expect("must be valid JSON");

    assert_eq!(
        value.get("additionalProperties"),
        Some(&serde_json::Value::Bool(false)),
        "root manifest schema must set additionalProperties: false; schema was: {schema}"
    );
    let root_required = value["required"]
        .as_array()
        .expect("root schema must have a `required` array");
    assert!(
        root_required.contains(&serde_json::Value::String("nodes".to_string())),
        "root `required` must include `nodes`, got: {root_required:?}"
    );

    let node = &value["$defs"]["Node"];
    assert_eq!(
        node.get("additionalProperties"),
        Some(&serde_json::Value::Bool(false)),
        "Node schema must set additionalProperties: false; schema was: {schema}"
    );
    let node_required = node["required"]
        .as_array()
        .expect("Node schema must have a `required` array");
    assert!(
        node_required.contains(&serde_json::Value::String("id".to_string())),
        "Node `required` must include `id`, got: {node_required:?}"
    );
}

#[test]
fn schema_carries_the_rt_hub_and_operator_locator_fields() {
    // The 14 downstream consumers of these fields read the *generated
    // schema* for editor completion, so a field that parses but never
    // reaches the schema is only half delivered. Spellings are frozen:
    // this test is what freezes them.
    let value: serde_json::Value = serde_json::from_str(&emit_schema()).expect("valid JSON");
    let defs = &value["$defs"];

    for name in ["RtConfig", "RtPolicy", "HubSource"] {
        assert!(
            defs.get(name).is_some(),
            "schema is missing the `{name}` definition"
        );
    }

    let node = defs["Node"]["properties"]
        .as_object()
        .expect("Node properties");
    assert!(node.contains_key("rt"), "Node schema is missing `rt`");
    assert!(node.contains_key("hub"), "Node schema is missing `hub`");

    let operator = &defs["OperatorConfig"];
    let properties = operator["properties"]
        .as_object()
        .expect("OperatorConfig properties");
    for field in ["operator", "dylib", "wasm", "hub"] {
        assert!(
            properties.contains_key(field),
            "OperatorConfig schema is missing `{field}`"
        );
    }
    // `operator` names the type and stays required for every locator kind.
    let required = operator["required"].as_array().expect("required array");
    assert!(
        required.contains(&serde_json::Value::String("operator".to_string())),
        "OperatorConfig `required` must keep `operator`, got: {required:?}"
    );

    // The three `rt.policy` spellings are the frozen wire values.
    let policy = serde_json::to_string(&defs["RtPolicy"]).expect("serializable");
    for spelling in ["\"normal\"", "\"fifo\"", "\"rr\""] {
        assert!(policy.contains(spelling), "RtPolicy is missing {spelling}");
    }
}
