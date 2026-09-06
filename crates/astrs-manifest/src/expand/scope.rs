//! The recursive engine behind [`super::expand`]: one call of
//! [`expand_scope`] per manifest "scope" — the root, or one loaded module —
//! fully resolving that scope's own `module:`-hosting nodes (recursively)
//! and its own plain nodes, and handing back a self-contained, locally
//! consistent node list for its caller to absorb (prefixing it by exactly
//! one more id segment) or return as the final result.
//!
//! See [`super`]'s module docs for the algorithm in prose; this file is the
//! literal implementation of that algorithm's two-pass shape (discover +
//! recurse into every `module:`-hosting node first, *then* assemble in the
//! manifest's original node order, so forward references between an
//! ordinary node and a module hosted later in the same file resolve
//! exactly like [`crate::Manifest::validate`] already allows).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::{EnvValue, Manifest, Node, TypeRule, Urn};

use super::error::ExpandError;
use super::rewrite;
use super::{ExpandOptions, MODULE_BOUNDARY_NODE_ID, ModuleLoader};

/// Read-only, non-recursion-varying context threaded through every
/// [`expand_scope`] call — grouped separately from [`RecursionState`] (which
/// *does* change/accumulate per call) purely to keep the function's own
/// parameter list under clippy's `too_many_arguments` threshold.
pub(super) struct Ctx<'a> {
    pub(super) options: &'a ExpandOptions,
    pub(super) loader: &'a dyn ModuleLoader,
}

/// Mutable state threaded through the whole recursive walk: the ancestor
/// path stack (cycle detection, depth limiting) and the flat accumulator
/// of every `type_rules` entry seen at any scope (blueprint §8.2 — a
/// module's own `type_rules` apply graph-wide once it is included, exactly
/// like the root's).
pub(super) struct RecursionState {
    pub(super) stack: Vec<PathBuf>,
    pub(super) type_rules: Vec<TypeRule>,
}

/// What the including scope supplies for one `module:`-hosting node's
/// boundary — the resolved *source* string (already valid in the
/// including scope's own namespace) and, optionally, a declared type for
/// each boundary input the host node's own `inputs`/`input_types` map.
#[derive(Default)]
pub(super) struct Boundary {
    inputs: BTreeMap<String, String>,
    input_types: BTreeMap<String, Urn>,
}

/// Everything about the scope currently being expanded that stays fixed
/// across [`expand_scope`]'s own two passes — grouped so the function
/// signatures around it stay under clippy's `too_many_arguments`
/// threshold.
pub(super) struct ScopeInput<'a> {
    /// The directory this scope's own [`crate::Node::module`] paths are
    /// resolved relative to.
    pub(super) base_dir: &'a Path,
    /// The cumulative environment from every strictly-outer scope
    /// (blueprint §8.5's "parent env overlays module env", already
    /// correctly layered — see [`super`]'s module docs).
    pub(super) ancestor_env: &'a BTreeMap<String, EnvValue>,
    /// This scope's own module boundary — empty for the root manifest,
    /// which is never itself an include target.
    pub(super) boundary: &'a Boundary,
    /// A human-readable label for error messages that have no specific
    /// host node to blame (`"the root manifest"`, or a string naming the
    /// current module manifest's resolved path).
    pub(super) location: &'a str,
}

/// One scope's fully-resolved contribution: a self-contained node list
/// (every internal cross-reference already resolved to a name valid
/// *within this list*) plus the set of ids that list actually uses — the
/// second half is what lets the caller tell "a reference into this scope,
/// which now needs one more prefix segment" apart from "a reference to
/// something outside this scope, already fully resolved, leave alone" when
/// it hoists this result up one level.
pub(super) struct ScopeResult {
    pub(super) nodes: Vec<Node>,
    pub(super) local_ids: BTreeSet<String>,
}

