//! The manifest validation pass (blueprint §8).
//!
//! Unlike parsing (§[`crate::Manifest::from_yaml_str`]), which fails fast
//! on the first syntax problem, [`validate`] collects **every** structural
//! violation in one pass, each tagged with a document path such as
//! `nodes[3].inputs.frames` — so an editor/CI integration can report a
//! manifest's *entire* problem list at once instead of a fix-one-rerun
//! loop.
//!
//! The pass is a fixed pipeline of independent `check_*` functions run
//! from [`validate`], each appending to a shared error list. A follow-up
//! revision should add another `check_*` function to that pipeline rather
//! than editing the existing ones.
//!
//! Checks performed:
//!
//! 1. [`check_node_ids`] — id charset `[a-zA-Z0-9_.-]+` and uniqueness.
//! 2. [`check_node_sources`] — exactly one source kind per node, `git`
//!    ref-selector exclusivity, the `dynamic` sentinel vs. `git` conflict.
//! 3. [`check_inputs`] — `queue_size >= 1`, input source resolution. A
//!    `source: _mod/<port>` reference (blueprint §8.5) resolves iff this
//!    manifest carries a [`crate::ModuleHeader`] (`manifest.module`) and
//!    `<port>` is one of its declared `inputs` — see [`NodeIndex::resolve`]
//!    — so a standalone module manifest validates cleanly on its own,
//!    before it is ever included by [`crate::expand`].
//! 4. [`check_record`] — `record:` sugar entries resolve with the same
//!    rules as ordinary inputs.
//! 5. [`check_restart_consistency`] — `max_restarts` requires
//!    `restart_policy != never`.
//! 6. [`check_urns`] — `input_types`/`output_types` URN syntax.
//! 7. [`check_type_rules`] — `type_rules[].from`/`.to` URN syntax.
//! 8. [`check_cpu_affinity`] — non-empty when present.
//! 9. [`check_type_annotations_match_ports`] — every `input_types`/
//!    `output_types` key names a port this node actually declares.
//! 10. [`check_hub_names`] — a `hub:` package name is non-empty and matches
//!     `[a-z0-9-]+`, on nodes and on operator entries alike.
//! 11. [`check_rt`] — an `rt:` block's policy/priority pairing: a real-time
//!     policy requires a priority in `1..=99`; `normal` rejects one outright.
//! 12. [`check_operator_locators`] — an operator entry declares at most one
//!     of `dylib`/`wasm`/`hub`.
//!
//! # Deliberately not checked
//!
//! [`crate::node::OperatorConfig`]'s own `inputs` (under a node's
//! `operators:` source, §9.3) are **not** resolved against this manifest's
//! node/output index. An operator's `source:` may legitimately name a
//! sibling *operator* id hosted in the same runtime process (for example,
//! `boxes: crop/boxes` where `crop` is another entry in the same node's
//! `operators:` list, not a top-level node) — a graph unknown to this
//! validation pass, which only indexes top-level [`crate::Node`] ids and
//! outputs. Resolving intra-runtime operator wiring is `astrs-runtime`'s
//! concern (§9.3), not the manifest's.

use std::collections::BTreeMap;
use std::fmt;

use crate::error::PathBuf;
use crate::module_header::MODULE_BOUNDARY_NODE_ID;
use crate::virtual_source::VirtualSourceError;
use crate::{Manifest, Node, Urn};

/// One structural validation failure, tagged with its location in the
/// manifest document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    /// A document path such as `nodes[3].inputs.frames`, pinpointing where
    /// the problem was found.
    pub path: String,
    /// The specific violation.
    pub kind: ValidationErrorKind,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.kind)
    }
}

impl std::error::Error for ValidationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.kind)
    }
}

