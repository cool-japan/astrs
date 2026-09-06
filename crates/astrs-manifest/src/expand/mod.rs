//! Module expansion (blueprint §8.5): flattening every `module:`-sourced
//! node in a manifest into its referenced sub-graph, recursively.
//!
//! [`Manifest::validate`](crate::Manifest::validate) accepts a manifest
//! that still has unexpanded `module:`-sourced nodes — a module boundary is
//! itself a well-formed, resolvable wiring shape (see
//! [`crate::Manifest::validate`]'s module-aware `_mod/<port>` resolution).
//! [`expand`] is the separate, subsequent step that actually inlines those
//! sub-graphs, producing a manifest with no `module:`-sourced nodes left at
//! all — the shape the graph-model layer (`astrs-graph`) and the `astrs`
//! runtime (which per §8.5 "never sees modules") expect. Call
//! [`crate::Manifest::validate`] again on the result if the caller wants
//! the full structural guarantee on the *flattened* graph too (module
//! expansion does not re-run every check `validate` performs — see this
//! module's "What expand does not check" section below).
//!
//! # The module format
//!
//! A manifest becomes includable as a module by carrying a
//! [`crate::ModuleHeader`] (`module: {name, inputs, outputs}` at the
//! root — see that type's docs for a worked example). Another manifest's
//! node includes it via [`crate::Node::module`] (a path, resolved relative
//! to the *including* manifest's own directory — see
//! [`FsModuleLoader`]/[`ModuleLoader`] — never the process's current
//! working directory), and wires it up like any other node: `inputs:` maps
//! the module's declared boundary inputs to sources visible in the
//! including scope, `outputs:` (a subset of the module's declared boundary
//! outputs) is what sibling nodes may reference as `<host id>/<port>`.
//!
//! Internally, a module's own nodes reach the boundary via the reserved id
//! [`MODULE_BOUNDARY_NODE_ID`] (`_mod`): `source: _mod/frames` reads
//! whatever the including node's `inputs.frames` supplies. Surfacing an
//! *output* is symmetric but does not need a reserved node id (outputs
//! have no `source:` field to write one into) — instead, whichever
//! internal node's own `outputs` list contains an entry literally named
//! `detections` **or** `_mod/detections` is that boundary output's
//! producer; `detections` (matching the header's own spelling) is the
//! recommended form for anything hand-written. Exactly one producer must
//! exist per exposed output — none is
//! [`ExpandError::MissingModuleOutputProducer`], more than one is
//! [`ExpandError::AmbiguousModuleOutput`].
//!
//! # Flattening
//!
//! A module-hosting node disappears entirely, replaced by every one of its
//! (recursively expanded) internal nodes, each renamed `<host
//! id>.<internal id>` — composable across nesting, so a module included two
//! levels deep produces ids like `mid.inner.leaf`. Every reference within
//! the flattened output is rewritten to match:
//!
//! - A sibling's reference to the host (`<host id>/<port>`) becomes a
//!   reference straight to the producer: `<host id>.<producer id>/<port or
//!   _mod/port>`.
//! - An internal node's `_mod/<port>` becomes whatever the host's own
//!   `inputs.<port>` supplied, verbatim (that string already names
//!   something in the *including* scope's namespace, so it is never itself
//!   prefixed).
//! - Any other internal cross-reference (one sibling-within-the-module
//!   referencing another) gets the same `<host id>.` prefix applied to its
//!   own node-id part.
//!
//! Declared type annotations follow the same boundary: a host node's
//! `output_types[<port>]` back-fills the producer's own `output_types`
//! (under whichever of `<port>`/`_mod/<port>` it actually used) when the
//! producer does not already annotate it, and a host's
//! `input_types[<port>]` back-fills every internal consumer's
//! `input_types` the same way — a typed module boundary must not silently
//! become untyped once flattened.
//!
//! # Environment merge
//!
//! A module's own root `env:` acts as a default beneath the including
//! scope's own `env:` — "parent env overlays module env" — which is itself
//! beneath the internal node's own `env:`. This composes correctly across
//! arbitrarily deep nesting: an outer manifest's `env:` wins over every
//! module it (transitively) includes, and each internal node's own
//! explicit `env:` still wins over all of it, exactly matching
//! [`crate::Node::effective_env`]'s existing node-over-graph precedence
//! (see this module's `scope` submodule for the cumulative-env derivation
//! this relies on). The flattened manifest's own root `env:` is left
//! exactly as the original root manifest's — only individual nodes'
//! `env:` maps change — and any node-level key whose *value* now exactly
//! equals the root's is pruned back out, since
//! [`crate::Node::effective_env`] would reconstruct the identical result
//! from the root layer alone; this keeps `to_yaml` output free of the
//! noise that baking every ancestor key into every node would otherwise
//! add.
//!
//! # Type rules
//!
//! Every scope's own `type_rules` (root and every included module) is
//! unioned into the flattened manifest's root `type_rules`, exact
//! duplicates removed — a rule declared inside a module applies
//! graph-wide once that module is included, exactly like the root's own.
//!
//! # Cycles and depth
//!
//! The ancestor chain of manifest files currently being expanded is
//! tracked purely as *that* chain (a DFS stack), not a global "ever
//! loaded" set — so the same module included independently by two
//! unrelated branches (a diamond, not a cycle) is fine, while actually
//! revisiting an ancestor is [`ExpandError::Cycle`], reported with the
//! full chain. [`ExpandOptions::max_depth`] (default 32) additionally
//! bounds plain nesting depth, catching a long chain of distinct modules
//! that never repeats a file at all.
//!
//! # What expand does not check
//!
//! [`expand`] transforms what it recognizes and otherwise leaves a
//! reference untouched — a dangling `<sibling>/<port>` that was already
//! broken before expansion is not re-diagnosed here with an
//! expand-specific error; it surfaces from a subsequent
//! [`crate::Manifest::validate`] call exactly as it would have without any
//! module involved. Fields that are not about wiring — a host node's own
//! `restart_policy`, `deploy`, `cpu_affinity`, and so on — are simply
//! discarded when the host node is replaced; only its internal nodes' own
//! such fields (as authored in the module file) survive, unmodified.