/// Merge two environment layers: `base` first, then every key in
/// `override_layer` wins on conflict — the one primitive both the
/// ancestor-env cumulative (module env overlaid by outer env, blueprint
/// §8.5) and [`crate::Node::effective_env`]'s own node-over-graph rule
/// build on.
fn merge_env(
    base: &BTreeMap<String, EnvValue>,
    override_layer: &BTreeMap<String, EnvValue>,
) -> BTreeMap<String, EnvValue> {
    let mut merged = base.clone();
    merged.extend(override_layer.iter().map(|(k, v)| (k.clone(), v.clone())));
    merged
}

/// Rewrite one `source:`/`record[]` string against this scope's known
/// substitutions, in priority order: an `astrs/...` virtual source (never
/// touched), `_mod/<port>` (boundary substitution — the reason this can
/// fail), a known `{host}/{port}` module-output alias (`output_rewrites`),
/// or otherwise unchanged (an ordinary sibling reference, left for the
/// caller's own prefix pass or for [`crate::Manifest::validate`] to
/// resolve or reject).
///
/// Returns the rewritten string plus, only for a resolved `_mod/<port>`
/// substitution, the boundary's declared type for that port (so the
/// caller can back-fill `input_types` — blueprint requirement: a typed
/// module boundary must not silently become untyped once flattened).
fn rewrite_plain_source(
    source: &str,
    boundary: &Boundary,
    output_rewrites: &BTreeMap<String, String>,
    location: &str,
) -> Result<(String, Option<Urn>), ExpandError> {
    if crate::virtual_source::recognize(source).is_some() {
        return Ok((source.to_string(), None));
    }

    if let Some(port) = source
        .strip_prefix(MODULE_BOUNDARY_NODE_ID)
        .and_then(|rest| rest.strip_prefix('/'))
    {
        return match boundary.inputs.get(port) {
            Some(value) => Ok((value.clone(), boundary.input_types.get(port).cloned())),
            None => Err(ExpandError::UnsuppliedModuleInput {
                location: location.to_string(),
                port: port.to_string(),
            }),
        };
    }

    if let Some(replacement) = output_rewrites.get(source) {
        return Ok((replacement.clone(), None));
    }

    Ok((source.to_string(), None))
}

/// Recursively expand one manifest scope. See this module's and
/// [`super`]'s docs for the full algorithm.
pub(super) fn expand_scope(
    ctx: &Ctx<'_>,
    state: &mut RecursionState,
    manifest: &Manifest,
    input: &ScopeInput<'_>,
) -> Result<ScopeResult, ExpandError> {
    state.type_rules.extend(manifest.type_rules.iter().cloned());

    // Pass 1: discover and recursively expand every `module:`-hosting
    // node, in original order, before touching any plain node — so a
    // plain node's forward reference to a module's exposed output (built
    // into `output_rewrites` here) resolves regardless of declaration
    // order, matching `Manifest::validate`'s own forward-reference
    // tolerance. A module-hosting node's *own* `inputs` are resolved
    // against `output_rewrites` as it stands at the point that node is
    // reached (see `expand_module_host`) — unlike plain nodes, a module
    // host's reference to a *later*-declared sibling module's exposed
    // output is a known, documented limitation, not a silent miscompile:
    // it passes through unresolved and surfaces from a subsequent
    // `Manifest::validate` call.
    let mut module_results: BTreeMap<usize, Vec<Node>> = BTreeMap::new();
    let mut output_rewrites: BTreeMap<String, String> = BTreeMap::new();

    for (index, host) in manifest.nodes.iter().enumerate() {
        let Some(module_field) = &host.module else {
            continue;
        };
        let mut prefixed =
            expand_module_host(ctx, state, input, &output_rewrites, host, module_field)?;
        record_output_rewrites(host, &mut prefixed, &mut output_rewrites)?;
        module_results.insert(index, prefix_child_nodes(host, prefixed));
    }

    // Pass 2: bake env and rewrite `source:`/`record[]` strings for every
    // plain (non-module) node, now that `output_rewrites` is complete for
    // every module hosted directly in this scope.
    let mut plain_results: BTreeMap<usize, Node> = BTreeMap::new();
    for (index, node) in manifest.nodes.iter().enumerate() {
        if node.module.is_some() {
            continue;
        }
        let mut node = node.clone();
        node.env = merge_env(input.ancestor_env, &node.env);
        rewrite_plain_node(&mut node, input.boundary, &output_rewrites, input.location)?;
        plain_results.insert(index, node);
    }

    // Assembly: walk the original node order, splicing in each module
    // host's (already-prefixed) children in place, detecting any id
    // collision as we go (blueprint requirement: expansion must never
    // silently produce a manifest with duplicate ids, whether the
    // duplicate already existed pre-expansion or was introduced by
    // prefixing — see this crate's module docs for a worked example of
    // the latter).
    let mut nodes = Vec::with_capacity(manifest.nodes.len());
    let mut local_ids = BTreeSet::new();
    for index in 0..manifest.nodes.len() {
        let batch = match plain_results.remove(&index) {
            Some(node) => vec![node],
            None => module_results.remove(&index).unwrap_or_default(),
        };
        for node in batch {
            if !local_ids.insert(node.id.clone()) {
                return Err(ExpandError::DuplicateNodeId {
                    location: input.location.to_string(),
                    id: node.id,
                });
            }
            nodes.push(node);
        }
    }

    Ok(ScopeResult { nodes, local_ids })
}

