//! One fixture per [`astrs_manifest::ValidationErrorKind`] variant,
//! asserting both the exact error path string and the error kind — not
//! just "validation failed somehow".
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_manifest::{Manifest, UnresolvedReferenceReason, ValidationErrorKind};

/// Parse `yaml` and return its validation errors, panicking if parsing
/// itself fails (every fixture here is meant to parse fine and fail only
/// at the *validation* stage — a parse failure would indicate the fixture
/// doesn't test what its name says).
fn validation_errors(yaml: &str) -> Vec<astrs_manifest::ValidationError> {
    let manifest = Manifest::from_yaml_str(yaml).expect("fixture must parse");
    match manifest.validate() {
        Ok(()) => panic!("expected validation to fail, but it passed"),
        Err(errors) => errors.into_iter().collect(),
    }
}

#[test]
fn invalid_id_charset() {
    let yaml = include_str!("fixtures/invalid/invalid_id_charset.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0].id");
    assert!(matches!(
        errors[0].kind,
        ValidationErrorKind::InvalidIdCharset { .. }
    ));
}

#[test]
fn duplicate_node_id() {
    let yaml = include_str!("fixtures/invalid/duplicate_node_id.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    // The *second* occurrence is flagged; the first stands as the
    // canonical definition.
    assert_eq!(errors[0].path, "nodes[1].id");
    assert!(matches!(
        errors[0].kind,
        ValidationErrorKind::DuplicateNodeId { ref id } if id == "camera"
    ));
}

#[test]
fn no_source_declared() {
    let yaml = include_str!("fixtures/invalid/no_source.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0]");
    assert!(matches!(
        errors[0].kind,
        ValidationErrorKind::NoSourceDeclared
    ));
}

#[test]
fn multiple_sources_declared() {
    let yaml = include_str!("fixtures/invalid/multiple_sources.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0]");
    match &errors[0].kind {
        ValidationErrorKind::MultipleSourcesDeclared { found } => {
            assert!(found.contains(&"path"));
            assert!(found.contains(&"module"));
        }
        other => panic!("expected MultipleSourcesDeclared, got {other:?}"),
    }
}

#[test]
fn git_field_without_git() {
    let yaml = include_str!("fixtures/invalid/git_field_without_git.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0].tag");
    assert!(matches!(
        errors[0].kind,
        ValidationErrorKind::GitFieldWithoutGit { field: "tag" }
    ));
}

#[test]
fn conflicting_git_refs() {
    let yaml = include_str!("fixtures/invalid/conflicting_git_refs.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0].git");
    match &errors[0].kind {
        ValidationErrorKind::ConflictingGitRefs { found } => {
            assert!(found.contains(&"branch"));
            assert!(found.contains(&"tag"));
        }
        other => panic!("expected ConflictingGitRefs, got {other:?}"),
    }
}

#[test]
fn dynamic_sentinel_with_git() {
    let yaml = include_str!("fixtures/invalid/dynamic_sentinel_with_git.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0].path");
    assert!(matches!(
        errors[0].kind,
        ValidationErrorKind::DynamicSentinelWithGit
    ));
}

#[test]
fn queue_size_too_small() {
    let yaml = include_str!("fixtures/invalid/queue_size_too_small.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[1].inputs.data");
    assert!(matches!(
        errors[0].kind,
        ValidationErrorKind::QueueSizeTooSmall { found: 0 }
    ));
}

#[test]
fn max_restarts_requires_policy() {
    let yaml = include_str!("fixtures/invalid/max_restarts_requires_policy.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0].max_restarts");
    assert!(matches!(
        errors[0].kind,
        ValidationErrorKind::MaxRestartsRequiresPolicy
    ));
}

#[test]
fn invalid_urn() {
    let yaml = include_str!("fixtures/invalid/invalid_urn.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0].output_types.out");
    assert!(matches!(
        errors[0].kind,
        ValidationErrorKind::InvalidUrn { .. }
    ));
}

#[test]
fn empty_cpu_affinity() {
    let yaml = include_str!("fixtures/invalid/empty_cpu_affinity.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0].cpu_affinity");
    assert!(matches!(
        errors[0].kind,
        ValidationErrorKind::EmptyCpuAffinity
    ));
}

#[test]
fn unresolved_reference_unknown_node() {
    let yaml = include_str!("fixtures/invalid/unresolved_reference_unknown_node.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0].inputs.data");
    match &errors[0].kind {
        ValidationErrorKind::UnresolvedReference { value, reason } => {
            assert_eq!(value, "ghost/out");
            assert!(matches!(reason, UnresolvedReferenceReason::UnknownNode(n) if n == "ghost"));
        }
        other => panic!("expected UnresolvedReference, got {other:?}"),
    }
}

#[test]
fn unresolved_reference_unknown_output() {
    let yaml = include_str!("fixtures/invalid/unresolved_reference_unknown_output.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[1].inputs.data");
    match &errors[0].kind {
        ValidationErrorKind::UnresolvedReference { reason, .. } => {
            assert!(matches!(
                reason,
                UnresolvedReferenceReason::UnknownOutput { node, output }
                    if node == "producer" && output == "detections"
            ));
        }
        other => panic!("expected UnresolvedReference, got {other:?}"),
    }
}

#[test]
fn unresolved_reference_malformed() {
    let yaml = include_str!("fixtures/invalid/unresolved_reference_malformed.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0].inputs.data");
    match &errors[0].kind {
        ValidationErrorKind::UnresolvedReference { reason, .. } => {
            assert!(matches!(reason, UnresolvedReferenceReason::Malformed));
        }
        other => panic!("expected UnresolvedReference, got {other:?}"),
    }
}

#[test]
fn unresolved_reference_bad_virtual_source() {
    let yaml = include_str!("fixtures/invalid/unresolved_reference_bad_virtual.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0].inputs.tick");
    match &errors[0].kind {
        ValidationErrorKind::UnresolvedReference { reason, .. } => {
            assert!(matches!(reason, UnresolvedReferenceReason::InvalidTimer(_)));
        }
        other => panic!("expected UnresolvedReference, got {other:?}"),
    }
}

#[test]
fn unknown_module_input() {
    let yaml = include_str!("fixtures/invalid/unknown_module_input.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0].inputs.frames");
    match &errors[0].kind {
        ValidationErrorKind::UnresolvedReference { reason, .. } => {
            assert!(matches!(
                reason,
                UnresolvedReferenceReason::UnknownModuleInput { port, declared }
                    if port == "framez" && declared == &vec!["frames".to_string()]
            ));
        }
        other => panic!("expected UnresolvedReference, got {other:?}"),
    }
}

#[test]
fn type_for_undeclared_output() {
    let yaml = include_str!("fixtures/invalid/type_for_undeclared_output.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0].output_types.frmaes");
    match &errors[0].kind {
        ValidationErrorKind::TypeForUndeclaredPort {
            map,
            port_kind,
            name,
        } => {
            assert_eq!(*map, "output_types");
            assert_eq!(*port_kind, "output");
            assert_eq!(name, "frmaes");
        }
        other => panic!("expected TypeForUndeclaredPort, got {other:?}"),
    }
}

#[test]
fn type_for_undeclared_input() {
    let yaml = include_str!("fixtures/invalid/type_for_undeclared_input.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[1].input_types.framez");
    match &errors[0].kind {
        ValidationErrorKind::TypeForUndeclaredPort {
            map,
            port_kind,
            name,
        } => {
            assert_eq!(*map, "input_types");
            assert_eq!(*port_kind, "input");
            assert_eq!(name, "framez");
        }
        other => panic!("expected TypeForUndeclaredPort, got {other:?}"),
    }
}

#[test]
fn invalid_hub_name() {
    let yaml = include_str!("fixtures/invalid/invalid_hub_name.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0].hub");
    assert!(matches!(
        errors[0].kind,
        ValidationErrorKind::InvalidHubName { ref name } if name == "Yolo_Detector"
    ));
}

#[test]
fn an_invalid_hub_name_on_an_operator_entry_is_flagged_at_its_own_path() {
    let yaml = "\
nodes:
  - id: host
    operators:
      - id: track
        operator: ByteTracker
        hub: Byte_Tracker@v1
";
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0].operators[0].hub");
    assert!(matches!(
        errors[0].kind,
        ValidationErrorKind::InvalidHubName { .. }
    ));
}

