//! The semantic walk over a parsed launch-file tree ([`super::xml`]'s
//! model): namespace composition through `<group>`/`<push_ros_namespace>`,
//! `<node>` discovery (remaps, params, param files, args), composable node
//! containers, and `<include>` recursion (relative-path resolution, depth
//! limiting, cycle detection) -- blueprint §8.6, §10.5.
//!
//! This module is deliberately a **skim, not a launch interpreter**: it
//! never evaluates `if=`/`unless=` conditions or `$(...)` substitutions
//! (blueprint's own scope line for this importer). Every construct with no
//! automatic AstRS equivalent becomes a [`RawNote`] here -- unscoped to any
//! node, since [`super::convert`] is what has enough context to decide
//! whether a note belongs next to a specific manifest node -- rather than
//! being silently skipped (blueprint §8.6).
//!
//! # Namespace resolution
//!
//! ROS 2 node/topic namespacing follows one rule, applied uniformly to
//! `<push_ros_namespace namespace="...">`, a `<node namespace="...">`
//! attribute, and a `<remap to="...">` target: a leading `/` makes it
//! **absolute** (replacing whatever namespace was ambient); a leading `~`
//! makes it **private** (relative to `<namespace>/<node name>`); anything
//! else is **relative** (appended to the ambient namespace). This is real
//! ROS 2 naming grammar (unchanged since ROS 1), not an invented
//! approximation -- see [`resolve_namespace`] and [`resolve_ros_name`].
//!
//! `<push_ros_namespace>` is a launch **action**, not a `<group>` attribute:
//! it only affects the actions that follow it *in document order, within
//! the same scope* (blueprint's own launch semantics, confirmed against
//! `launch_ros`'s `PushRosNamespace`/`GroupAction` -- a group is
//! `scoped=True` by default, so pushes inside it never leak to siblings of
//! the group itself). [`walk_children`] models this exactly: it threads a
//! namespace through a children list sequentially and restores the
//! caller's namespace once that list is exhausted, so a push inside a
//! `<group>` affects only that group's own remaining contents.

use std::path::{Path, PathBuf};

use super::xml::XmlElement;
use crate::error::Ros2MigrateError;

/// The maximum `<include>` nesting depth before [`Ros2MigrateError::IncludeDepthExceeded`].
///
/// Matches `astrs-manifest::expand::ExpandOptions::default().max_depth` --
/// the same failure mode (a long, acyclic chain of distinct included
/// files) in a structurally identical recursive-inclusion problem.
/// Re-exported at [`crate::ros2::MAX_INCLUDE_DEPTH`].
pub const MAX_INCLUDE_DEPTH: usize = 32;

/// One `<remap from="..." to="..."/>` found under a `<node>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Remap {
    /// The `from=` attribute, verbatim.
    pub from: String,
    /// The `to=` attribute, verbatim.
    pub to: String,
}

/// One `<param name="..." value="..."/>` found under a `<node>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParamEntry {
    /// The `name=` attribute, verbatim.
    pub name: String,
    /// The `value=` attribute, verbatim.
    pub value: String,
}