/// Resolve, load, cycle/depth-check, and recursively expand one
/// `module:`-hosting node's referenced manifest, returning its
/// (not-yet-prefixed) [`ScopeResult`].
///
/// `host`'s own `inputs` values are resolved against `input`'s boundary
/// and `output_rewrites_so_far` — the *current* scope's own substitutions
/// — before being handed down as the recursive call's boundary: `host`
/// may itself use `_mod/<port>` (referencing *this* scope's own boundary,
/// one level further out) or `<sibling>/<port>` (an earlier-declared
/// sibling module's exposed output), and either must already be resolved
/// to something meaningful in the *including* scope's namespace by the
/// time it becomes the nested scope's boundary — passing it down
/// unresolved would leave a literal `_mod/...` string for the nested
/// scope to (incorrectly) treat as already-external.
fn expand_module_host(
    ctx: &Ctx<'_>,
    state: &mut RecursionState,
    input: &ScopeInput<'_>,
    output_rewrites_so_far: &BTreeMap<String, String>,
    host: &Node,
    module_field: &str,
) -> Result<ScopeResult, ExpandError> {
    let child_path = rewrite::resolve_module_path(input.base_dir, module_field);

    if let Some(pos) = state.stack.iter().position(|p| p == &child_path) {
        let mut chain = state.stack[pos..].to_vec();
        chain.push(child_path);
        return Err(ExpandError::Cycle { chain });
    }
    if state.stack.len() >= ctx.options.max_depth {
        return Err(ExpandError::DepthExceeded {
            max_depth: ctx.options.max_depth,
        });
    }

    let text = ctx
        .loader
        .read_to_string(&child_path)
        .map_err(|source| ExpandError::Io {
            path: child_path.clone(),
            source,
        })?;
    let child_manifest = Manifest::from_yaml_str(&text).map_err(|source| ExpandError::Parse {
        path: child_path.clone(),
        source,
    })?;
    let header = child_manifest
        .module
        .clone()
        .ok_or_else(|| ExpandError::NotAModule {
            path: child_path.clone(),
        })?;

    for port in host.inputs.keys() {
        if !header.inputs.iter().any(|p| p == port) {
            return Err(ExpandError::UnknownModuleInput {
                host_node: host.id.clone(),
                port: port.clone(),
            });
        }
    }
    for port in &host.outputs {
        if !header.outputs.iter().any(|p| p == port) {
            return Err(ExpandError::UnknownModuleOutput {
                host_node: host.id.clone(),
                port: port.clone(),
            });
        }
    }

    // Resolve `host`'s own `inputs` values (and, where the resolution came
    // from *this* scope's own boundary substitution, the type that
    // substitution carried) before handing them down as the nested
    // scope's boundary — see this function's docs. An inherited type
    // hint only fills a port `host` does not itself annotate directly:
    // `host.input_types` (an explicit declaration on the very node doing
    // the including) is always the more specific source of truth.
    let mut resolved_inputs = BTreeMap::new();
    let mut child_input_types = BTreeMap::new();
    for (port, value) in &host.inputs {
        let (resolved, type_hint) = rewrite_plain_source(
            &value.source,
            input.boundary,
            output_rewrites_so_far,
            input.location,
        )?;
        resolved_inputs.insert(port.clone(), resolved);
        if let Some(ty) = type_hint {
            child_input_types.insert(port.clone(), ty);
        }
    }
    for (port, ty) in &host.input_types {
        if header.inputs.iter().any(|p| p == port) {
            child_input_types.insert(port.clone(), ty.clone());
        }
    }
    let child_boundary = Boundary {
        inputs: resolved_inputs,
        input_types: child_input_types,
    };
    let child_ancestor_env = merge_env(&child_manifest.env, input.ancestor_env);
    let child_base_dir = child_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| input.base_dir.to_path_buf());
    let child_location = format!("module manifest `{}`", child_path.display());
    let child_input = ScopeInput {
        base_dir: &child_base_dir,
        ancestor_env: &child_ancestor_env,
        boundary: &child_boundary,
        location: &child_location,
    };

    state.stack.push(child_path);
    let result = expand_scope(ctx, state, &child_manifest, &child_input);
    state.stack.pop();
    result
}

