//! Mapping a [`WalkResult`] onto an [`astrs_manifest::Manifest`] (blueprint
//! §8.6, §10.5): every discovered `<node>` becomes one manifest node
//! stanza with a `ros2:` bridge block scaffold (§10.5) -- id/namespace
//! mapped, topic name derived from its first remap where one exists,
//! params/`<env>` mapped to `env:`, args shell-tokenized exactly like
//! [`crate::dora::convert`] tokenizes dora's own `args:`. Every root-scoped
//! observation from the walk (an unfollowed `<include>`, a composable node
//! container, an unrecognized tag) becomes a root-level [`MigrationNote`]
//! unchanged.
//!
//! # What is never guessed
//!
//! `message_type` and bridge `direction` cannot be inferred from a launch
//! file skim alone -- knowing them requires the node's own source code,
//! which this importer never reads (blueprint's own scope line: "a launch
//! file skim..., NOT a full launch interpreter"). Both are always left
//! unset, with a [`MigrationNote`] explaining why, rather than defaulting
//! either to a guessed value -- this is also *why* the single-topic
//! `ros2:` form is used exclusively rather than the bulk `topics: [...]`
//! form: [`astrs_manifest::Ros2Topic::direction`] is a required
//! (non-`Option`) field, so representing an unknown direction honestly is
//! only possible in the single-topic form, where `direction` is
//! `Option`.
//!
//! A node whose `name=` contains an unresolved `$(...)` launch substitution
//! is the sharpest example of "never guess silently": that text is carried
//! into the manifest `id` **verbatim**, unsanitized (only a namespace
//! prefix's `/` separators are ever rewritten, to `_`, for id-charset
//! compliance -- see [`sanitize_namespace_for_id`]). The result reliably
//! fails [`astrs_manifest::Manifest::validate`]'s `InvalidIdCharset` check
//! -- which is the point: a validation failure that names the exact
//! unresolved text is more honest than an id this importer invented that
//! would validate cleanly while meaning nothing.

use std::collections::BTreeMap;

use serde::Serialize;

use super::walk::{DiscoveredNode, RawNote, WalkResult, resolve_ros_name};

/// How serious a [`MigrationNote`] is -- identical vocabulary to
/// [`crate::dora::NoteSeverity`] (a separate type: [`super`]'s file
/// ownership keeps this crate-internal, mirroring
/// [`crate::error::Ros2MigrateError`] living apart from
/// [`crate::error::DoraMigrateError`] despite the shared shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteSeverity {
    /// A launch construct that had no effect in the source launch file
    /// either was dropped outright -- informational only. Not currently
    /// produced by this importer's own logic (every ROS 2 launch construct
    /// this importer recognizes is meaningful in ROS 2 itself), but kept
    /// for shape-parity with [`crate::dora::NoteSeverity`] and to leave
    /// room for a future refinement that legitimately needs it.
    Dropped,
    /// A launch construct with no automatic AstRS equivalent needs a human
    /// decision; nothing was silently lost, but nothing was automatically
    /// ported either.
    NeedsAttention,
}

/// One observation made while migrating a ROS 2 launch file.
///
/// See [`crate::dora::MigrationNote`] for the shape this mirrors --
/// `super::render` turns each into a `TODO(astrs migrate)` comment placed
/// next to the node it names (or, for `node: None`, into a header comment
/// at the top of the file).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigrationNote {
    /// The node this note is about, or `None` for a root-level observation
    /// (an unfollowed `<include>`, a composable node container, an
    /// unrecognized tag -- nothing this importer turned into a manifest
    /// node at all).
    pub node: Option<String>,
    /// How serious this observation is.
    pub severity: NoteSeverity,
    /// A human-readable explanation, written to stand on its own inside a
    /// YAML comment.
    pub message: String,
}

impl MigrationNote {
    /// Build a root-level (not scoped to any node) `NeedsAttention` note.
    /// `pub(super)`: also used by [`super::python`], whose candidates
    /// never correspond to a real manifest node.
    pub(super) fn root(message: String) -> Self {
        Self {
            node: None,
            severity: NoteSeverity::NeedsAttention,
            message,
        }
    }