/// One `<node>` (or `<node namespace="...">`) discovered by the walk, with
/// enough raw information for [`super::convert`] to build both an
/// [`astrs_manifest::Node`] and every note it needs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct DiscoveredNode {
    /// The `name=` attribute, verbatim -- including any unresolved `$(...)`
    /// substitution text, which [`super::convert`] carries through into
    /// the manifest id **unsanitized** rather than guessing a resolved
    /// value (blueprint §8.6: "never guess silently"). `None` when `name=`
    /// was omitted (ROS then assigns the node's name from its own code at
    /// run time, which this importer cannot know).
    pub raw_name: Option<String>,
    /// The `pkg=` attribute.
    pub package: Option<String>,
    /// The `exec=` attribute.
    pub executable: Option<String>,
    /// This node's fully resolved ROS namespace: `""` for the root
    /// namespace, otherwise a leading-`/`, no-trailing-`/` string such as
    /// `"/robot1"` -- the ambient namespace from enclosing
    /// `<push_ros_namespace>` actions, further resolved against this
    /// node's own `namespace=` attribute if it has one.
    pub namespace: String,
    /// Every `<remap>` found, in document order.
    pub remaps: Vec<Remap>,
    /// Every `<param name=".." value="..">` found, in document order.
    pub params: Vec<ParamEntry>,
    /// Every `<env name=".." value="..">` found, in document order --
    /// unlike `params`, this is already an OS environment variable in ROS
    /// 2's own model, so [`super::convert`] maps it to `env:` with more
    /// confidence than a `<param>` (which is a ROS parameter-server value
    /// this importer is only *approximating* as an env var).
    pub envs: Vec<ParamEntry>,
    /// Every `<param from="..">` (a parameter **file**, not a scalar)
    /// found, verbatim -- has no single-`env:`-entry equivalent.
    pub param_files: Vec<String>,
    /// The `args=` attribute, to be shell-tokenized by [`super::convert`]
    /// exactly like [`crate::dora::convert`] tokenizes dora's `args:`.
    pub args: Option<String>,
    /// Every `if=`/`unless=` condition attribute on this `<node>` itself,
    /// as `(attribute name, raw value)` pairs.
    pub conditions: Vec<(String, String)>,
    /// The `respawn=` attribute, verbatim.
    pub respawn: Option<String>,
    /// The `respawn_delay=` (or `respawn-delay=`) attribute, verbatim.
    pub respawn_delay: Option<String>,
    /// The `output=` attribute, verbatim.
    pub output: Option<String>,
    /// Every attribute on this `<node>` outside [`KNOWN_NODE_ATTRS`], in
    /// document order, verbatim -- e.g. `launch-prefix="gdb --args"`.
    /// [`super::convert`] turns each into a node-scoped
    /// [`super::convert::MigrationNote`] (blueprint §8.6: "never silently
    /// drop"), mirroring [`crate::dora::model::DoraNode::extra`]'s identical
    /// role for an unrecognized YAML field -- this is the `<node>`-attribute
    /// analogue of [`note_unknown_tag`]'s coverage of an unrecognized
    /// *child tag*.
    pub extra_attrs: Vec<(String, String)>,
    /// Whether this element was a `<lifecycle_node>` rather than a plain
    /// `<node>` -- a distinct, real launch action
    /// (`launch_ros.actions.LifecycleNode`, `@expose_action
    /// ('lifecycle_node')`, attribute-compatible with `Node` but carrying an
    /// additional managed state machine -- Unconfigured/Inactive/Active/
    /// Finalized -- that this importer's bridge scaffold has no equivalent
    /// for). [`super::convert`] uses this to add a note rather than
    /// scaffolding the state machine silently.
    pub is_lifecycle: bool,
}

/// A root-scoped observation with no corresponding manifest node -- an
/// `<include>` that was not followed, a composable node container, an
/// unrecognized launch tag. Always renders as a header-level `TODO(astrs
/// migrate)` comment (blueprint §8.6), never lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawNote {
    /// The note text, already fully formatted -- `walk` has the richest
    /// context (the actual tag and attributes involved) for these
    /// specific, not-node-scoped observations, so it renders the final
    /// message itself rather than handing structured data upstream.
    pub message: String,
}

/// Everything [`walk`] discovered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct WalkResult {
    /// Every `<node>` found, in document order.
    pub nodes: Vec<DiscoveredNode>,
    /// Every root-scoped observation, in the order it was found.
    pub notes: Vec<RawNote>,
}

impl WalkResult {
    fn note(&mut self, message: impl Into<String>) {
        self.notes.push(RawNote {
            message: message.into(),
        });
    }
}

/// Mutable state threaded through the recursive walk: the ambient
/// namespace (mutated and restored by [`walk_children`] per scope) and the
/// `<include>` resolution context (`None` when there is no base directory
/// to resolve relative include paths against -- see
/// [`super::migrate_ros2_launch_str`]'s own docs for why that API never
/// follows includes).
struct WalkState {
    namespace: String,
    include_depth: usize,
    include_stack: Vec<PathBuf>,
    base_dir: Option<PathBuf>,
}

/// Walk `root`'s children, discovering every `<node>` and recording a
/// [`RawNote`] for everything else this importer does not map.
///
/// `base_dir` resolves relative `<include file="...">` paths; `None` means
/// there is no file-backed context to resolve against, so every
/// `<include>` becomes a note explaining why it was not followed rather
/// than being read relative to an arbitrary (and non-reproducible) working
/// directory.
///
/// # Errors
///
/// Returns [`Ros2MigrateError::IncludeCycle`] or
/// [`Ros2MigrateError::IncludeDepthExceeded`] if the include graph reached
/// through *followed* includes is structurally broken -- an include that
/// cannot be resolved/read/parsed at all degrades to a note instead (see
/// this module's top-level docs).
pub(crate) fn walk(
    root: &XmlElement,
    base_dir: Option<&Path>,
) -> Result<WalkResult, Ros2MigrateError> {
    let mut state = WalkState {
        namespace: String::new(),
        include_depth: 0,
        include_stack: Vec::new(),
        base_dir: base_dir.map(Path::to_path_buf),
    };
    let mut out = WalkResult::default();
    walk_children(&root.children, &mut state, &mut out)?;
    Ok(out)
}

