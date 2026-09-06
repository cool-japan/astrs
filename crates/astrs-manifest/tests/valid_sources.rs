//! The positive half of the source-exclusivity matrix: one fixture per
//! source *kind*, each validating clean on its own — complementing
//! `tests/validate.rs`'s negative fixtures (`multiple_sources`,
//! `git_field_without_git`, `conflicting_git_refs`,
//! `dynamic_sentinel_with_git`), which only prove what's *rejected*.
//!
//! In particular this locks in the exclusivity rule from [`Node`]'s docs:
//! `git` + `path` is one source (not two), `git` combined with each of
//! `branch`/`tag`/`rev` individually is accepted (only *combining* two of
//! them is rejected), and the `dynamic` sentinel is fine as long as it is
//! the *sole* source.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_manifest::Manifest;

fn assert_validates_clean(yaml: &str, fixture_name: &str) {
    let manifest =
        Manifest::from_yaml_str(yaml).unwrap_or_else(|e| panic!("{fixture_name}: must parse: {e}"));
    let result = manifest.validate();
    assert!(
        result.is_ok(),
        "{fixture_name}: expected clean validation, got: {:#?}",
        result.err().map(|e| e.errors().to_vec())
    );
}

#[test]
fn module_alone_validates_clean() {
    assert_validates_clean(
        include_str!("fixtures/valid/source_module_alone.yaml"),
        "source_module_alone",
    );
}

#[test]
fn module_boundary_input_resolves_clean_in_a_standalone_module() {
    // A module manifest's internal `_mod/<port>` reference resolves
    // against its own `module:` header — even standalone, with no
    // including manifest anywhere in sight (blueprint §8.5; see
    // `NodeIndex::resolve`'s module-aware branch).
    assert_validates_clean(
        include_str!("fixtures/valid/source_module_boundary.yaml"),
        "source_module_boundary",
    );
}

#[test]
fn operators_alone_validates_clean() {
    assert_validates_clean(
        include_str!("fixtures/valid/source_operators_alone.yaml"),
        "source_operators_alone",
    );
}

#[test]
fn ros2_alone_validates_clean() {
    assert_validates_clean(
        include_str!("fixtures/valid/source_ros2_alone.yaml"),
        "source_ros2_alone",
    );
}

#[test]
fn record_alone_validates_clean() {
    assert_validates_clean(
        include_str!("fixtures/valid/source_record_alone.yaml"),
        "source_record_alone",
    );
}

#[test]
fn dynamic_sentinel_alone_validates_clean() {
    let yaml = include_str!("fixtures/valid/source_dynamic_alone.yaml");
    assert_validates_clean(yaml, "source_dynamic_alone");
    let manifest = Manifest::from_yaml_str(yaml).unwrap();
    assert!(manifest.nodes[0].is_dynamic_path());
}

#[test]
fn git_with_branch_alone_validates_clean() {
    assert_validates_clean(
        include_str!("fixtures/valid/source_git_branch.yaml"),
        "source_git_branch",
    );
}

#[test]
fn git_with_rev_alone_validates_clean() {
    assert_validates_clean(
        include_str!("fixtures/valid/source_git_rev.yaml"),
        "source_git_rev",
    );
}

#[test]
fn git_with_no_ref_selector_validates_clean() {
    // No branch/tag/rev at all: the exclusivity check only rejects
    // *combining* two or more, not omitting all three (the daemon's build
    // driver falls back to the repo's default branch).
    assert_validates_clean(
        include_str!("fixtures/valid/source_git_no_ref.yaml"),
        "source_git_no_ref",
    );
}

#[test]
fn hub_alone_validates_clean_in_the_short_form() {
    let yaml = include_str!("fixtures/valid/source_hub_alone.yaml");
    assert_validates_clean(yaml, "source_hub_alone");
    let manifest = Manifest::from_yaml_str(yaml).unwrap();
    let hub = manifest.nodes[0].hub.as_ref().expect("a hub source");
    assert_eq!(hub.name, "yolo-detector");
    assert_eq!(hub.rev.as_deref(), Some("v0.3.1"));
}

#[test]
fn hub_alone_validates_clean_in_the_structured_form() {
    let yaml = include_str!("fixtures/valid/source_hub_structured.yaml");
    assert_validates_clean(yaml, "source_hub_structured");

    // The two spellings must parse to the same value, or downstream
    // consumers would have to care which one the author happened to write.
    let structured = Manifest::from_yaml_str(yaml).unwrap();
    let short =
        Manifest::from_yaml_str(include_str!("fixtures/valid/source_hub_alone.yaml")).unwrap();
    assert_eq!(structured.nodes[0].hub, short.nodes[0].hub);
}

#[test]
fn hub_without_a_revision_validates_clean() {
    // No `@rev` at all: the index's own notion of latest, the same way
    // `git:` with no ref selector falls back to the default branch.
    assert_validates_clean(
        "nodes:\n  - id: detector\n    hub: yolo-detector\n",
        "hub, no revision",
    );
}

#[test]
fn a_realtime_rt_block_validates_clean() {
    let yaml = include_str!("fixtures/valid/node_rt_realtime.yaml");
    assert_validates_clean(yaml, "node_rt_realtime");
    let manifest = Manifest::from_yaml_str(yaml).unwrap();

    let control = manifest.nodes[0].rt.as_ref().expect("an rt block");
    assert_eq!(control.policy, astrs_manifest::RtPolicy::Fifo);
    assert_eq!(control.priority, Some(80));

    // `normal` with no priority is equally clean.
    let logger = manifest.nodes[1].rt.as_ref().expect("an rt block");
    assert_eq!(logger.policy, astrs_manifest::RtPolicy::Normal);
    assert_eq!(logger.priority, None);
}

#[test]
fn a_node_with_no_rt_block_validates_clean() {
    // The overwhelmingly common case: `rt:` is entirely optional.
    let manifest = Manifest::from_yaml_str("nodes:\n  - id: x\n    path: ./x\n").unwrap();
    assert!(manifest.nodes[0].rt.is_none());
    assert!(manifest.validate().is_ok());
}

#[test]
fn every_operator_locator_validates_clean() {
    let yaml = include_str!("fixtures/valid/operator_locators.yaml");
    assert_validates_clean(yaml, "operator_locators");
    let manifest = Manifest::from_yaml_str(yaml).unwrap();
    let operators = manifest.nodes[0]
        .operators
        .as_ref()
        .expect("an operators list");
    assert_eq!(operators.len(), 4);

    // The compiled-in registry entry carries no locator at all.
    assert!(operators[0].is_registry_sourced());
    assert_eq!(operators[1].locator_kinds(), vec!["dylib"]);
    assert_eq!(operators[2].locator_kinds(), vec!["wasm"]);
    assert_eq!(operators[3].locator_kinds(), vec!["hub"]);

    // Every entry still names the operator *type* it loads.
    for operator in operators {
        assert!(!operator.operator.is_empty(), "{operator:?}");
    }
}

#[test]
fn git_with_tag_alone_validates_clean() {
    // The blueprint's own §8.1 `detector` node already covers git+tag
    // (+path as the in-repo artifact); this isolates just git+tag with no
    // `path`, to confirm `path` is genuinely optional under `git`.
    let yaml = "\
nodes:
  - id: from-tag
    git: https://example.com/repo.git
    tag: v2.0.0
";
    assert_validates_clean(yaml, "inline git+tag, no path");
}