/// For each output `host` exposes, find the unique internal (module-local)
/// producer — a node declaring an output literally named `<port>` or
/// `_mod/<port>` — register `{host.id}/{port}` -> `{host.id}.{producer}/
/// {spelling}` in `output_rewrites`, and back-fill `host`'s
/// `output_types[port]` (if declared) onto the producer's own
/// `output_types` when the producer does not already annotate it.
fn record_output_rewrites(
    host: &Node,
    child: &mut ScopeResult,
    output_rewrites: &mut BTreeMap<String, String>,
) -> Result<(), ExpandError> {
    for port in &host.outputs {
        let mod_spelling = format!("{MODULE_BOUNDARY_NODE_ID}/{port}");
        let mut producers: Vec<(String, String)> = Vec::new();
        for candidate in &child.nodes {
            if candidate.outputs.iter().any(|o| o == port) {
                producers.push((candidate.id.clone(), port.clone()));
            }
            if candidate.outputs.iter().any(|o| o == &mod_spelling) {
                producers.push((candidate.id.clone(), mod_spelling.clone()));
            }
        }

        let (producer_id, spelling) = match producers.len() {
            0 => {
                return Err(ExpandError::MissingModuleOutputProducer {
                    host_node: host.id.clone(),
                    port: port.clone(),
                });
            }
            1 => producers.remove(0),
            _ => {
                return Err(ExpandError::AmbiguousModuleOutput {
                    host_node: host.id.clone(),
                    port: port.clone(),
                    producers: producers.into_iter().map(|(id, _)| id).collect(),
                });
            }
        };

        output_rewrites.insert(
            format!("{}/{port}", host.id),
            format!("{}.{producer_id}/{spelling}", host.id),
        );

        if let Some(ty) = host.output_types.get(port)
            && let Some(producer_node) = child.nodes.iter_mut().find(|n| n.id == producer_id)
        {
            producer_node
                .output_types
                .entry(spelling)
                .or_insert_with(|| ty.clone());
        }
    }
    Ok(())
}

/// Prefix every node `child` contributes with `host.id` and re-target any
/// of its own internal cross-references that pointed at a node local to
/// `child`'s own scope — the composable "one more `.segment`" step that
/// makes multi-level nesting produce `grandparent.parent.child`-style ids.
fn prefix_child_nodes(host: &Node, child: ScopeResult) -> Vec<Node> {
    let ScopeResult { nodes, local_ids } = child;
    let prefix = host.id.as_str();
    let rename = |id: &str| local_ids.contains(id).then(|| format!("{prefix}.{id}"));

    nodes
        .into_iter()
        .map(|mut node| {
            node.id = format!("{prefix}.{}", node.id);
            for input in node.inputs.values_mut() {
                input.source = rewrite::rewrite_reference_node_id(&input.source, rename);
            }
            if let Some(record) = &mut node.record {
                for entry in record.iter_mut() {
                    *entry = rewrite::rewrite_reference_node_id(entry, rename);
                }
            }
            node
        })
        .collect()
}