/// The specific kind of structural violation a [`ValidationError`] reports.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ValidationErrorKind {
    /// The node id contains characters outside `[a-zA-Z0-9_.-]+`, or is empty.
    #[error("invalid node id `{id}`: ids must be non-empty and match [a-zA-Z0-9_.-]+")]
    InvalidIdCharset {
        /// The offending id.
        id: String,
    },

    /// Two or more nodes declared the same `id`.
    #[error("duplicate node id `{id}`")]
    DuplicateNodeId {
        /// The repeated id.
        id: String,
    },

    /// A node declared none of
    /// `path`/`git`/`hub`/`module`/`operators`/`ros2`/`record`.
    #[error(
        "node declares no source; exactly one of \
         path/git/hub/module/operators/ros2/record is required"
    )]
    NoSourceDeclared,

    /// A node declared more than one source kind.
    #[error("node declares multiple sources ({found:?}); exactly one is required")]
    MultipleSourcesDeclared {
        /// The source kinds found present, e.g. `["git", "module"]`.
        found: Vec<&'static str>,
    },

    /// `branch`/`tag`/`rev` was set without `git`.
    #[error("`{field}` requires `git` to also be set")]
    GitFieldWithoutGit {
        /// The offending field name.
        field: &'static str,
    },

    /// More than one of `branch`/`tag`/`rev` was set alongside `git`.
    #[error("at most one of `branch`, `tag`, `rev` may be set (found {found:?})")]
    ConflictingGitRefs {
        /// The ref-selector field names found present.
        found: Vec<&'static str>,
    },

    /// `path: dynamic` was combined with `git`.
    #[error(
        "the `dynamic` path sentinel cannot be combined with `git` (a git-sourced node's `path` \
         names its in-repo build artifact, not an external-attach point)"
    )]
    DynamicSentinelWithGit,

    /// An input's `queue_size` was `0`.
    #[error("queue_size must be >= 1, found {found}")]
    QueueSizeTooSmall {
        /// The offending queue size.
        found: u32,
    },

    /// `max_restarts` was set while the effective `restart_policy` is `never`.
    #[error(
        "max_restarts is set but restart_policy is `never` (or unset); set restart_policy to \
         `on_failure` or `always`"
    )]
    MaxRestartsRequiresPolicy,

    /// A type URN failed syntax validation.
    #[error("invalid type URN `{urn}`: {reason}")]
    InvalidUrn {
        /// The offending URN string.
        urn: String,
        /// Why it failed validation.
        reason: String,
    },

    /// `cpu_affinity` was present but an empty list.
    #[error("cpu_affinity is present but empty; omit the field or list at least one core index")]
    EmptyCpuAffinity,

    /// An input or `record:` entry did not resolve to a declared output or
    /// a valid virtual source.
    #[error("unresolved reference `{value}`: {reason}")]
    UnresolvedReference {
        /// The offending reference string.
        value: String,
        /// Why it failed to resolve.
        reason: UnresolvedReferenceReason,
    },

    /// A `hub:` source's package name is empty or carries characters outside
    /// `[a-z0-9-]`.
    #[error("invalid hub package name `{name}`: names must be non-empty and match [a-z0-9-]+")]
    InvalidHubName {
        /// The offending package name.
        name: String,
    },

    /// An `rt:` block set a real-time policy (`fifo`/`rr`) with no
    /// `priority`. See [`crate::RtConfig`] for why there is no defensible
    /// default to fall back on.
    #[error(
        "rt.policy is `{policy}` but no priority is set; a real-time policy requires an explicit \
         priority in {min}..={max}"
    )]
    RtPriorityRequired {
        /// The real-time policy that was set.
        policy: crate::RtPolicy,
        /// [`crate::RT_PRIORITY_MIN`].
        min: u8,
        /// [`crate::RT_PRIORITY_MAX`].
        max: u8,
    },

    /// An `rt:` block set a `priority` outside
    /// [`crate::RT_PRIORITY_MIN`]`..=`[`crate::RT_PRIORITY_MAX`].
    #[error("rt.priority must be in {min}..={max}, found {found}")]
    RtPriorityOutOfRange {
        /// The offending priority.
        found: u8,
        /// [`crate::RT_PRIORITY_MIN`].
        min: u8,
        /// [`crate::RT_PRIORITY_MAX`].
        max: u8,
    },

    /// An `rt:` block set a `priority` alongside `policy: normal`, which has
    /// no priority axis at all — see [`crate::RtConfig`].
    #[error(
        "rt.priority is set but rt.policy is `normal`, which has no priority axis; use \
         `fifo`/`rr`, or drop the priority"
    )]
    RtPriorityWithNormalPolicy,

    /// An operator entry declared more than one of `dylib`/`wasm`/`hub`.
    #[error("operator declares multiple locators ({found:?}); at most one is allowed")]
    MultipleOperatorLocators {
        /// The locator field names found present, e.g. `["dylib", "wasm"]`.
        found: Vec<&'static str>,
    },

    /// An `input_types`/`output_types` key named a port (`inputs`/
    /// `outputs` entry) this node does not declare — most often a typo,
    /// which would otherwise silently annotate nothing rather than error
    /// (the "config that lies" failure mode blueprint §2.2 exists to
    /// eliminate, here in a new field rather than a dead one).
    #[error(
        "`{map}` declares a type for `{name}`, but this node has no {port_kind} named `{name}`"
    )]
    TypeForUndeclaredPort {
        /// Which map the stray entry was found in: `"input_types"` or
        /// `"output_types"`.
        map: &'static str,
        /// What kind of port was expected: `"input"` or `"output"`.
        port_kind: &'static str,
        /// The port name the map key names.
        name: String,
    },
}