    fn needs_attention(node: &str, message: String) -> Self {
        Self {
            node: Some(node.to_string()),
            severity: NoteSeverity::NeedsAttention,
            message,
        }
    }
}

/// Strip a resolved ROS namespace's leading `/` and rewrite any remaining
/// `/` separators to `_`, e.g. `"/robot1/arm"` -> `"robot1_arm"` -- the
/// only sanitization this importer ever applies while building a manifest
/// `id`. The node's own local name (from `name=`) is **never** touched by
/// this or anything else: see this module's top-level docs for why that
/// matters.
fn sanitize_namespace_for_id(namespace: &str) -> String {
    namespace.trim_start_matches('/').replace('/', "_")
}

/// Build this node's manifest-id candidate from its (unsanitized) local
/// name and its (sanitized) namespace prefix.
fn build_candidate_id(local_name: &str, namespace: &str) -> String {
    if namespace.is_empty() {
        local_name.to_string()
    } else {
        format!("{}_{}", sanitize_namespace_for_id(namespace), local_name)
    }
}

/// Assign `candidate` as a manifest id, disambiguating with a numeric
/// suffix (`_2`, `_3`, ...) against every id already handed out by this
/// call, so this importer's own id-construction logic never itself
/// produces a `DuplicateNodeId` validation failure (a residual collision
/// after namespace-prefixing is realistic -- e.g. two genuinely identical
/// `<node>` entries -- but is always resolved here, with a note, rather
/// than left for `astrs validate` to catch as a surprise). Returns the
/// final id and whether it had to be renamed.
fn assign_unique_id(candidate: String, used: &mut BTreeMap<String, u32>) -> (String, bool) {
    if let std::collections::btree_map::Entry::Vacant(e) = used.entry(candidate.clone()) {
        e.insert(1);
        return (candidate, false);
    }
    let mut n = 2;
    loop {
        let suffixed = format!("{candidate}_{n}");
        if !used.contains_key(&suffixed) {
            used.insert(suffixed.clone(), 1);
            return (suffixed, true);
        }
        n += 1;
    }
}

fn node_env(discovered: &DiscoveredNode) -> BTreeMap<String, astrs_manifest::EnvValue> {
    let mut env = BTreeMap::new();
    // `<param>` first (a ROS parameter, only *approximated* as an env var
    // by this importer), then `<env>` (already an OS environment variable
    // in ROS 2's own model, so it wins on a name collision).
    for param in &discovered.params {
        env.insert(
            param.name.clone(),
            astrs_manifest::EnvValue::String(param.value.clone()),
        );
    }
    for entry in &discovered.envs {
        env.insert(
            entry.name.clone(),
            astrs_manifest::EnvValue::String(entry.value.clone()),
        );
    }
    env
}

fn node_args(discovered: &DiscoveredNode, id: &str, notes: &mut Vec<MigrationNote>) -> Vec<String> {
    match &discovered.args {
        None => Vec::new(),
        Some(raw) => match shlex::split(raw) {
            Some(tokens) => tokens,
            None => {
                notes.push(MigrationNote::needs_attention(
                    id,
                    format!(
                        "args `{raw}` could not be tokenized as a shell command line \
                         (unbalanced quoting?); left empty"
                    ),
                ));
                Vec::new()
            }
        },
    }
}