#[test]
fn rt_priority_required_for_a_realtime_policy() {
    let yaml = include_str!("fixtures/invalid/rt_priority_required.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0].rt.priority");
    match &errors[0].kind {
        ValidationErrorKind::RtPriorityRequired { policy, min, max } => {
            assert_eq!(*policy, astrs_manifest::RtPolicy::Fifo);
            assert_eq!(*min, astrs_manifest::RT_PRIORITY_MIN);
            assert_eq!(*max, astrs_manifest::RT_PRIORITY_MAX);
        }
        other => panic!("expected RtPriorityRequired, got {other:?}"),
    }
}

#[test]
fn rt_priority_out_of_range() {
    let yaml = include_str!("fixtures/invalid/rt_priority_out_of_range.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0].rt.priority");
    assert!(matches!(
        errors[0].kind,
        ValidationErrorKind::RtPriorityOutOfRange { found: 100, .. }
    ));
}

#[test]
fn rt_priority_zero_is_out_of_range_too() {
    // The real-time range starts at 1: `sched_priority: 0` is what
    // SCHED_OTHER uses, i.e. "not actually real-time".
    let yaml = "nodes:\n  - id: x\n    path: ./x\n    rt:\n      policy: fifo\n      priority: 0\n";
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert!(matches!(
        errors[0].kind,
        ValidationErrorKind::RtPriorityOutOfRange { found: 0, .. }
    ));
}