use std::collections::BTreeMap;
use std::path::Path;

use crate::{Manifest, TypeRule};

mod error;
mod loader;
mod rewrite;
mod scope;

pub use error::ExpandError;
pub use loader::{FsModuleLoader, MemoryModuleLoader, ModuleLoader};

pub use crate::module_header::MODULE_BOUNDARY_NODE_ID;

/// Options controlling [`expand`]/[`expand_with_options`]'s recursion
/// limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpandOptions {
    /// The maximum module-inclusion nesting depth
    /// ([`ExpandError::DepthExceeded`] beyond it) — see this module's
    /// "Cycles and depth" docs.
    pub max_depth: usize,
}

impl Default for ExpandOptions {
    fn default() -> Self {
        Self { max_depth: 32 }
    }
}

/// Flatten every `module:`-sourced node in `manifest`, recursively, using
/// [`ExpandOptions::default`].
///
/// `base_dir` is the directory `manifest`'s own [`crate::Node::module`]
/// paths are resolved relative to — typically the directory containing
/// whatever file `manifest` itself was loaded from. This is a plain
/// parameter, never the process's current working directory (blueprint
/// §8.5).
///
/// This is what [`crate::Manifest::expand`] delegates to; call
/// [`expand_with_options`] directly to override [`ExpandOptions`].
///
/// # Errors
///
/// See [`ExpandError`].
pub fn expand(
    manifest: &Manifest,
    base_dir: &Path,
    loader: &dyn ModuleLoader,
) -> Result<Manifest, ExpandError> {
    expand_with_options(manifest, base_dir, loader, ExpandOptions::default())
}