fn walk_children(
    children: &[XmlElement],
    state: &mut WalkState,
    out: &mut WalkResult,
) -> Result<(), Ros2MigrateError> {
    let saved_namespace = state.namespace.clone();
    for child in children {
        match child.tag.as_str() {
            "node" => handle_node(child, state, out, false),
            // A real, distinct launch_ros action --
            // `launch_ros.actions.LifecycleNode`, `@expose_action
            // ('lifecycle_node')` -- attribute-compatible with `<node>` but
            // carrying an additional managed state machine
            // (Unconfigured/Inactive/Active/Finalized) this importer's
            // bridge scaffold has no equivalent for; still discovered and
            // mapped like an ordinary node (see [`super::convert`]'s use of
            // [`DiscoveredNode::is_lifecycle`] for the resulting note),
            // rather than falling through to [`note_unknown_tag`] and being
            // lost entirely.
            "lifecycle_node" => handle_node(child, state, out, true),
            "node_container" | "composable_node_container" => handle_container(child, state, out),
            "group" => handle_group(child, state, out)?,
            "include" => handle_include(child, state, out)?,
            "push_ros_namespace" | "push-ros-namespace" => handle_push_namespace(child, state, out),
            _ => {
                // A tag this importer does not recognize by name (e.g. a
                // real `<timer period="...">`, `launch.actions.TimerAction`,
                // `@expose_action('timer')`, or a hypothetical future
                // action) still gets its *own* semantics -- the wrapper's
                // attributes, its delay, its condition -- reported as a
                // note (blueprint §8.6). But its children are ordinary
                // launch actions too (a real `<timer>` nests `<node>`s
                // exactly like `<group>` does), so they are walked
                // recursively rather than vanishing along with the
                // unrecognized wrapper: a `<node>` sitting inside an
                // unrecognized tag is still discovered and still becomes a
                // real manifest node with its own bridge-scaffold note,
                // instead of silently disappearing because its immediate
                // parent tag happened to be one this importer does not
                // special-case by name. Ambient namespace is preserved
                // across this descent exactly as it is for `<group>`
                // (`walk_children`'s own save/restore, invoked here
                // recursively) -- an unrecognized wrapper is never assumed
                // to be a namespace scope of its own, since only `<group>`
                // is documented to be one.
                note_unknown_tag(child, out);
                walk_children(&child.children, state, out)?;
            }
        }
    }
    state.namespace = saved_namespace;
    Ok(())
}

fn handle_push_namespace(el: &XmlElement, state: &mut WalkState, out: &mut WalkResult) {
    match el.attr("namespace") {
        Some(raw) => state.namespace = resolve_namespace(&state.namespace, raw),
        None => out.note(
            "`<push_ros_namespace>` (or `<push-ros-namespace>`) with no `namespace=` attribute; \
             ignored"
                .to_string(),
        ),
    }
}

fn handle_group(
    el: &XmlElement,
    state: &mut WalkState,
    out: &mut WalkResult,
) -> Result<(), Ros2MigrateError> {
    note_conditions(el, "group", out);
    walk_children(&el.children, state, out)
}

/// Every `<node>` attribute this importer already interprets by name --
/// anything else on the element becomes an [`DiscoveredNode::extra_attrs`]
/// entry instead of vanishing (blueprint §8.6). `if`/`unless` are covered
/// separately by [`XmlElement::conditions`], not listed here, since that
/// method (not this list) is what [`handle_node`] consults for them.
const KNOWN_NODE_ATTRS: [&str; 9] = [
    "name",
    "pkg",
    "exec",
    "namespace",
    "args",
    "if",
    "unless",
    "respawn",
    "output",
];

/// The `respawn_delay=`/`respawn-delay=` and `output=` handling above
/// already special-cases the underscore/hyphen pair for `respawn_delay`;
/// [`KNOWN_NODE_ATTRS`] omits it because both spellings are checked here,
/// against the *raw* attribute list, not through that constant.
fn is_known_node_attr(key: &str) -> bool {
    KNOWN_NODE_ATTRS.contains(&key) || key == "respawn_delay" || key == "respawn-delay"
}