#[test]
fn rt_priority_with_normal_policy_is_rejected() {
    let yaml = include_str!("fixtures/invalid/rt_priority_with_normal_policy.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0].rt.priority");
    assert!(matches!(
        errors[0].kind,
        ValidationErrorKind::RtPriorityWithNormalPolicy
    ));
}

#[test]
fn a_priority_with_an_omitted_policy_is_rejected_as_normal() {
    // `policy` defaults to `normal`, so `rt: { priority: 80 }` is the same
    // rejected shape as spelling `normal` out — never a silent promotion to
    // a real-time class.
    let yaml = "nodes:\n  - id: x\n    path: ./x\n    rt:\n      priority: 80\n";
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert!(matches!(
        errors[0].kind,
        ValidationErrorKind::RtPriorityWithNormalPolicy
    ));
}

#[test]
fn multiple_operator_locators_are_rejected() {
    let yaml = include_str!("fixtures/invalid/multiple_operator_locators.yaml");
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    assert_eq!(errors[0].path, "nodes[0].operators[0]");
    match &errors[0].kind {
        ValidationErrorKind::MultipleOperatorLocators { found } => {
            assert_eq!(found, &vec!["dylib", "wasm"]);
        }
        other => panic!("expected MultipleOperatorLocators, got {other:?}"),
    }
}

#[test]
fn hub_is_a_source_kind_that_conflicts_with_the_others() {
    let yaml = "nodes:\n  - id: confused\n    path: ./confused\n    hub: yolo\n";
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
    match &errors[0].kind {
        ValidationErrorKind::MultipleSourcesDeclared { found } => {
            assert_eq!(found, &vec!["path", "hub"]);
        }
        other => panic!("expected MultipleSourcesDeclared, got {other:?}"),
    }
}

#[test]
fn validate_collects_multiple_errors_in_one_pass() {
    // Blueprint §8: "returns ALL errors not first-fail" — a manifest with
    // two independent problems must report both, not stop at the first.
    let yaml = "\
nodes:
  - id: \"bad id\"
    max_restarts: 3
";
    let errors = validation_errors(yaml);
    assert_eq!(errors.len(), 3, "errors were: {errors:#?}");
    let kinds: Vec<&str> = errors
        .iter()
        .map(|e| match &e.kind {
            ValidationErrorKind::InvalidIdCharset { .. } => "invalid_id",
            ValidationErrorKind::NoSourceDeclared => "no_source",
            ValidationErrorKind::MaxRestartsRequiresPolicy => "max_restarts",
            other => panic!("unexpected error kind: {other:?}"),
        })
        .collect();
    assert!(kinds.contains(&"invalid_id"));
    assert!(kinds.contains(&"no_source"));
    assert!(kinds.contains(&"max_restarts"));
}

#[test]
fn valid_manifest_returns_ok() {
    let yaml = "nodes:\n  - id: solo\n    path: ./solo\n";
    let manifest = Manifest::from_yaml_str(yaml).unwrap();
    assert!(manifest.validate().is_ok());
}