/// Rewrite `node`'s `inputs`/`record` source strings against this scope's
/// boundary and module-output substitutions (env baking is the caller's
/// job, done before this is called), back-filling `input_types` for any
/// boundary substitution that carried a declared type.
fn rewrite_plain_node(
    node: &mut Node,
    boundary: &Boundary,
    output_rewrites: &BTreeMap<String, String>,
    location: &str,
) -> Result<(), ExpandError> {
    let mut type_hints: Vec<(String, Urn)> = Vec::new();
    for (name, input) in node.inputs.iter_mut() {
        let (new_source, type_hint) =
            rewrite_plain_source(&input.source, boundary, output_rewrites, location)?;
        input.source = new_source;
        if let Some(ty) = type_hint {
            type_hints.push((name.clone(), ty));
        }
    }
    for (name, ty) in type_hints {
        node.input_types.entry(name).or_insert(ty);
    }

    if let Some(record) = &mut node.record {
        for entry in record.iter_mut() {
            let (new_source, _) = rewrite_plain_source(entry, boundary, output_rewrites, location)?;
            *entry = new_source;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn merge_env_lets_override_layer_win() {
        let mut base = BTreeMap::new();
        base.insert("A".to_string(), EnvValue::String("base".to_string()));
        base.insert("B".to_string(), EnvValue::Int(1));
        let mut over = BTreeMap::new();
        over.insert("A".to_string(), EnvValue::String("over".to_string()));

        let merged = merge_env(&base, &over);
        assert_eq!(merged.get("A"), Some(&EnvValue::String("over".to_string())));
        assert_eq!(merged.get("B"), Some(&EnvValue::Int(1)));
    }

    #[test]
    fn rewrite_plain_source_leaves_virtual_sources_untouched() {
        let boundary = Boundary::default();
        let (out, ty) =
            rewrite_plain_source("astrs/timer/hz/50", &boundary, &BTreeMap::new(), "x").unwrap();
        assert_eq!(out, "astrs/timer/hz/50");
        assert!(ty.is_none());
    }

    #[test]
    fn rewrite_plain_source_substitutes_a_supplied_boundary_input() {
        let mut boundary = Boundary::default();
        boundary
            .inputs
            .insert("frames".to_string(), "camera/frames".to_string());
        boundary
            .input_types
            .insert("frames".to_string(), Urn::new("std/media/v1/Image"));

        let (out, ty) =
            rewrite_plain_source("_mod/frames", &boundary, &BTreeMap::new(), "x").unwrap();
        assert_eq!(out, "camera/frames");
        assert_eq!(ty, Some(Urn::new("std/media/v1/Image")));
    }

    #[test]
    fn rewrite_plain_source_errors_on_unsupplied_boundary_input() {
        let boundary = Boundary::default();
        let err =
            rewrite_plain_source("_mod/frames", &boundary, &BTreeMap::new(), "loc").unwrap_err();
        assert!(matches!(
            err,
            ExpandError::UnsuppliedModuleInput { ref location, ref port }
                if location == "loc" && port == "frames"
        ));
    }

    #[test]
    fn rewrite_plain_source_applies_a_known_output_rewrite() {
        let mut rewrites = BTreeMap::new();
        rewrites.insert(
            "perception/detections".to_string(),
            "perception.detector/detections".to_string(),
        );
        let boundary = Boundary::default();
        let (out, _) =
            rewrite_plain_source("perception/detections", &boundary, &rewrites, "x").unwrap();
        assert_eq!(out, "perception.detector/detections");
    }

    #[test]
    fn rewrite_plain_source_leaves_unrelated_references_untouched() {
        let boundary = Boundary::default();
        let (out, _) =
            rewrite_plain_source("camera/frames", &boundary, &BTreeMap::new(), "x").unwrap();
        assert_eq!(out, "camera/frames");
    }
}