/// Why a `node/output`-or-virtual-source reference failed to resolve.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum UnresolvedReferenceReason {
    /// The string was neither `<node>/<output>` nor `astrs/...`.
    #[error("expected `node/output` or an `astrs/...` virtual source")]
    Malformed,
    /// No node with this id is declared in the manifest.
    #[error("no node with id `{0}` is declared in this manifest")]
    UnknownNode(String),
    /// The named node exists but does not declare this output.
    #[error("node `{node}` does not declare an output named `{output}`")]
    UnknownOutput {
        /// The referenced node id.
        node: String,
        /// The output name that was not found.
        output: String,
    },
    /// `astrs/...` did not match `timer`, `logs`, or `status`.
    #[error("unknown virtual source family (expected astrs/timer/*, astrs/logs*, or astrs/status)")]
    UnknownVirtualFamily,
    /// `astrs/timer/...` was malformed.
    #[error("invalid virtual timer source: {0}")]
    InvalidTimer(String),
    /// `astrs/logs...` was malformed.
    #[error("invalid virtual log source: {0}")]
    InvalidLogs(String),
    /// `astrs/status...` was malformed.
    #[error("invalid virtual status source: {0}")]
    InvalidStatus(String),
    /// `_mod/<port>` was used, this manifest *is* a module (carries a
    /// [`crate::ModuleHeader`]), but `<port>` is not one of its declared
    /// `inputs` — most often a typo of the module header's `inputs` list.
    #[error(
        "`{port}` is not a declared boundary input of this module (declared inputs: {declared:?})"
    )]
    UnknownModuleInput {
        /// The offending port name after `_mod/`.
        port: String,
        /// This module's actually-declared boundary input names.
        declared: Vec<String>,
    },
}

impl From<VirtualSourceError> for UnresolvedReferenceReason {
    fn from(e: VirtualSourceError) -> Self {
        match e {
            VirtualSourceError::UnknownFamily => Self::UnknownVirtualFamily,
            VirtualSourceError::InvalidTimer(m) => Self::InvalidTimer(m),
            VirtualSourceError::InvalidLogs(m) => Self::InvalidLogs(m),
            VirtualSourceError::InvalidStatus(m) => Self::InvalidStatus(m),
        }
    }
}

/// A non-empty(-on-failure) collection of [`ValidationError`]s, returned by
/// [`crate::Manifest::validate`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationErrors(pub(crate) Vec<ValidationError>);

impl ValidationErrors {
    /// The individual errors, in the order the validation pass found them
    /// (deterministic: node order, then field-declaration order).
    #[must_use]
    pub fn errors(&self) -> &[ValidationError] {
        &self.0
    }