/// [`expand`], with an explicit [`ExpandOptions`] rather than the default.
///
/// # Errors
///
/// See [`ExpandError`].
pub fn expand_with_options(
    manifest: &Manifest,
    base_dir: &Path,
    loader: &dyn ModuleLoader,
    options: ExpandOptions,
) -> Result<Manifest, ExpandError> {
    let ctx = scope::Ctx {
        options: &options,
        loader,
    };
    let mut state = scope::RecursionState {
        stack: Vec::new(),
        type_rules: Vec::new(),
    };
    let root_env = manifest.env.clone();
    let root_boundary = scope::Boundary::default();
    let input = scope::ScopeInput {
        base_dir,
        ancestor_env: &root_env,
        boundary: &root_boundary,
        location: "the root manifest",
    };

    let result = scope::expand_scope(&ctx, &mut state, manifest, &input)?;

    let mut flattened = manifest.clone();
    flattened.nodes = result.nodes;
    flattened.type_rules = dedupe_type_rules(state.type_rules);
    flattened.module = None;
    prune_redundant_env(&mut flattened);
    Ok(flattened)
}

/// Remove exact-duplicate `{from, to}` pairs from a collected
/// `type_rules` list, keeping the first occurrence's position.
fn dedupe_type_rules(rules: Vec<TypeRule>) -> Vec<TypeRule> {
    let mut seen = std::collections::BTreeSet::new();
    rules
        .into_iter()
        .filter(|rule| seen.insert((rule.from.clone(), rule.to.clone())))
        .collect()
}