/// The bridge-scaffold note: always present (message_type is never
/// inferable), naming the derived topic when a remap exists.
fn bridge_note(discovered: &DiscoveredNode, id: &str, local_name: &str) -> MigrationNote {
    let pkg = discovered.package.as_deref().unwrap_or("?");
    let exec = discovered.executable.as_deref().unwrap_or("?");
    let message = match discovered.remaps.first() {
        None => format!(
            "ros2 bridge scaffold for node `{id}` (pkg=`{pkg}`, exec=`{exec}`): no <remap> was \
             found, so the generated `ros2:` block has no `topic` set -- this node may not need \
             a bridge at all (this importer only derives topics from explicit remaps), or set \
             `topic`/`message_type`/`direction` by hand if it does. message_type can never be \
             inferred from a launch file alone; qos is left unset, using astrs-rtps's defaults \
             (§10.2) unless this topic needs specific reliability/durability/history."
        ),
        Some(first) => {
            let resolved = resolve_ros_name(&first.to, &discovered.namespace, local_name);
            format!(
                "ros2 bridge scaffold for node `{id}` (pkg=`{pkg}`, exec=`{exec}`): topic \
                 `{resolved}` derived from <remap from=\"{}\" to=\"{}\"/>; message_type could \
                 not be inferred from the launch file alone, and direction (to_astrs/from_astrs) \
                 is not derivable either -- fill in both by hand, then add an `outputs:` (for \
                 to_astrs) or `inputs:` (for from_astrs) entry once direction is chosen. qos is \
                 left unset, using astrs-rtps's defaults (§10.2) unless this topic needs specific \
                 reliability/durability/history.",
                first.from, first.to
            )
        }
    };
    MigrationNote::needs_attention(id, message)
}

fn additional_remap_notes(
    discovered: &DiscoveredNode,
    id: &str,
    local_name: &str,
) -> Vec<MigrationNote> {
    discovered
        .remaps
        .iter()
        .skip(1)
        .map(|remap| {
            let resolved = resolve_ros_name(&remap.to, &discovered.namespace, local_name);
            MigrationNote::needs_attention(
                id,
                format!(
                    "additional remap on node `{id}`: <remap from=\"{}\" to=\"{}\"/> (resolves \
                     to `{resolved}`) was not represented in the generated `ros2:` block (only \
                     the first remap becomes the scaffolded topic) -- add a `topics:` entry by \
                     hand for this one too, with its own message_type and direction",
                    remap.from, remap.to
                ),
            )
        })
        .collect()
}