    /// Whether there are no errors (never actually observed on a value
    /// returned by [`crate::Manifest::validate`], which returns `Ok(())`
    /// instead of an empty `ValidationErrors` — provided for completeness
    /// when a caller builds one manually).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The number of errors.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, e) in self.0.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{e}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

impl IntoIterator for ValidationErrors {
    type Item = ValidationError;
    type IntoIter = std::vec::IntoIter<ValidationError>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a ValidationErrors {
    type Item = &'a ValidationError;
    type IntoIter = std::slice::Iter<'a, ValidationError>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Run every structural check against `manifest`, returning every
/// violation found. See this module's top-level docs for the check list.
pub(crate) fn validate(manifest: &Manifest) -> Result<(), ValidationErrors> {
    let mut errors = Vec::new();

    check_node_ids(manifest, &mut errors);
    check_node_sources(manifest, &mut errors);

    let index = NodeIndex::build(manifest);
    check_inputs(manifest, &index, &mut errors);
    check_record(manifest, &index, &mut errors);

    check_restart_consistency(manifest, &mut errors);
    check_urns(manifest, &mut errors);
    check_type_rules(manifest, &mut errors);
    check_cpu_affinity(manifest, &mut errors);
    check_type_annotations_match_ports(manifest, &mut errors);
    check_hub_names(manifest, &mut errors);
    check_rt(manifest, &mut errors);
    check_operator_locators(manifest, &mut errors);

    if errors.is_empty() {
        Ok(())
    } else {
        Err(ValidationErrors(errors))
    }
}

/// Check 1: id charset and uniqueness.
fn check_node_ids(manifest: &Manifest, errors: &mut Vec<ValidationError>) {
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, node) in manifest.nodes.iter().enumerate() {
        let path = PathBuf::new("nodes").index(i).join("id");

        if !is_valid_id_charset(&node.id) {
            errors.push(ValidationError {
                path: path.clone().into(),
                kind: ValidationErrorKind::InvalidIdCharset {
                    id: node.id.clone(),
                },
            });
        }

        if seen.insert(node.id.as_str(), i).is_some() {
            errors.push(ValidationError {
                path: path.into(),
                kind: ValidationErrorKind::DuplicateNodeId {
                    id: node.id.clone(),
                },
            });
        }
    }
}

fn is_valid_id_charset(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Check 2: source-kind exclusivity.
///
/// `git` and `path` are **not** competing kinds: §8.1's `detector` node
/// sets both (`git` to fetch the repo, `path` as the in-repo build
/// artifact), so `path` only counts as its own source kind when `git` is
/// absent. See the [`Node`] struct docs for the full source-kind list.
fn check_node_sources(manifest: &Manifest, errors: &mut Vec<ValidationError>) {
    for (i, node) in manifest.nodes.iter().enumerate() {
        let node_path = PathBuf::new("nodes").index(i);
        let has_git = node.git.is_some();

        let mut kinds: Vec<&'static str> = Vec::new();
        if has_git {
            kinds.push("git");
        }
        if node.path.is_some() && !has_git {
            kinds.push("path");
        }
        if node.hub.is_some() {
            kinds.push("hub");
        }
        if node.module.is_some() {
            kinds.push("module");
        }
        if node.operators.is_some() {
            kinds.push("operators");
        }
        if node.ros2.is_some() {
            kinds.push("ros2");
        }
        if node.record.is_some() {
            kinds.push("record");
        }

        match kinds.len() {
            0 => errors.push(ValidationError {
                path: node_path.clone().into(),
                kind: ValidationErrorKind::NoSourceDeclared,
            }),
            1 => {}
            _ => errors.push(ValidationError {
                path: node_path.clone().into(),
                kind: ValidationErrorKind::MultipleSourcesDeclared { found: kinds },
            }),
        }

        check_git_ref_fields(node, &node_path, has_git, errors);

        if has_git && node.is_dynamic_path() {
            errors.push(ValidationError {
                path: node_path.join("path").into(),
                kind: ValidationErrorKind::DynamicSentinelWithGit,
            });
        }
    }
}

fn check_git_ref_fields(
    node: &Node,
    node_path: &PathBuf,
    has_git: bool,
    errors: &mut Vec<ValidationError>,
) {
    let ref_fields: [(&'static str, bool); 3] = [
        ("branch", node.branch.is_some()),
        ("tag", node.tag.is_some()),
        ("rev", node.rev.is_some()),
    ];

    if !has_git {
        for (field, present) in ref_fields {
            if present {
                errors.push(ValidationError {
                    path: node_path.join(field).into(),
                    kind: ValidationErrorKind::GitFieldWithoutGit { field },
                });
            }
        }
        return;
    }

    let present_refs: Vec<&'static str> = ref_fields
        .into_iter()
        .filter(|(_, present)| *present)
        .map(|(field, _)| field)
        .collect();
    if present_refs.len() > 1 {
        errors.push(ValidationError {
            path: node_path.join("git").into(),
            kind: ValidationErrorKind::ConflictingGitRefs {
                found: present_refs,
            },
        });
    }
}

/// Check 3: `queue_size >= 1` and input source resolution.
fn check_inputs(manifest: &Manifest, index: &NodeIndex<'_>, errors: &mut Vec<ValidationError>) {
    for (i, node) in manifest.nodes.iter().enumerate() {
        let node_path = PathBuf::new("nodes").index(i);
        for (name, input) in &node.inputs {
            let field_path = node_path.join("inputs").join(name);

            if input.queue_size < 1 {
                errors.push(ValidationError {
                    path: field_path.clone().into(),
                    kind: ValidationErrorKind::QueueSizeTooSmall {
                        found: input.queue_size,
                    },
                });
            }

            if let Err(reason) = index.resolve(&input.source) {
                errors.push(ValidationError {
                    path: field_path.into(),
                    kind: ValidationErrorKind::UnresolvedReference {
                        value: input.source.clone(),
                        reason,
                    },
                });
            }
        }
    }
}

/// Check 4: `record:` sugar entries resolve with the same rules as inputs.
fn check_record(manifest: &Manifest, index: &NodeIndex<'_>, errors: &mut Vec<ValidationError>) {
    for (i, node) in manifest.nodes.iter().enumerate() {
        let Some(record) = &node.record else {
            continue;
        };
        let record_path = PathBuf::new("nodes").index(i).join("record");
        for (j, value) in record.iter().enumerate() {
            if let Err(reason) = index.resolve(value) {
                errors.push(ValidationError {
                    path: record_path.index(j).into(),
                    kind: ValidationErrorKind::UnresolvedReference {
                        value: value.clone(),
                        reason,
                    },
                });
            }
        }
    }
}

/// Check 5: `max_restarts` requires `restart_policy != never`.
fn check_restart_consistency(manifest: &Manifest, errors: &mut Vec<ValidationError>) {
    for (i, node) in manifest.nodes.iter().enumerate() {
        if node.max_restarts.is_some() && !node.effective_restart_policy().restarts_at_all() {
            errors.push(ValidationError {
                path: PathBuf::new("nodes").index(i).join("max_restarts").into(),
                kind: ValidationErrorKind::MaxRestartsRequiresPolicy,
            });
        }
    }
}

/// Check 6: `input_types`/`output_types` URN syntax.
fn check_urns(manifest: &Manifest, errors: &mut Vec<ValidationError>) {
    for (i, node) in manifest.nodes.iter().enumerate() {
        let node_path = PathBuf::new("nodes").index(i);
        for (name, urn) in &node.input_types {
            check_one_urn(urn, node_path.join("input_types").join(name), errors);
        }
        for (name, urn) in &node.output_types {
            check_one_urn(urn, node_path.join("output_types").join(name), errors);
        }
    }
}

/// Check 7: `type_rules[].from`/`.to` URN syntax.
fn check_type_rules(manifest: &Manifest, errors: &mut Vec<ValidationError>) {
    for (i, rule) in manifest.type_rules.iter().enumerate() {
        let rule_path = PathBuf::new("type_rules").index(i);
        check_one_urn(&rule.from, rule_path.join("from"), errors);
        check_one_urn(&rule.to, rule_path.join("to"), errors);
    }
}

fn check_one_urn(urn: &Urn, path: PathBuf, errors: &mut Vec<ValidationError>) {
    if let Err(e) = urn.validate() {
        errors.push(ValidationError {
            path: path.into(),
            kind: ValidationErrorKind::InvalidUrn {
                urn: urn.as_str().to_string(),
                reason: e.to_string(),
            },
        });
    }
}

/// Check 8: `cpu_affinity` non-empty when present.
fn check_cpu_affinity(manifest: &Manifest, errors: &mut Vec<ValidationError>) {
    for (i, node) in manifest.nodes.iter().enumerate() {
        if let Some(affinity) = &node.cpu_affinity
            && affinity.is_empty()
        {
            errors.push(ValidationError {
                path: PathBuf::new("nodes").index(i).join("cpu_affinity").into(),
                kind: ValidationErrorKind::EmptyCpuAffinity,
            });
        }
    }
}

/// Check 9: `input_types`/`output_types` keys must each name a port this
/// node actually declares in `inputs`/`outputs`.
///
/// A manifest may reasonably leave a port untyped (an absent entry means
/// "no declared type," which is fine — typed-by-default is a *default*,
/// not a mandate, per blueprint §3.7's "typed by default, dynamic by
/// consent"). What is never reasonable is a type annotation for a port
/// that does not exist at all: `output_types: { frmaes: ... }` next to
/// `outputs: [frames]` is a typo that would otherwise be accepted and
/// silently do nothing, forever.
fn check_type_annotations_match_ports(manifest: &Manifest, errors: &mut Vec<ValidationError>) {
    for (i, node) in manifest.nodes.iter().enumerate() {
        let node_path = PathBuf::new("nodes").index(i);

        for name in node.input_types.keys() {
            if !node.inputs.contains_key(name) {
                errors.push(ValidationError {
                    path: node_path.join("input_types").join(name).into(),
                    kind: ValidationErrorKind::TypeForUndeclaredPort {
                        map: "input_types",
                        port_kind: "input",
                        name: name.clone(),
                    },
                });
            }
        }

        for name in node.output_types.keys() {
            if !node.outputs.iter().any(|output| output == name) {
                errors.push(ValidationError {
                    path: node_path.join("output_types").join(name).into(),
                    kind: ValidationErrorKind::TypeForUndeclaredPort {
                        map: "output_types",
                        port_kind: "output",
                        name: name.clone(),
                    },
                });
            }
        }
    }
}

/// Check 10: every `hub:` package name — on a node, and on each of its
/// operator entries — is non-empty and matches `[a-z0-9-]+`.
///
/// The revision half is deliberately *not* checked: a tag, a branch, a commit
/// hash and a content digest are all legitimate, and only the hub client can
/// say which of them resolves. The name is different — it is what the index
/// is keyed by, and a name outside the charset can never match anything.
fn check_hub_names(manifest: &Manifest, errors: &mut Vec<ValidationError>) {
    for (i, node) in manifest.nodes.iter().enumerate() {
        let node_path = PathBuf::new("nodes").index(i);

        if let Some(hub) = &node.hub
            && !hub.has_valid_name()
        {
            errors.push(ValidationError {
                path: node_path.join("hub").into(),
                kind: ValidationErrorKind::InvalidHubName {
                    name: hub.name.clone(),
                },
            });
        }

        for (j, operator) in node.operators.iter().flatten().enumerate() {
            if let Some(hub) = &operator.hub
                && !hub.has_valid_name()
            {
                errors.push(ValidationError {
                    path: node_path.join("operators").index(j).join("hub").into(),
                    kind: ValidationErrorKind::InvalidHubName {
                        name: hub.name.clone(),
                    },
                });
            }
        }
    }
}

/// Check 11: an `rt:` block's policy/priority pairing (§11.3).
///
/// A real-time policy (`fifo`/`rr`) requires an explicit priority in
/// [`crate::RT_PRIORITY_MIN`]`..=`[`crate::RT_PRIORITY_MAX`]; `normal` rejects
/// one outright. See [`crate::RtConfig`] for why neither half has a
/// defensible default.
fn check_rt(manifest: &Manifest, errors: &mut Vec<ValidationError>) {
    for (i, node) in manifest.nodes.iter().enumerate() {
        let Some(rt) = &node.rt else {
            continue;
        };
        let rt_path = PathBuf::new("nodes").index(i).join("rt");

        if rt.policy.is_realtime() {
            match rt.priority {
                None => errors.push(ValidationError {
                    path: rt_path.join("priority").into(),
                    kind: ValidationErrorKind::RtPriorityRequired {
                        policy: rt.policy,
                        min: crate::RT_PRIORITY_MIN,
                        max: crate::RT_PRIORITY_MAX,
                    },
                }),
                Some(priority) if !rt.has_valid_realtime_priority() => {
                    errors.push(ValidationError {
                        path: rt_path.join("priority").into(),
                        kind: ValidationErrorKind::RtPriorityOutOfRange {
                            found: priority,
                            min: crate::RT_PRIORITY_MIN,
                            max: crate::RT_PRIORITY_MAX,
                        },
                    });
                }
                Some(_) => {}
            }
        } else if rt.priority.is_some() {
            errors.push(ValidationError {
                path: rt_path.join("priority").into(),
                kind: ValidationErrorKind::RtPriorityWithNormalPolicy,
            });
        }
    }
}

/// Check 12: an operator entry declares at most one of `dylib`/`wasm`/`hub`.
///
/// Zero is the common case (the hosting binary's own compiled-in
/// `register_operator!` registry); `operator` itself is required by the
/// `Deserialize` shape, so there is no "declares nothing" case to report
/// here.
fn check_operator_locators(manifest: &Manifest, errors: &mut Vec<ValidationError>) {
    for (i, node) in manifest.nodes.iter().enumerate() {
        let node_path = PathBuf::new("nodes").index(i);
        for (j, operator) in node.operators.iter().flatten().enumerate() {
            let locators = operator.locator_kinds();
            if locators.len() > 1 {
                errors.push(ValidationError {
                    path: node_path.join("operators").index(j).into(),
                    kind: ValidationErrorKind::MultipleOperatorLocators { found: locators },
                });
            }
        }
    }
}

/// A lookup from declared node id to its declared output names, shared by
/// [`check_inputs`] and [`check_record`] so both use exactly one reference
/// resolver.
struct NodeIndex<'a> {
    outputs: BTreeMap<&'a str, &'a [String]>,
    /// This manifest's own module boundary inputs (`manifest.module.inputs`),
    /// if it carries a [`crate::ModuleHeader`] at all — `None` for an
    /// ordinary (non-module) manifest, where `_mod/<port>` is simply an
    /// unresolvable reference to a nonexistent node (see
    /// [`NodeIndex::resolve`]).
    module_inputs: Option<&'a [String]>,
}

impl<'a> NodeIndex<'a> {
    fn build(manifest: &'a Manifest) -> Self {
        let mut outputs = BTreeMap::new();
        for node in &manifest.nodes {
            outputs.insert(node.id.as_str(), node.outputs.as_slice());
        }
        let module_inputs = manifest
            .module
            .as_ref()
            .map(|header| header.inputs.as_slice());
        Self {
            outputs,
            module_inputs,
        }
    }

    /// Resolve one reference string — an input `source` or a `record[]`
    /// entry — against declared nodes/outputs, a recognized virtual
    /// source, or (blueprint §8.5) this manifest's own module boundary.
    fn resolve(&self, value: &str) -> Result<(), UnresolvedReferenceReason> {
        if let Some(result) = crate::virtual_source::recognize(value) {
            return result.map(|_| ()).map_err(UnresolvedReferenceReason::from);
        }

        let Some((node_id, output_name)) = value.split_once('/') else {
            return Err(UnresolvedReferenceReason::Malformed);
        };
        if node_id.is_empty() || output_name.is_empty() {
            return Err(UnresolvedReferenceReason::Malformed);
        }

        if node_id == MODULE_BOUNDARY_NODE_ID {
            return match self.module_inputs {
                Some(declared) if declared.iter().any(|p| p == output_name) => Ok(()),
                Some(declared) => Err(UnresolvedReferenceReason::UnknownModuleInput {
                    port: output_name.to_string(),
                    declared: declared.to_vec(),
                }),
                // Not a module at all: `_mod` is simply not a declared
                // node id here, same as any other unknown reference —
                // this preserves the pre-existing message for manifests
                // that never carry a `module:` header.
                None => Err(UnresolvedReferenceReason::UnknownNode(node_id.to_string())),
            };
        }

        let Some(declared_outputs) = self.outputs.get(node_id) else {
            return Err(UnresolvedReferenceReason::UnknownNode(node_id.to_string()));
        };
        if !declared_outputs.iter().any(|o| o == output_name) {
            return Err(UnresolvedReferenceReason::UnknownOutput {
                node: node_id.to_string(),
                output: output_name.to_string(),
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn id_charset_accepts_dots_dashes_underscores() {
        assert!(is_valid_id_charset("camera-1"));
        assert!(is_valid_id_charset("camera_1"));
        assert!(is_valid_id_charset("camera.1"));
        assert!(is_valid_id_charset("Camera123"));
    }

    #[test]
    fn id_charset_rejects_empty_and_special_chars() {
        assert!(!is_valid_id_charset(""));
        assert!(!is_valid_id_charset("camera/1"));
        assert!(!is_valid_id_charset("camera 1"));
        assert!(!is_valid_id_charset("camera:1"));
    }

    #[test]
    fn node_index_resolves_declared_output() {
        let mut camera = Node::with_path("camera", "./camera");
        camera.outputs = vec!["frames".to_string()];
        let manifest = Manifest {
            astrs: "1".to_string(),
            name: None,
            nodes: vec![camera],
            health_check_interval: 5.0,
            exit_when_nodes_finish: false,
            strict_types: false,
            type_rules: Vec::new(),
            env: BTreeMap::new(),
            deploy: None,
            debug: false,
            module: None,
        };
        let index = NodeIndex::build(&manifest);
        assert!(index.resolve("camera/frames").is_ok());
        assert!(matches!(
            index.resolve("camera/missing"),
            Err(UnresolvedReferenceReason::UnknownOutput { .. })
        ));
        assert!(matches!(
            index.resolve("nope/frames"),
            Err(UnresolvedReferenceReason::UnknownNode(_))
        ));
        assert!(index.resolve("astrs/timer/hz/50").is_ok());
        assert!(matches!(
            index.resolve("malformed"),
            Err(UnresolvedReferenceReason::Malformed)
        ));
    }

    #[test]
    fn mod_reference_is_unknown_node_when_manifest_is_not_a_module() {
        let manifest = manifest_with_node(Node::with_path("solo", "./solo"));
        let index = NodeIndex::build(&manifest);
        assert!(matches!(
            index.resolve("_mod/frames"),
            Err(UnresolvedReferenceReason::UnknownNode(ref id)) if id == "_mod"
        ));
    }

    #[test]
    fn mod_reference_resolves_against_declared_module_inputs() {
        let mut manifest = manifest_with_node(Node::with_path("detector", "./detector"));
        manifest.module = Some(crate::ModuleHeader {
            name: "perception".to_string(),
            inputs: vec!["frames".to_string()],
            outputs: vec!["detections".to_string()],
        });
        let index = NodeIndex::build(&manifest);
        assert!(index.resolve("_mod/frames").is_ok());
    }

    #[test]
    fn mod_reference_to_undeclared_input_is_unknown_module_input() {
        let mut manifest = manifest_with_node(Node::with_path("detector", "./detector"));
        manifest.module = Some(crate::ModuleHeader {
            name: "perception".to_string(),
            inputs: vec!["frames".to_string()],
            outputs: Vec::new(),
        });
        let index = NodeIndex::build(&manifest);
        assert!(matches!(
            index.resolve("_mod/framez"),
            Err(UnresolvedReferenceReason::UnknownModuleInput { ref port, ref declared })
                if port == "framez" && declared == &vec!["frames".to_string()]
        ));
    }

    fn manifest_with_node(node: Node) -> Manifest {
        Manifest {
            astrs: "1".to_string(),
            name: None,
            nodes: vec![node],
            health_check_interval: 5.0,
            exit_when_nodes_finish: false,
            strict_types: false,
            type_rules: Vec::new(),
            env: BTreeMap::new(),
            deploy: None,
            debug: false,
            module: None,
        }
    }

    #[test]
    fn type_annotation_matching_declared_port_is_ok() {
        let mut node = Node::with_path("producer", "./producer");
        node.outputs = vec!["frames".to_string()];
        node.output_types
            .insert("frames".to_string(), crate::Urn::new("std/media/v1/Image"));
        let mut errors = Vec::new();
        check_type_annotations_match_ports(&manifest_with_node(node), &mut errors);
        assert!(errors.is_empty(), "errors were: {errors:#?}");
    }

    #[test]
    fn type_annotation_for_undeclared_output_is_rejected() {
        let mut node = Node::with_path("producer", "./producer");
        node.outputs = vec!["frames".to_string()];
        // Typo: "frmaes" instead of "frames".
        node.output_types
            .insert("frmaes".to_string(), crate::Urn::new("std/media/v1/Image"));
        let mut errors = Vec::new();
        check_type_annotations_match_ports(&manifest_with_node(node), &mut errors);
        assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
        assert_eq!(errors[0].path, "nodes[0].output_types.frmaes");
        assert!(matches!(
            &errors[0].kind,
            ValidationErrorKind::TypeForUndeclaredPort {
                map: "output_types",
                port_kind: "output",
                name,
            } if name == "frmaes"
        ));
    }

    #[test]
    fn type_annotation_for_undeclared_input_is_rejected() {
        let mut node = Node::with_path("consumer", "./consumer");
        node.input_types
            .insert("framez".to_string(), crate::Urn::new("std/media/v1/Image"));
        let mut errors = Vec::new();
        check_type_annotations_match_ports(&manifest_with_node(node), &mut errors);
        assert_eq!(errors.len(), 1, "errors were: {errors:#?}");
        assert_eq!(errors[0].path, "nodes[0].input_types.framez");
        assert!(matches!(
            &errors[0].kind,
            ValidationErrorKind::TypeForUndeclaredPort {
                map: "input_types",
                port_kind: "input",
                name,
            } if name == "framez"
        ));
    }
}