/// Drop any node-level `env:` entry whose value now exactly matches the
/// flattened manifest's own root `env:` — see this module's "Environment
/// merge" docs for why this is always lossless.
fn prune_redundant_env(manifest: &mut Manifest) {
    let root_env: BTreeMap<_, _> = manifest.env.clone();
    for node in &mut manifest.nodes {
        node.env.retain(|k, v| root_env.get(k) != Some(v));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::EnvValue;
    use std::path::PathBuf;

    fn base_dir() -> PathBuf {
        PathBuf::from("/graphs")
    }

    #[test]
    fn expands_a_boundary_only_module() {
        let root = Manifest::from_yaml_str(
            "\
nodes:
  - id: camera
    path: ./camera
    outputs: [frames]
  - id: perception
    module: ./perception.yaml
    inputs:
      frames: camera/frames
    outputs: [detections]
  - id: planner
    path: ./planner
    inputs:
      detections: perception/detections
",
        )
        .unwrap();
        let loader = MemoryModuleLoader::new().with_file(
            "/graphs/perception.yaml",
            "\
module:
  name: perception
  inputs: [frames]
  outputs: [detections]
nodes:
  - id: passthrough
    path: ./passthrough
    inputs:
      frames: _mod/frames
    outputs: [detections]
",
        );

        let flattened = expand(&root, &base_dir(), &loader).unwrap();
        let ids: Vec<_> = flattened.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["camera", "perception.passthrough", "planner"]);

        let passthrough = &flattened.nodes[1];
        assert_eq!(
            passthrough.inputs.get("frames").map(|i| i.source.as_str()),
            Some("camera/frames")
        );

        let planner = &flattened.nodes[2];
        assert_eq!(
            planner.inputs.get("detections").map(|i| i.source.as_str()),
            Some("perception.passthrough/detections")
        );

        assert!(flattened.module.is_none());
        flattened
            .validate()
            .expect("flattened manifest must validate clean");
    }

    #[test]
    fn expands_a_two_level_nested_composition() {
        let root = Manifest::from_yaml_str(
            "\
nodes:
  - id: mid
    module: ./mid.yaml
    outputs: [result]
  - id: sink
    path: ./sink
    inputs:
      value: mid/result
",
        )
        .unwrap();
        let loader = MemoryModuleLoader::new()
            .with_file(
                "/graphs/mid.yaml",
                "\
module:
  name: mid
  outputs: [result]
nodes:
  - id: inner
    module: ./leaf.yaml
    outputs: [result]
",
            )
            .with_file(
                "/graphs/leaf.yaml",
                "\
module:
  name: leaf
  outputs: [result]
nodes:
  - id: source
    path: ./source
    outputs: [result]
",
            );

        let flattened = expand(&root, &base_dir(), &loader).unwrap();
        let ids: Vec<_> = flattened.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["mid.inner.source", "sink"]);

        let sink = &flattened.nodes[1];
        assert_eq!(
            sink.inputs.get("value").map(|i| i.source.as_str()),
            Some("mid.inner.source/result")
        );
        flattened
            .validate()
            .expect("flattened manifest must validate clean");
    }

    #[test]
    fn detects_an_include_cycle() {
        let root = Manifest::from_yaml_str(
            "\
nodes:
  - id: a
    module: ./a.yaml
",
        )
        .unwrap();
        let loader = MemoryModuleLoader::new()
            .with_file(
                "/graphs/a.yaml",
                "\
module:
  name: a
nodes:
  - id: to_b
    module: ./b.yaml
",
            )
            .with_file(
                "/graphs/b.yaml",
                "\
module:
  name: b
nodes:
  - id: back_to_a
    module: ./a.yaml
",
            );

        let err = expand(&root, &base_dir(), &loader).unwrap_err();
        assert!(matches!(err, ExpandError::Cycle { .. }));
        if let ExpandError::Cycle { chain } = err {
            assert_eq!(
                chain,
                vec![
                    PathBuf::from("/graphs/a.yaml"),
                    PathBuf::from("/graphs/b.yaml"),
                    PathBuf::from("/graphs/a.yaml"),
                ]
            );
        }
    }

    #[test]
    fn depth_limit_rejects_a_long_acyclic_chain() {
        // Three distinct files, none repeated: not a cycle, but three
        // levels deep.
        let root = Manifest::from_yaml_str("nodes:\n  - id: a\n    module: ./a.yaml\n").unwrap();
        let loader = MemoryModuleLoader::new()
            .with_file(
                "/graphs/a.yaml",
                "module:\n  name: a\nnodes:\n  - id: b\n    module: ./b.yaml\n",
            )
            .with_file(
                "/graphs/b.yaml",
                "module:\n  name: b\nnodes:\n  - id: c\n    module: ./c.yaml\n",
            )
            .with_file(
                "/graphs/c.yaml",
                "module:\n  name: c\nnodes:\n  - id: leaf\n    path: ./leaf\n",
            );

        let err = expand_with_options(&root, &base_dir(), &loader, ExpandOptions { max_depth: 2 })
            .unwrap_err();
        assert!(matches!(err, ExpandError::DepthExceeded { max_depth: 2 }));

        // The same chain succeeds with a high enough limit.
        assert!(
            expand_with_options(&root, &base_dir(), &loader, ExpandOptions { max_depth: 3 })
                .is_ok()
        );
    }

    #[test]
    fn name_collision_after_prefixing_is_rejected() {
        // A plain sibling node literally named `a.b` collides with the
        // node the module hosted at id `a` would produce once its own
        // internal `b` is prefixed to `a.b` — a collision that cannot
        // exist before expansion (nothing here violates
        // `Manifest::validate`'s own duplicate-id check on the
        // unexpanded manifest).
        let root = Manifest::from_yaml_str(
            "\
nodes:
  - id: a.b
    path: ./collider
  - id: a
    module: ./sub.yaml
",
        )
        .unwrap();
        let loader = MemoryModuleLoader::new().with_file(
            "/graphs/sub.yaml",
            "module:\n  name: sub\nnodes:\n  - id: b\n    path: ./b\n",
        );

        let err = expand(&root, &base_dir(), &loader).unwrap_err();
        assert!(matches!(
            err,
            ExpandError::DuplicateNodeId { ref id, .. } if id == "a.b"
        ));
    }

    #[test]
    fn missing_output_producer_is_rejected() {
        let root = Manifest::from_yaml_str(
            "nodes:\n  - id: m\n    module: ./m.yaml\n    outputs: [detections]\n",
        )
        .unwrap();
        let loader = MemoryModuleLoader::new().with_file(
            "/graphs/m.yaml",
            "module:\n  name: m\n  outputs: [detections]\nnodes:\n  - id: x\n    path: ./x\n",
        );

        let err = expand(&root, &base_dir(), &loader).unwrap_err();
        assert!(matches!(
            err,
            ExpandError::MissingModuleOutputProducer { ref host_node, ref port }
                if host_node == "m" && port == "detections"
        ));
    }

    #[test]
    fn ambiguous_output_producer_is_rejected() {
        let root = Manifest::from_yaml_str(
            "nodes:\n  - id: m\n    module: ./m.yaml\n    outputs: [detections]\n",
        )
        .unwrap();
        let loader = MemoryModuleLoader::new().with_file(
            "/graphs/m.yaml",
            "\
module:
  name: m
  outputs: [detections]
nodes:
  - id: x
    path: ./x
    outputs: [detections]
  - id: y
    path: ./y
    outputs: [_mod/detections]
",
        );

        let err = expand(&root, &base_dir(), &loader).unwrap_err();
        assert!(matches!(
            err,
            ExpandError::AmbiguousModuleOutput { ref host_node, .. } if host_node == "m"
        ));
    }

    #[test]
    fn unsupplied_boundary_input_is_rejected() {
        let root = Manifest::from_yaml_str("nodes:\n  - id: m\n    module: ./m.yaml\n").unwrap();
        let loader = MemoryModuleLoader::new().with_file(
            "/graphs/m.yaml",
            "\
module:
  name: m
  inputs: [frames]
nodes:
  - id: x
    path: ./x
    inputs:
      frames: _mod/frames
",
        );

        let err = expand(&root, &base_dir(), &loader).unwrap_err();
        assert!(matches!(err, ExpandError::UnsuppliedModuleInput { .. }));
    }

    #[test]
    fn unknown_host_input_port_is_rejected() {
        let root = Manifest::from_yaml_str(
            "\
nodes:
  - id: camera
    path: ./camera
    outputs: [frames]
  - id: m
    module: ./m.yaml
    inputs:
      framez: camera/frames
",
        )
        .unwrap();
        let loader = MemoryModuleLoader::new().with_file(
            "/graphs/m.yaml",
            "module:\n  name: m\n  inputs: [frames]\nnodes: []\n",
        );

        let err = expand(&root, &base_dir(), &loader).unwrap_err();
        assert!(matches!(
            err,
            ExpandError::UnknownModuleInput { ref host_node, ref port }
                if host_node == "m" && port == "framez"
        ));
    }

    #[test]
    fn not_a_module_is_rejected() {
        let root =
            Manifest::from_yaml_str("nodes:\n  - id: m\n    module: ./plain.yaml\n").unwrap();
        let loader = MemoryModuleLoader::new()
            .with_file("/graphs/plain.yaml", "nodes:\n  - id: x\n    path: ./x\n");

        let err = expand(&root, &base_dir(), &loader).unwrap_err();
        assert!(matches!(err, ExpandError::NotAModule { .. }));
    }

    #[test]
    fn env_merge_precedence_is_module_then_outer_then_node() {
        let root = Manifest::from_yaml_str(
            "\
env: { LEVEL: root, ROOT_ONLY: r }
nodes:
  - id: m
    module: ./m.yaml
",
        )
        .unwrap();
        let loader = MemoryModuleLoader::new().with_file(
            "/graphs/m.yaml",
            "\
module:
  name: m
env: { LEVEL: module, MODULE_ONLY: mo }
nodes:
  - id: x
    path: ./x
    env: { LEVEL: node }
  - id: y
    path: ./y
",
        );

        let flattened = expand(&root, &base_dir(), &loader).unwrap();
        let x = flattened.nodes.iter().find(|n| n.id == "m.x").unwrap();
        let y = flattened.nodes.iter().find(|n| n.id == "m.y").unwrap();

        // `x` set its own LEVEL: node's own setting wins over everything.
        let x_env = x.effective_env(&flattened.env);
        assert_eq!(
            x_env.get("LEVEL"),
            Some(&EnvValue::String("node".to_string()))
        );
        // Root's LEVEL wins over the module's for `y`, which set nothing.
        let y_env = y.effective_env(&flattened.env);
        assert_eq!(
            y_env.get("LEVEL"),
            Some(&EnvValue::String("root".to_string()))
        );
        // Keys unique to each layer still flow through for both nodes.
        assert_eq!(
            y_env.get("ROOT_ONLY"),
            Some(&EnvValue::String("r".to_string()))
        );
        assert_eq!(
            y_env.get("MODULE_ONLY"),
            Some(&EnvValue::String("mo".to_string()))
        );
    }

    #[test]
    fn type_annotations_transfer_across_the_boundary() {
        let root = Manifest::from_yaml_str(
            "\
nodes:
  - id: camera
    path: ./camera
    outputs: [frames]
    output_types: { frames: \"std/media/v1/Image\" }
  - id: m
    module: ./m.yaml
    inputs:
      frames: camera/frames
    outputs: [detections]
    input_types: { frames: \"std/media/v1/Image\" }
    output_types: { detections: \"std/vision/v1/Detections\" }
",
        )
        .unwrap();
        let loader = MemoryModuleLoader::new().with_file(
            "/graphs/m.yaml",
            "\
module:
  name: m
  inputs: [frames]
  outputs: [detections]
nodes:
  - id: x
    path: ./x
    inputs:
      frames: _mod/frames
    outputs: [detections]
",
        );

        let flattened = expand(&root, &base_dir(), &loader).unwrap();
        let x = flattened.nodes.iter().find(|n| n.id == "m.x").unwrap();
        assert_eq!(
            x.input_types.get("frames").map(|u| u.as_str()),
            Some("std/media/v1/Image")
        );
        assert_eq!(
            x.output_types.get("detections").map(|u| u.as_str()),
            Some("std/vision/v1/Detections")
        );
    }

    #[test]
    fn type_annotations_propagate_through_an_intermediate_level_that_redeclares_neither() {
        // Two levels of nesting: `outer` declares the boundary types,
        // `mid`'s own hosting node re-declares neither `input_types` nor
        // `output_types` (only `inputs`/`outputs`), and `leaf`'s internal
        // node does not self-annotate either — the types must still
        // arrive at the innermost node, chained through the level that
        // itself carries no annotation at all.
        let root = Manifest::from_yaml_str(
            "\
nodes:
  - id: camera
    path: ./camera
    outputs: [frames]
  - id: outer
    module: ./mid.yaml
    inputs:
      frames: camera/frames
    outputs: [detections]
    input_types: { frames: \"std/media/v1/Image\" }
    output_types: { detections: \"std/vision/v1/Detections\" }
",
        )
        .unwrap();
        let loader = MemoryModuleLoader::new()
            .with_file(
                "/graphs/mid.yaml",
                "\
module:
  name: mid
  inputs: [frames]
  outputs: [detections]
nodes:
  - id: inner
    module: ./leaf.yaml
    inputs:
      frames: _mod/frames
    outputs: [detections]
",
            )
            .with_file(
                "/graphs/leaf.yaml",
                "\
module:
  name: leaf
  inputs: [frames]
  outputs: [detections]
nodes:
  - id: x
    path: ./x
    inputs:
      frames: _mod/frames
    outputs: [detections]
",
            );

        let flattened = expand(&root, &base_dir(), &loader).unwrap();
        let x = flattened
            .nodes
            .iter()
            .find(|n| n.id == "outer.inner.x")
            .unwrap();
        assert_eq!(
            x.input_types.get("frames").map(|u| u.as_str()),
            Some("std/media/v1/Image"),
            "input type must chain through `inner`, which redeclares nothing itself"
        );
        assert_eq!(
            x.output_types.get("detections").map(|u| u.as_str()),
            Some("std/vision/v1/Detections"),
            "output type must be found on the eventual producer `x`, however deep it is nested"
        );
    }

    #[test]
    fn type_rules_from_root_and_module_are_unioned_and_deduped() {
        let root = Manifest::from_yaml_str(
            "\
type_rules:
  - from: std/core/v1/Float32
    to: std/core/v1/Float64
nodes:
  - id: m
    module: ./m.yaml
",
        )
        .unwrap();
        let loader = MemoryModuleLoader::new().with_file(
            "/graphs/m.yaml",
            "\
module:
  name: m
type_rules:
  - from: std/core/v1/Float32
    to: std/core/v1/Float64
  - from: std/core/v1/Int32
    to: std/core/v1/Int64
nodes:
  - id: x
    path: ./x
",
        );

        let flattened = expand(&root, &base_dir(), &loader).unwrap();
        assert_eq!(flattened.type_rules.len(), 2);
    }

    #[test]
    fn a_module_included_via_two_unrelated_branches_is_not_a_cycle() {
        // Diamond reuse: `left` and `right` both include the same leaf
        // module independently. Neither includes the other, so this must
        // not be flagged as a cycle.
        let root = Manifest::from_yaml_str(
            "\
nodes:
  - id: left
    module: ./leaf.yaml
  - id: right
    module: ./leaf.yaml
",
        )
        .unwrap();
        let loader = MemoryModuleLoader::new().with_file(
            "/graphs/leaf.yaml",
            "module:\n  name: leaf\nnodes:\n  - id: x\n    path: ./x\n",
        );

        let flattened = expand(&root, &base_dir(), &loader).unwrap();
        let ids: Vec<_> = flattened.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["left.x", "right.x"]);
    }

    #[test]
    fn expand_method_on_manifest_delegates_to_the_free_function() {
        let root = Manifest::from_yaml_str("nodes:\n  - id: solo\n    path: ./solo\n").unwrap();
        let loader = MemoryModuleLoader::new();
        let flattened = root.expand(&base_dir(), &loader).unwrap();
        assert_eq!(flattened.nodes.len(), 1);
    }

    #[test]
    fn a_module_with_no_module_references_is_a_no_op() {
        let root = Manifest::from_yaml_str("nodes:\n  - id: solo\n    path: ./solo\n").unwrap();
        let loader = MemoryModuleLoader::new();
        let flattened = expand(&root, &base_dir(), &loader).unwrap();
        assert_eq!(flattened, root);
    }

    #[test]
    fn dedupe_type_rules_keeps_first_occurrence_order() {
        let a = TypeRule {
            from: crate::Urn::new("a/v1/A"),
            to: crate::Urn::new("a/v1/B"),
        };
        let b = TypeRule {
            from: crate::Urn::new("c/v1/C"),
            to: crate::Urn::new("c/v1/D"),
        };
        let deduped = dedupe_type_rules(vec![a.clone(), b.clone(), a.clone()]);
        assert_eq!(deduped, vec![a, b]);
    }

    #[test]
    fn prune_redundant_env_drops_only_exact_value_matches() {
        let mut manifest =
            Manifest::from_yaml_str("env: { A: root }\nnodes:\n  - id: solo\n    path: ./solo\n")
                .unwrap();
        manifest.nodes[0]
            .env
            .insert("A".to_string(), EnvValue::String("root".to_string()));
        manifest.nodes[0]
            .env
            .insert("B".to_string(), EnvValue::String("different".to_string()));

        prune_redundant_env(&mut manifest);
        assert!(!manifest.nodes[0].env.contains_key("A"));
        assert_eq!(
            manifest.nodes[0].env.get("B"),
            Some(&EnvValue::String("different".to_string()))
        );
    }
}