fn convert_node(
    discovered: &DiscoveredNode,
    used_ids: &mut BTreeMap<String, u32>,
    notes: &mut Vec<MigrationNote>,
) -> astrs_manifest::Node {
    let (local_name, name_is_fallback) = match &discovered.raw_name {
        Some(name) => (name.clone(), false),
        None => (
            discovered
                .executable
                .clone()
                .unwrap_or_else(|| "node".to_string()),
            true,
        ),
    };

    let candidate_id = build_candidate_id(&local_name, &discovered.namespace);
    let (id, was_renamed) = assign_unique_id(candidate_id.clone(), used_ids);

    if name_is_fallback {
        notes.push(MigrationNote::needs_attention(
            &id,
            format!(
                "no `name=` attribute on this <node> (pkg=`{}`, exec=`{}`); used the \
                 executable name as a placeholder id -- ROS assigns this node's actual name \
                 from its own code at run time, which this importer cannot know, and it could \
                 differ",
                discovered.package.as_deref().unwrap_or("?"),
                discovered.executable.as_deref().unwrap_or("?")
            ),
        ));
    }
    if discovered.package.is_none() && discovered.executable.is_none() {
        notes.push(MigrationNote::needs_attention(
            &id,
            "neither `pkg=` nor `exec=` was found on this <node>; it appears incomplete or \
             malformed in the source launch file"
                .to_string(),
        ));
    }
    if was_renamed {
        notes.push(MigrationNote::needs_attention(
            &id,
            format!(
                "id `{candidate_id}` collided with another discovered node's id and was renamed \
                 to `{id}` -- verify this is the node you expect, and rename by hand if `{id}` \
                 is not a good AstRS id for it"
            ),
        ));
    }

    // Seeded via `with_path` purely for a starting value (mirrors
    // `crate::dora::convert::convert_node`'s own comment): `path` is
    // cleared immediately below since a `ros2:`-sourced node's source kind
    // is `ros2`, not `path` -- leaving the seeded `Some(String::new())`
    // in place would make `astrs validate` see *two* declared source
    // kinds on every migrated node (`MultipleSourcesDeclared`).
    let mut node = astrs_manifest::Node::with_path(id.clone(), String::new());
    node.path = None;

    let namespace = if discovered.namespace.is_empty() {
        None
    } else {
        Some(discovered.namespace.clone())
    };
    node.ros2 = Some(astrs_manifest::Ros2Config {
        compat: astrs_manifest::RosCompat::Humble,
        topic: discovered
            .remaps
            .first()
            .map(|r| resolve_ros_name(&r.to, &discovered.namespace, &local_name)),
        message_type: None,
        direction: None,
        topics: Vec::new(),
        service: None,
        action: None,
        role: None,
        qos: None,
        namespace,
        node_name: if name_is_fallback {
            None
        } else {
            Some(local_name.clone())
        },
    });

    node.env = node_env(discovered);
    node.args = node_args(discovered, &id, notes);

    notes.push(bridge_note(discovered, &id, &local_name));
    notes.extend(additional_remap_notes(discovered, &id, &local_name));

    if discovered.is_lifecycle {
        notes.push(MigrationNote::needs_attention(
            &id,
            "this was a <lifecycle_node>, not a plain <node>; the generated `ros2:` bridge \
             scaffold has no equivalent for its managed state machine (Unconfigured -> \
             Inactive -> Active -> Finalized, driven by lifecycle transition services) -- the \
             scaffolded bridge will exchange messages unconditionally rather than only while \
             the original node is Active. Port the lifecycle behavior by hand once this \
             bridge's message_type/direction are filled in"
                .to_string(),
        ));
    }

    for file in &discovered.param_files {
        notes.push(MigrationNote::needs_attention(
            &id,
            format!(
                "param file `{file}` referenced (<param from=\"{file}\"/>); AstRS has no \
                 manifest-level equivalent for bulk ROS parameter files -- port these values by \
                 hand (as `env:` entries, or once bridge-side parameter support exists)"
            ),
        ));
    }

    if !discovered.conditions.is_empty() {
        let rendered = discovered
            .conditions
            .iter()
            .map(|(k, v)| format!("{k}=\"{v}\""))
            .collect::<Vec<_>>()
            .join(", ");
        notes.push(MigrationNote::needs_attention(
            &id,
            format!(
                "this <node> is conditional in the source launch file ({rendered}); this \
                 importer does not evaluate conditions, so it is scaffolded unconditionally -- \
                 add your own conditional wiring by hand if needed"
            ),
        ));
    }

    if discovered.respawn.is_some() || discovered.respawn_delay.is_some() {
        notes.push(MigrationNote::needs_attention(
            &id,
            format!(
                "respawn=\"{}\"{} was set on this ROS node in the launch file; this has no \
                 AstRS equivalent on the generated bridge stanza -- restarting the bridge (via \
                 `restart_policy:`) is not the same as ROS respawning the original node, which \
                 remains under `ros2 launch`'s own control",
                discovered.respawn.as_deref().unwrap_or("?"),
                discovered
                    .respawn_delay
                    .as_deref()
                    .map(|d| format!(", respawn_delay=\"{d}\""))
                    .unwrap_or_default()
            ),
        ));
    }
    if let Some(output) = &discovered.output {
        notes.push(MigrationNote::needs_attention(
            &id,
            format!(
                "output=\"{output}\" was set on this ROS node in the launch file, controlling \
                 where `ros2 launch` sends its stdout/stderr; the bridge scaffold has no \
                 equivalent (it does not spawn the original ROS process)"
            ),
        ));
    }

    for (key, value) in &discovered.extra_attrs {
        notes.push(MigrationNote::needs_attention(
            &id,
            format!(
                "unrecognized attribute `{key}=\"{value}\"` on this <node>; carried through as \
                 this note only -- mirrors `crate::dora`'s handling of an unrecognized YAML \
                 field (blueprint §8.6)"
            ),
        ));
    }

    node
}

fn convert_root_note(raw: &RawNote) -> MigrationNote {
    MigrationNote::root(raw.message.clone())
}