/// Discover one `<node>` or `<lifecycle_node>` element -- `is_lifecycle`
/// distinguishes which (see [`DiscoveredNode::is_lifecycle`]'s docs for why
/// that distinction matters even though both share the same attribute
/// grammar).
fn handle_node(el: &XmlElement, state: &WalkState, out: &mut WalkResult, is_lifecycle: bool) {
    let namespace = match el.attr("namespace") {
        Some(raw) => resolve_namespace(&state.namespace, raw),
        None => state.namespace.clone(),
    };

    let mut node = DiscoveredNode {
        raw_name: el.attr("name").map(str::to_string),
        package: el.attr("pkg").map(str::to_string),
        executable: el.attr("exec").map(str::to_string),
        namespace,
        args: el.attr("args").map(str::to_string),
        conditions: el
            .conditions()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        respawn: el.attr("respawn").map(str::to_string),
        respawn_delay: el
            .attr("respawn_delay")
            .or_else(|| el.attr("respawn-delay"))
            .map(str::to_string),
        output: el.attr("output").map(str::to_string),
        extra_attrs: el
            .attrs
            .iter()
            .filter(|(k, _)| !is_known_node_attr(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        is_lifecycle,
        ..DiscoveredNode::default()
    };

    for child in &el.children {
        match child.tag.as_str() {
            "remap" => match (child.attr("from"), child.attr("to")) {
                (Some(from), Some(to)) => node.remaps.push(Remap {
                    from: from.to_string(),
                    to: to.to_string(),
                }),
                _ => out.note(format!(
                    "<remap> under node `{}` is missing `from=`/`to=`; skipped",
                    node_label(&node)
                )),
            },
            "param" => match (child.attr("name"), child.attr("value"), child.attr("from")) {
                (Some(name), Some(value), _) => node.params.push(ParamEntry {
                    name: name.to_string(),
                    value: value.to_string(),
                }),
                (_, _, Some(from)) => node.param_files.push(from.to_string()),
                _ => out.note(format!(
                    "<param> under node `{}` has neither `name=`/`value=` nor `from=` (or is a \
                     nested structured param this importer does not flatten); skipped",
                    node_label(&node)
                )),
            },
            "env" => match (child.attr("name"), child.attr("value")) {
                (Some(name), Some(value)) => node.envs.push(ParamEntry {
                    name: name.to_string(),
                    value: value.to_string(),
                }),
                _ => out.note(format!(
                    "<env> under node `{}` is missing `name=`/`value=`; skipped",
                    node_label(&node)
                )),
            },
            other => out.note(format!(
                "unrecognized child `<{other}>` under node `{}`; not interpreted",
                node_label(&node)
            )),
        }
    }

    out.nodes.push(node);
}

/// A best-effort human label for a node still being built -- its `name=`
/// if given, else its `exec=`, else a generic placeholder. Used only in
/// note text; [`super::convert`] owns the actual id-assignment policy.
fn node_label(node: &DiscoveredNode) -> &str {
    node.raw_name
        .as_deref()
        .or(node.executable.as_deref())
        .unwrap_or("<unnamed>")
}

fn handle_container(el: &XmlElement, state: &WalkState, out: &mut WalkResult) {
    let namespace = match el.attr("namespace") {
        Some(raw) => resolve_namespace(&state.namespace, raw),
        None => state.namespace.clone(),
    };
    let label = el
        .attr("name")
        .or_else(|| el.attr("exec"))
        .unwrap_or("<unnamed>");
    let composable: Vec<String> = el
        .children
        .iter()
        .filter(|c| c.tag == "composable_node")
        .map(|c| {
            let name = c.attr("name").unwrap_or("<unnamed>");
            let plugin = c.attr("plugin").unwrap_or("<unknown plugin>");
            format!("`{name}` (plugin=`{plugin}`)")
        })
        .collect();

    let mut message = format!(
        "composable node container `{label}` (pkg=`{}`, exec=`{}`, namespace=`{}`) with {} \
         composable node(s){}; composable-node bridging is not scaffolded by this importer -- \
         port each one by hand as its own `ros2:` bridge node once its runtime topic behavior \
         is known",
        el.attr("pkg").unwrap_or("?"),
        el.attr("exec").unwrap_or("?"),
        if namespace.is_empty() {
            "/"
        } else {
            &namespace
        },
        composable.len(),
        if composable.is_empty() {
            String::new()
        } else {
            format!(": {}", composable.join(", "))
        },
    );
    let conditions = el.conditions();
    if !conditions.is_empty() {
        message.push_str(&format!(
            "; conditional in the source launch file ({})",
            format_conditions(&conditions)
        ));
    }
    out.note(message);
}

fn handle_include(
    el: &XmlElement,
    state: &mut WalkState,
    out: &mut WalkResult,
) -> Result<(), Ros2MigrateError> {
    note_conditions(el, "include", out);

    let Some(raw_file) = el.attr("file") else {
        out.note("<include> with no `file=` attribute; skipped".to_string());
        return Ok(());
    };

    if raw_file.contains("$(") {
        out.note(format!(
            "<include file=\"{raw_file}\"> uses a launch substitution and was not followed; \
             port its contents by hand, or resolve the substitution yourself and re-run `astrs \
             migrate from-ros2` on the resolved file directly"
        ));
        return Ok(());
    }

    let Some(base_dir) = state.base_dir.clone() else {
        out.note(format!(
            "<include file=\"{raw_file}\"> was not followed: no base directory is available to \
             resolve relative include paths against (this happens when migrating from a bare \
             string, not a file) -- run `astrs migrate from-ros2` on the file directly to have \
             includes followed"
        ));
        return Ok(());
    };
    let resolved = base_dir.join(raw_file);

    if state.include_depth >= MAX_INCLUDE_DEPTH {
        return Err(Ros2MigrateError::IncludeDepthExceeded {
            max_depth: MAX_INCLUDE_DEPTH,
        });
    }

    let canonical = resolved.canonicalize().unwrap_or_else(|_| resolved.clone());
    if state.include_stack.contains(&canonical) {
        let mut chain: Vec<String> = state
            .include_stack
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        chain.push(canonical.display().to_string());
        return Err(Ros2MigrateError::IncludeCycle { chain });
    }

    let Ok(content) = std::fs::read_to_string(&resolved) else {
        out.note(format!(
            "<include file=\"{raw_file}\"> could not be read (resolved against this launch \
             file's own directory); not followed -- verify the path"
        ));
        return Ok(());
    };

    if resolved.extension().and_then(|e| e.to_str()) == Some("py") {
        out.note(format!(
            "<include file=\"{raw_file}\"> targets a Python launch file; this importer does \
             not parse Python even when reached via <include> -- port it by hand, or run `astrs \
             migrate from-ros2` on it directly for a best-effort regex skim"
        ));
        return Ok(());
    }

    let child_root = match super::xml::parse(&content) {
        Ok(tree) => tree,
        Err(message) => {
            out.note(format!(
                "<include file=\"{raw_file}\"> could not be parsed as XML ({message}); not \
                 followed"
            ));
            return Ok(());
        }
    };

    let saved_base_dir = state.base_dir.clone();
    state.base_dir = resolved.parent().map(Path::to_path_buf).or(Some(base_dir));
    state.include_depth += 1;
    state.include_stack.push(canonical);

    let walked = walk_children(&child_root.children, state, out);

    state.include_stack.pop();
    state.include_depth -= 1;
    state.base_dir = saved_base_dir;

    walked
}

fn note_conditions(el: &XmlElement, kind: &str, out: &mut WalkResult) {
    let conditions = el.conditions();
    if conditions.is_empty() {
        return;
    }
    out.note(format!(
        "<{kind}> is conditional in the source launch file ({}); this importer does not \
         evaluate conditions, so its contents are scaffolded unconditionally -- add your own \
         conditional wiring by hand if needed",
        format_conditions(&conditions)
    ));
}

fn format_conditions(conditions: &[(&str, &str)]) -> String {
    conditions
        .iter()
        .map(|(k, v)| format!("{k}=\"{v}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

fn note_unknown_tag(el: &XmlElement, out: &mut WalkResult) {
    out.note(format!(
        "unrecognized launch tag `<{}>`; not interpreted by this importer, carried through as \
         this note only",
        el.tag
    ));
}

/// Resolve a ROS 2 namespace: absolute (`raw` starts with `/`) replaces
/// `ambient` entirely; relative appends to it; an empty `raw` leaves
/// `ambient` unchanged. See this module's top-level docs.
pub(crate) fn resolve_namespace(ambient: &str, raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return ambient.to_string();
    }
    if let Some(stripped) = raw.strip_prefix('/') {
        let trimmed = stripped.trim_end_matches('/');
        return if trimmed.is_empty() {
            String::new()
        } else {
            format!("/{trimmed}")
        };
    }
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() {
        return ambient.to_string();
    }
    if ambient.is_empty() {
        format!("/{trimmed}")
    } else {
        format!("{ambient}/{trimmed}")
    }
}

/// Resolve a ROS 2 graph name (a remap's `to=`, most often) against a
/// node's fully-resolved `namespace` and its own `local_name`: absolute
/// (`/...`) is unchanged; private (`~` or `~/...`) expands to
/// `<namespace>/<local_name>[/...]`; anything else is relative and is
/// appended to `namespace`. See this module's top-level docs.
pub(crate) fn resolve_ros_name(raw: &str, namespace: &str, local_name: &str) -> String {
    let raw = raw.trim();
    if raw.starts_with('/') {
        return raw.to_string();
    }
    if raw == "~" {
        return format!("{namespace}/{local_name}");
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return format!("{namespace}/{local_name}/{rest}");
    }
    format!("{namespace}/{raw}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::ros2::xml::parse as parse_xml;

    fn walk_str(xml: &str) -> WalkResult {
        let tree = parse_xml(xml).unwrap();
        walk(&tree, None).unwrap()
    }

    #[test]
    fn resolve_namespace_absolute_replaces_ambient() {
        assert_eq!(resolve_namespace("/old", "/new"), "/new");
        assert_eq!(resolve_namespace("", "/new"), "/new");
    }

    #[test]
    fn resolve_namespace_relative_appends() {
        assert_eq!(resolve_namespace("", "robot1"), "/robot1");
        assert_eq!(resolve_namespace("/robot1", "arm"), "/robot1/arm");
    }

    #[test]
    fn resolve_namespace_empty_raw_is_a_no_op() {
        assert_eq!(resolve_namespace("/robot1", ""), "/robot1");
    }

    #[test]
    fn resolve_ros_name_absolute_is_unchanged() {
        assert_eq!(resolve_ros_name("/scan", "/robot1", "lidar"), "/scan");
    }

    #[test]
    fn resolve_ros_name_relative_appends_namespace() {
        assert_eq!(resolve_ros_name("scan", "", "lidar"), "/scan");
        assert_eq!(resolve_ros_name("scan", "/robot1", "lidar"), "/robot1/scan");
    }

    #[test]
    fn resolve_ros_name_private_expands_with_node_name() {
        assert_eq!(resolve_ros_name("~", "", "lidar"), "/lidar");
        assert_eq!(
            resolve_ros_name("~/status", "/robot1", "lidar"),
            "/robot1/lidar/status"
        );
    }

    #[test]
    fn a_single_node_is_discovered_with_root_namespace() {
        let result = walk_str(r#"<launch><node pkg="p" exec="e" name="n"/></launch>"#);
        assert_eq!(result.nodes.len(), 1);
        assert_eq!(result.nodes[0].raw_name.as_deref(), Some("n"));
        assert_eq!(result.nodes[0].namespace, "");
    }

    #[test]
    fn group_and_push_ros_namespace_compose_for_nodes_after_it() {
        let result = walk_str(
            r#"<launch>
                 <group>
                   <push_ros_namespace namespace="robot1"/>
                   <node pkg="p" exec="e" name="a"/>
                 </group>
                 <node pkg="p" exec="e" name="b"/>
               </launch>"#,
        );
        assert_eq!(result.nodes.len(), 2);
        assert_eq!(result.nodes[0].namespace, "/robot1");
        assert_eq!(
            result.nodes[1].namespace, "",
            "push_ros_namespace must not leak out of its own group"
        );
    }

    #[test]
    fn push_ros_namespace_hyphen_spelling_is_also_recognized() {
        let result = walk_str(
            r#"<launch>
                 <group>
                   <push-ros-namespace namespace="robot2"/>
                   <node pkg="p" exec="e" name="a"/>
                 </group>
               </launch>"#,
        );
        assert_eq!(result.nodes[0].namespace, "/robot2");
    }

    #[test]
    fn node_namespace_attribute_composes_with_ambient() {
        let result = walk_str(
            r#"<launch>
                 <group>
                   <push_ros_namespace namespace="robot1"/>
                   <node pkg="p" exec="e" name="a" namespace="sensors"/>
                 </group>
               </launch>"#,
        );
        assert_eq!(result.nodes[0].namespace, "/robot1/sensors");
    }

    #[test]
    fn remaps_and_params_and_envs_and_param_files_are_collected() {
        let result = walk_str(
            r#"<launch>
                 <node pkg="p" exec="e" name="a">
                   <remap from="in" to="/out"/>
                   <param name="rate" value="10"/>
                   <param from="cfg.yaml"/>
                   <env name="LOG" value="debug"/>
                 </node>
               </launch>"#,
        );
        let node = &result.nodes[0];
        assert_eq!(
            node.remaps,
            vec![Remap {
                from: "in".into(),
                to: "/out".into()
            }]
        );
        assert_eq!(
            node.params,
            vec![ParamEntry {
                name: "rate".into(),
                value: "10".into()
            }]
        );
        assert_eq!(node.param_files, vec!["cfg.yaml".to_string()]);
        assert_eq!(
            node.envs,
            vec![ParamEntry {
                name: "LOG".into(),
                value: "debug".into()
            }]
        );
    }

    #[test]
    fn unrecognized_node_attribute_is_captured_as_an_extra() {
        let result = walk_str(
            r#"<launch><node pkg="p" exec="e" name="n" launch-prefix="gdb --args"/></launch>"#,
        );
        assert_eq!(
            result.nodes[0].extra_attrs,
            vec![("launch-prefix".to_string(), "gdb --args".to_string())]
        );
    }

    #[test]
    fn known_node_attributes_never_land_in_extras() {
        let result = walk_str(
            r#"<launch><node pkg="p" exec="e" name="n" namespace="ns" args="--x" respawn="true"
                              respawn_delay="1.0" output="screen" if="$(var c)"/></launch>"#,
        );
        assert!(result.nodes[0].extra_attrs.is_empty());
    }

    #[test]
    fn respawn_delay_hyphen_spelling_is_also_a_known_attribute() {
        let result =
            walk_str(r#"<launch><node pkg="p" exec="e" name="n" respawn-delay="2.0"/></launch>"#);
        assert!(
            result.nodes[0].extra_attrs.is_empty(),
            "extras: {:?}",
            result.nodes[0].extra_attrs
        );
        assert_eq!(result.nodes[0].respawn_delay.as_deref(), Some("2.0"));
    }

    #[test]
    fn node_conditions_are_captured_on_the_discovered_node() {
        let result = walk_str(r#"<launch><node pkg="p" exec="e" if="$(var use_a)"/></launch>"#);
        assert_eq!(
            result.nodes[0].conditions,
            vec![("if".to_string(), "$(var use_a)".to_string())]
        );
    }

    #[test]
    fn group_condition_becomes_a_root_note() {
        let result = walk_str(
            r#"<launch><group if="$(var use_a)"><node pkg="p" exec="e"/></group></launch>"#,
        );
        assert!(result.notes.iter().any(|n| n.message.contains("<group>")));
        assert_eq!(
            result.nodes.len(),
            1,
            "the group's contents are still scaffolded"
        );
    }

    #[test]
    fn composable_container_becomes_a_note_with_no_node_emitted() {
        let result = walk_str(
            r#"<launch>
                 <node_container pkg="rclcpp_components" exec="component_container" name="c">
                   <composable_node pkg="p" plugin="p::Plugin" name="comp"/>
                 </node_container>
               </launch>"#,
        );
        assert!(result.nodes.is_empty());
        assert_eq!(result.notes.len(), 1);
        assert!(result.notes[0].message.contains("comp"));
        assert!(result.notes[0].message.contains("p::Plugin"));
    }

    #[test]
    fn unknown_tag_becomes_a_note_not_a_silent_drop() {
        let result = walk_str(r#"<launch><let name="x" value="y"/></launch>"#);
        assert_eq!(result.nodes.len(), 0);
        assert!(result.notes.iter().any(|n| n.message.contains("<let>")));
    }

    /// `<timer>` is a real, registered launch_ros/launch action
    /// (`launch.actions.TimerAction`, `@expose_action('timer')`) that nests
    /// ordinary actions -- including `<node>` -- exactly like `<group>`
    /// does. This importer does not recognize `<timer>` by name (its delay
    /// semantics are out of this skim's scope), but a `<node>` nested
    /// inside it must still be discovered rather than vanishing along with
    /// the unrecognized wrapper -- the sharpest form of blueprint §8.6
    /// applied to a *tag this importer has never heard of*, not just a
    /// known-but-unmappable one.
    #[test]
    fn a_node_nested_inside_an_unrecognized_wrapper_tag_is_still_discovered() {
        let result = walk_str(
            r#"<launch>
                 <timer period="5.0">
                   <node pkg="p" exec="e" name="delayed"/>
                 </timer>
               </launch>"#,
        );
        assert_eq!(
            result.nodes.len(),
            1,
            "a <node> inside an unrecognized <timer> wrapper must not be lost: {result:?}"
        );
        assert_eq!(result.nodes[0].raw_name.as_deref(), Some("delayed"));
        assert!(
            result.notes.iter().any(|n| n.message.contains("<timer>")),
            "the wrapper itself must still be noted, exactly as it was before recursion was \
             added: {:?}",
            result.notes
        );
    }

    #[test]
    fn ambient_namespace_still_composes_through_an_unrecognized_wrapper() {
        let result = walk_str(
            r#"<launch>
                 <group>
                   <push_ros_namespace namespace="robot1"/>
                   <timer period="1.0">
                     <node pkg="p" exec="e" name="delayed"/>
                   </timer>
                 </group>
                 <node pkg="p" exec="e" name="sibling"/>
               </launch>"#,
        );
        let delayed = result
            .nodes
            .iter()
            .find(|n| n.raw_name.as_deref() == Some("delayed"))
            .unwrap();
        assert_eq!(delayed.namespace, "/robot1");
        let sibling = result
            .nodes
            .iter()
            .find(|n| n.raw_name.as_deref() == Some("sibling"))
            .unwrap();
        assert_eq!(
            sibling.namespace, "",
            "the push inside the group+timer must not leak to a sibling outside the group"
        );
    }

    /// `<lifecycle_node>` is a real, distinct launch_ros action
    /// (`launch_ros.actions.LifecycleNode`, `@expose_action
    /// ('lifecycle_node')`) -- attribute-compatible with `<node>`, and must
    /// be discovered the same way rather than falling through to
    /// [`note_unknown_tag`] and being lost entirely.
    #[test]
    fn lifecycle_node_is_discovered_like_an_ordinary_node() {
        let result =
            walk_str(r#"<launch><lifecycle_node pkg="p" exec="e" name="managed"/></launch>"#);
        assert_eq!(result.nodes.len(), 1);
        assert_eq!(result.nodes[0].raw_name.as_deref(), Some("managed"));
        assert!(result.nodes[0].is_lifecycle);
    }

    #[test]
    fn plain_node_is_not_flagged_as_lifecycle() {
        let result = walk_str(r#"<launch><node pkg="p" exec="e" name="plain"/></launch>"#);
        assert!(!result.nodes[0].is_lifecycle);
    }

    #[test]
    fn include_with_substitution_is_noted_not_followed() {
        let result =
            walk_str(r#"<launch><include file="$(find-pkg-share pkg)/launch/x.xml"/></launch>"#);
        assert!(
            result
                .notes
                .iter()
                .any(|n| n.message.contains("substitution"))
        );
    }

    #[test]
    fn include_with_no_base_dir_is_noted_not_followed() {
        let result = walk_str(r#"<launch><include file="child.xml"/></launch>"#);
        assert!(
            result
                .notes
                .iter()
                .any(|n| n.message.contains("no base directory"))
        );
    }

    #[test]
    fn include_of_a_missing_file_is_noted_not_a_hard_error() {
        let tree = parse_xml(r#"<launch><include file="does-not-exist.xml"/></launch>"#).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "astrs-migrate-ros2-walk-test-{}-missing-include",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let result = walk(&tree, Some(&dir)).unwrap();
        assert!(result.nodes.is_empty());
        assert!(
            result
                .notes
                .iter()
                .any(|n| n.message.contains("could not be read"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn include_of_a_python_file_is_noted_not_followed() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-migrate-ros2-walk-test-{}-py-include",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("child.py"), "# not parsed").unwrap();
        let tree = parse_xml(r#"<launch><include file="child.py"/></launch>"#).unwrap();
        let result = walk(&tree, Some(&dir)).unwrap();
        assert!(result.nodes.is_empty());
        assert!(
            result
                .notes
                .iter()
                .any(|n| n.message.contains("Python launch file"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_real_relative_include_is_followed_and_inherits_namespace() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-migrate-ros2-walk-test-{}-real-include",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("child.xml"),
            r#"<launch><node pkg="p" exec="e" name="child_node"/></launch>"#,
        )
        .unwrap();
        let tree = parse_xml(
            r#"<launch><group><push_ros_namespace namespace="robot1"/><include file="child.xml"/></group></launch>"#,
        )
        .unwrap();
        let result = walk(&tree, Some(&dir)).unwrap();
        assert_eq!(result.nodes.len(), 1);
        assert_eq!(result.nodes[0].raw_name.as_deref(), Some("child_node"));
        assert_eq!(result.nodes[0].namespace, "/robot1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_two_file_include_cycle_is_a_hard_error() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-migrate-ros2-walk-test-{}-cycle",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.xml"),
            r#"<launch><include file="b.xml"/></launch>"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("b.xml"),
            r#"<launch><include file="a.xml"/></launch>"#,
        )
        .unwrap();
        let tree = parse_xml(r#"<launch><include file="a.xml"/></launch>"#).unwrap();
        let err = walk(&tree, Some(&dir)).unwrap_err();
        assert!(matches!(err, Ros2MigrateError::IncludeCycle { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn include_depth_exceeded_is_a_hard_error() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-migrate-ros2-walk-test-{}-depth",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // A chain of MAX_INCLUDE_DEPTH + 2 distinct files, none repeated:
        // not a cycle, but deep enough to trip the depth limit.
        let chain_len = MAX_INCLUDE_DEPTH + 2;
        for i in 0..chain_len {
            let content = if i + 1 < chain_len {
                format!(r#"<launch><include file="f{}.xml"/></launch>"#, i + 1)
            } else {
                r#"<launch><node pkg="p" exec="e"/></launch>"#.to_string()
            };
            std::fs::write(dir.join(format!("f{i}.xml")), content).unwrap();
        }
        let tree = parse_xml(r#"<launch><include file="f0.xml"/></launch>"#).unwrap();
        let err = walk(&tree, Some(&dir)).unwrap_err();
        assert!(matches!(
            err,
            Ros2MigrateError::IncludeDepthExceeded { max_depth } if max_depth == MAX_INCLUDE_DEPTH
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_diamond_include_is_not_a_cycle() {
        // root includes both a.xml and b.xml, which both include shared.xml
        // -- revisiting shared.xml via two unrelated branches must not be
        // flagged as a cycle (matches astrs-manifest's own module-expand
        // precedent for the identical shape).
        let dir = std::env::temp_dir().join(format!(
            "astrs-migrate-ros2-walk-test-{}-diamond",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("shared.xml"),
            r#"<launch><node pkg="p" exec="e" name="shared"/></launch>"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("a.xml"),
            r#"<launch><include file="shared.xml"/></launch>"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("b.xml"),
            r#"<launch><include file="shared.xml"/></launch>"#,
        )
        .unwrap();
        let tree = parse_xml(r#"<launch><include file="a.xml"/><include file="b.xml"/></launch>"#)
            .unwrap();
        let result = walk(&tree, Some(&dir)).unwrap();
        assert_eq!(result.nodes.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