/// Map a [`WalkResult`] onto an AstRS [`astrs_manifest::Manifest`],
/// collecting a [`MigrationNote`] for every construct this importer could
/// not automatically translate, and for every bridge scaffold's inherently
/// unknown `message_type`/`direction` (see this module's top-level docs).
///
/// The returned manifest is not validated -- [`super::migrate_ros2_launch_str`]
/// does that only when a caller explicitly asks (this crate's own tests
/// do, to prove the scaffold is honest); an unvalidated manifest is still
/// visible to a real caller as ordinary `astrs validate` diagnostics
/// rather than a migration failure.
pub(crate) fn convert(walked: &WalkResult) -> (astrs_manifest::Manifest, Vec<MigrationNote>) {
    let mut notes: Vec<MigrationNote> = walked.notes.iter().map(convert_root_note).collect();

    let mut used_ids: BTreeMap<String, u32> = BTreeMap::new();
    let nodes: Vec<astrs_manifest::Node> = walked
        .nodes
        .iter()
        .map(|discovered| convert_node(discovered, &mut used_ids, &mut notes))
        .collect();

    let manifest = astrs_manifest::Manifest {
        // `Manifest::default()`'s `#[derive(Default)]` does not know about
        // `#[serde(default = "...")]`'s custom defaulter functions -- see
        // `crate::dora::convert::convert`'s identical comment.
        astrs: astrs_manifest::DEFAULT_MANIFEST_FORMAT.to_string(),
        health_check_interval: astrs_manifest::default_health_check_interval(),
        nodes,
        ..astrs_manifest::Manifest::default()
    };

    (manifest, notes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::ros2::walk::Remap;

    fn discovered(name: &str) -> DiscoveredNode {
        DiscoveredNode {
            raw_name: Some(name.to_string()),
            package: Some("pkg".to_string()),
            executable: Some("exec".to_string()),
            ..DiscoveredNode::default()
        }
    }

    #[test]
    fn sanitize_namespace_strips_leading_slash_and_joins_with_underscore() {
        assert_eq!(sanitize_namespace_for_id("/robot1"), "robot1");
        assert_eq!(sanitize_namespace_for_id("/robot1/arm"), "robot1_arm");
    }

    #[test]
    fn candidate_id_uses_bare_local_name_at_root_namespace() {
        assert_eq!(build_candidate_id("camera", ""), "camera");
    }

    #[test]
    fn candidate_id_prefixes_sanitized_namespace() {
        assert_eq!(build_candidate_id("camera", "/robot1"), "robot1_camera");
    }

    #[test]
    fn candidate_id_never_sanitizes_the_local_name_itself() {
        // The load-bearing property: a `$(...)` substitution in the local
        // name must survive verbatim so `astrs validate` catches it,
        // rather than this importer silently inventing a resolved value.
        let id = build_candidate_id("$(var robot_name)_controller", "/robot1");
        assert_eq!(id, "robot1_$(var robot_name)_controller");
    }

    #[test]
    fn assign_unique_id_keeps_the_first_occurrence_plain() {
        let mut used = BTreeMap::new();
        let (id, renamed) = assign_unique_id("driver".to_string(), &mut used);
        assert_eq!(id, "driver");
        assert!(!renamed);
    }

    #[test]
    fn assign_unique_id_suffixes_a_collision() {
        let mut used = BTreeMap::new();
        let _ = assign_unique_id("driver".to_string(), &mut used);
        let (id, renamed) = assign_unique_id("driver".to_string(), &mut used);
        assert_eq!(id, "driver_2");
        assert!(renamed);
        let (id3, _) = assign_unique_id("driver".to_string(), &mut used);
        assert_eq!(id3, "driver_3");
    }

    #[test]
    fn plain_node_maps_id_and_ros2_block_with_no_fallback_notes() {
        let d = discovered("camera");
        let mut used = BTreeMap::new();
        let mut notes = Vec::new();
        let node = convert_node(&d, &mut used, &mut notes);
        assert_eq!(node.id, "camera");
        assert!(node.path.is_none());
        assert!(node.git.is_none());
        assert!(node.module.is_none());
        assert!(node.operators.is_none());
        assert!(node.record.is_none());
        // Exactly one source kind is declared -- the point of `path = None`.
        assert!(node.path.is_none() && node.ros2.is_some());
        let ros2 = node.ros2.unwrap();
        assert_eq!(ros2.compat, astrs_manifest::RosCompat::Humble);
        assert_eq!(ros2.node_name.as_deref(), Some("camera"));
        assert_eq!(ros2.topic, None);
        assert_eq!(ros2.message_type, None);
        assert_eq!(ros2.direction, None);
        // A bridge note is always present (message_type is never known).
        assert!(notes.iter().any(|n| n.node.as_deref() == Some("camera")));
    }

    #[test]
    fn missing_name_falls_back_to_executable_with_a_note() {
        let mut d = discovered("ignored");
        d.raw_name = None;
        d.executable = Some("talker".to_string());
        let mut used = BTreeMap::new();
        let mut notes = Vec::new();
        let node = convert_node(&d, &mut used, &mut notes);
        assert_eq!(node.id, "talker");
        assert_eq!(node.ros2.as_ref().unwrap().node_name, None);
        assert!(
            notes
                .iter()
                .any(|n| n.message.contains("no `name=` attribute"))
        );
    }

    #[test]
    fn first_remap_becomes_the_topic_and_further_remaps_are_noted() {
        let mut d = discovered("lidar");
        d.remaps = vec![
            Remap {
                from: "scan_raw".to_string(),
                to: "/scan".to_string(),
            },
            Remap {
                from: "scan_filtered".to_string(),
                to: "filtered".to_string(),
            },
        ];
        let mut used = BTreeMap::new();
        let mut notes = Vec::new();
        let node = convert_node(&d, &mut used, &mut notes);
        assert_eq!(node.ros2.as_ref().unwrap().topic.as_deref(), Some("/scan"));
        assert!(
            notes
                .iter()
                .any(|n| n.message.contains("additional remap") && n.message.contains("filtered"))
        );
    }

    #[test]
    fn relative_remap_target_resolves_against_namespace() {
        let mut d = discovered("lidar");
        d.namespace = "/robot1".to_string();
        d.remaps = vec![Remap {
            from: "scan".to_string(),
            to: "scan".to_string(),
        }];
        let mut used = BTreeMap::new();
        let mut notes = Vec::new();
        let node = convert_node(&d, &mut used, &mut notes);
        assert_eq!(
            node.ros2.as_ref().unwrap().topic.as_deref(),
            Some("/robot1/scan")
        );
    }

    #[test]
    fn params_and_envs_map_to_env_with_env_winning_on_conflict() {
        let mut d = discovered("x");
        d.params = vec![super::super::walk::ParamEntry {
            name: "RATE".to_string(),
            value: "10".to_string(),
        }];
        d.envs = vec![super::super::walk::ParamEntry {
            name: "RATE".to_string(),
            value: "20".to_string(),
        }];
        let mut used = BTreeMap::new();
        let mut notes = Vec::new();
        let node = convert_node(&d, &mut used, &mut notes);
        assert_eq!(
            node.env.get("RATE"),
            Some(&astrs_manifest::EnvValue::String("20".to_string()))
        );
    }

    #[test]
    fn args_are_shell_tokenized() {
        let mut d = discovered("x");
        d.args = Some("--flag \"quoted value\"".to_string());
        let mut used = BTreeMap::new();
        let mut notes = Vec::new();
        let node = convert_node(&d, &mut used, &mut notes);
        assert_eq!(
            node.args,
            vec!["--flag".to_string(), "quoted value".to_string()]
        );
        // A bridge note is always present (message_type is never known),
        // but tokenizing well-formed args must not add one of its own.
        assert!(notes.iter().all(|n| !n.message.contains("tokenized")));
    }

    #[test]
    fn unbalanced_args_quoting_notes_and_leaves_empty() {
        let mut d = discovered("x");
        d.args = Some("--flag \"unterminated".to_string());
        let mut used = BTreeMap::new();
        let mut notes = Vec::new();
        let node = convert_node(&d, &mut used, &mut notes);
        assert!(node.args.is_empty());
        assert!(notes.iter().any(|n| n.message.contains("tokenized")));
    }

    #[test]
    fn param_file_is_noted() {
        let mut d = discovered("x");
        d.param_files = vec!["cfg.yaml".to_string()];
        let mut used = BTreeMap::new();
        let mut notes = Vec::new();
        let _ = convert_node(&d, &mut used, &mut notes);
        assert!(notes.iter().any(|n| n.message.contains("cfg.yaml")));
    }

    #[test]
    fn condition_is_noted() {
        let mut d = discovered("x");
        d.conditions = vec![("if".to_string(), "$(var use_a)".to_string())];
        let mut used = BTreeMap::new();
        let mut notes = Vec::new();
        let _ = convert_node(&d, &mut used, &mut notes);
        assert!(
            notes
                .iter()
                .any(|n| n.message.contains("conditional") && n.message.contains("use_a"))
        );
    }

    #[test]
    fn respawn_and_output_are_noted() {
        let mut d = discovered("x");
        d.respawn = Some("true".to_string());
        d.output = Some("screen".to_string());
        let mut used = BTreeMap::new();
        let mut notes = Vec::new();
        let _ = convert_node(&d, &mut used, &mut notes);
        assert!(notes.iter().any(|n| n.message.contains("respawn=")));
        assert!(notes.iter().any(|n| n.message.contains("output=")));
    }

    #[test]
    fn lifecycle_node_gets_its_own_note_and_still_maps_like_a_node() {
        let mut d = discovered("managed");
        d.is_lifecycle = true;
        let mut used = BTreeMap::new();
        let mut notes = Vec::new();
        let node = convert_node(&d, &mut used, &mut notes);
        assert_eq!(node.id, "managed");
        assert!(
            notes
                .iter()
                .any(|n| n.message.contains("lifecycle_node") && n.message.contains("Active"))
        );
    }

    #[test]
    fn a_plain_node_gets_no_lifecycle_note() {
        let d = discovered("plain");
        let mut used = BTreeMap::new();
        let mut notes = Vec::new();
        let _ = convert_node(&d, &mut used, &mut notes);
        assert!(notes.iter().all(|n| !n.message.contains("lifecycle_node")));
    }

    #[test]
    fn unrecognized_node_attribute_is_noted() {
        let mut d = discovered("x");
        d.extra_attrs = vec![("launch-prefix".to_string(), "gdb --args".to_string())];
        let mut used = BTreeMap::new();
        let mut notes = Vec::new();
        let _ = convert_node(&d, &mut used, &mut notes);
        assert!(
            notes
                .iter()
                .any(|n| n.message.contains("launch-prefix") && n.message.contains("gdb --args"))
        );
    }

    #[test]
    fn duplicate_candidate_ids_are_disambiguated_with_a_note() {
        let walked = WalkResult {
            nodes: vec![discovered("driver"), discovered("driver")],
            notes: Vec::new(),
        };
        let (manifest, notes) = convert(&walked);
        assert_eq!(manifest.nodes[0].id, "driver");
        assert_eq!(manifest.nodes[1].id, "driver_2");
        assert!(notes.iter().any(|n| n.message.contains("collided")));
    }

    #[test]
    fn root_notes_pass_through_with_no_node_scope() {
        let walked = WalkResult {
            nodes: Vec::new(),
            notes: vec![RawNote {
                message: "composable node container `c` ...".to_string(),
            }],
        };
        let (_manifest, notes) = convert(&walked);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].node, None);
    }

    #[test]
    fn root_defaults_are_astrs_defaults_not_derived_zero_values() {
        let walked = WalkResult::default();
        let (manifest, _) = convert(&walked);
        assert_eq!(manifest.astrs, astrs_manifest::DEFAULT_MANIFEST_FORMAT);
        assert_eq!(
            manifest.health_check_interval,
            astrs_manifest::default_health_check_interval()
        );
    }
}
