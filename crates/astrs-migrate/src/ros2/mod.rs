//! `astrs migrate from-ros2`: skimming a ROS 2 launch file into a bridge
//! manifest scaffold (blueprint §8.6, §10.5, §17).
//!
//! Mirrors [`crate::dora`]'s architecture -- parse, map, note everything
//! unmappable, render -- across a different, larger split because a ROS 2
//! launch file has no natural `#[derive(Deserialize)]` shape the way
//! dora's YAML descriptor does:
//!
//! - `xml` (crate-private): a generic XML element tree, built on the
//!   workspace's `quick-xml` shim (`oxixml-quickxml-compat`) -- the
//!   syntax-level "model" layer, understanding nothing about launch-file
//!   semantics.
//! - `walk` (crate-private): the semantic walk over that tree --
//!   namespace composition through `<group>`/`<push_ros_namespace>`,
//!   `<node>` discovery (`pkg`/`exec`/`name`/`namespace`/`remap`/`param`
//!   `+` param files, per blueprint §17's own scope line), composable node
//!   containers, and `<include>` recursion (relative-path resolution,
//!   depth limiting, cycle detection).
//! - `convert` (crate-private): maps every discovered `<node>` onto an
//!   [`astrs_manifest::Node`] with a `ros2:` bridge block scaffold (§10.5),
//!   collecting a [`MigrationNote`] for every construct with no automatic
//!   AstRS equivalent -- and, always, for a bridge's inherently
//!   unknowable `message_type`/`direction` (see that module's docs for
//!   why guessing either would be worse than leaving them unset).
//! - `render` (crate-private): turns those notes into `TODO(astrs
//!   migrate)` YAML comments next to the node they describe, exactly like
//!   `crate::dora::render` (a separate, parallel implementation, not
//!   shared code -- see that module's own docs for why).
//! - `python` (crate-private): the `.py` launch-file path, which never
//!   executes or parses Python at all -- an empty-`nodes:` skeleton plus a
//!   best-effort, clearly-`UNVERIFIED` regex/bracket-matching skim of
//!   `Node(...)`-shaped calls, rendered as comments only.
//!
//! [`migrate_ros2_launch_str`] / [`migrate_ros2_launch_file`] wire these
//! together into the two entry points `astrs-cli`'s `migrate from-ros2`
//! verb calls.
//!
//! # Format detection
//!
//! [`migrate_ros2_launch_file`] dispatches on the path's extension --
//! `.py` is Python, everything else is XML -- matching how `ros2 launch`
//! itself tells the two formats apart. [`migrate_ros2_launch_str`] has no
//! filename to consult, so it sniffs instead: trimmed input starting with
//! `<` is XML, anything else is treated as Python (an XML launch file
//! always starts with `<?xml` or `<launch`; a Python one starts with
//! `import`/`from`/a shebang/a comment -- never `<`).
//!
//! # Why `migrate_ros2_launch_str` never follows `<include>`
//!
//! Resolving a relative `<include file="...">` needs a base directory. A
//! bare string has none, and defaulting to the process's current working
//! directory would make the same input string migrate differently
//! depending on where the caller happens to be running -- a
//! non-reproducible result this crate does not want, especially since
//! `astrs-manifest`'s own analogous problem (`module:` inclusion,
//! §8.5) makes exactly the same call: [`astrs_manifest::Manifest::expand`]
//! *requires* an explicit `base_dir` rather than defaulting one. Every
//! `<include>` reached this way becomes a [`MigrationNote`] explaining
//! why it was not followed and pointing at [`migrate_ros2_launch_file`],
//! which has a real file location to resolve against.

mod convert;
mod python;
mod render;
mod walk;
mod xml;

pub use convert::{MigrationNote, NoteSeverity};
pub use walk::MAX_INCLUDE_DEPTH;

use std::path::Path;

use crate::error::Ros2MigrateError;

/// The result of migrating one ROS 2 launch file.
#[derive(Debug, Clone, PartialEq)]
pub struct MigrationResult {
    /// The migrated manifest, rendered as YAML with `TODO(astrs migrate)`
    /// comments -- see [`crate::dora::MigrationResult::yaml`] for why this
    /// is the text a caller should print or write, not
    /// [`astrs_manifest::Manifest::to_yaml`]'s bare output.
    pub yaml: String,
    /// Every observation made during the mapping, in the order the
    /// mapping pass found them (root-level observations first, then each
    /// node in discovery order).
    pub notes: Vec<MigrationNote>,
}

impl MigrationResult {
    /// How many notes are [`NoteSeverity::NeedsAttention`].
    ///
    /// For the XML path, this is **never zero once at least one `<node>`
    /// was discovered**: `message_type` (and bridge `direction`) can never
    /// be inferred from a launch file alone, so every discovered node
    /// contributes at least one such note (see `convert`'s docs). A
    /// caller surfacing this count (`astrs-cli`'s `FromRos2Report::exit_code`,
    /// for instance) should treat that as expected and honest, not as a
    /// sign something went wrong.
    #[must_use]
    pub fn needs_attention_count(&self) -> usize {
        self.notes
            .iter()
            .filter(|n| n.severity == NoteSeverity::NeedsAttention)
            .count()
    }

    /// How many notes are [`NoteSeverity::Dropped`] (informational). See
    /// [`NoteSeverity::Dropped`]'s own docs: this importer's current logic
    /// does not produce any, so this is `0` today, kept for shape-parity
    /// with [`crate::dora::MigrationResult::dropped_count`].
    #[must_use]
    pub fn dropped_count(&self) -> usize {
        self.notes
            .iter()
            .filter(|n| n.severity == NoteSeverity::Dropped)
            .count()
    }
}

/// Migrate a ROS 2 launch file, given as its text.
///
/// Auto-detects XML vs. Python (see this module's top-level docs); never
/// follows `<include>` (also see this module's top-level docs) -- use
/// [`migrate_ros2_launch_file`] for that.
///
/// # Errors
///
/// Returns [`Ros2MigrateError::Xml`] if `input` looks like XML but is not
/// well-formed, or [`Ros2MigrateError::Render`] if the mapped manifest
/// could not be serialized back to YAML (see that variant's docs for why
/// this is not expected to be reachable in practice). Never returns
/// [`Ros2MigrateError::Io`], [`Ros2MigrateError::IncludeCycle`] or
/// [`Ros2MigrateError::IncludeDepthExceeded`] -- this entry point touches
/// no filesystem.
pub fn migrate_ros2_launch_str(input: &str) -> Result<MigrationResult, Ros2MigrateError> {
    if looks_like_python(input) {
        migrate_python_str(input)
    } else {
        migrate_xml_str(input, "<input>", None)
    }
}

/// [`migrate_ros2_launch_str`], reading the launch file from disk first --
/// and, for XML input, following `<include>`s relative to `path`'s own
/// directory (see this module's top-level docs).
///
/// # Errors
///
/// Returns [`Ros2MigrateError::Io`] if `path` cannot be read, plus
/// everything [`migrate_ros2_launch_str`] can return for its contents,
/// plus [`Ros2MigrateError::IncludeCycle`] / [`Ros2MigrateError::IncludeDepthExceeded`]
/// if `path`'s own `<include>` graph is structurally broken (an
/// individual broken *branch* of it -- one missing/unreadable/malformed
/// included file -- degrades to a [`MigrationNote`] instead; see
/// `walk`'s docs).
pub fn migrate_ros2_launch_file(
    path: impl AsRef<Path>,
) -> Result<MigrationResult, Ros2MigrateError> {
    let path_ref = path.as_ref();
    let content = std::fs::read_to_string(path_ref)
        .map_err(|source| Ros2MigrateError::io(path_ref, source))?;

    if path_ref.extension().and_then(|e| e.to_str()) == Some("py") {
        return migrate_python_str(&content);
    }

    let display_path = path_ref.display().to_string();
    let base_dir = path_ref.parent();
    migrate_xml_str(&content, &display_path, base_dir)
}

/// Whether `input` should be treated as a Python launch file rather than
/// XML -- see this module's top-level docs for the heuristic.
fn looks_like_python(input: &str) -> bool {
    !input.trim_start().starts_with('<')
}

fn migrate_python_str(source: &str) -> Result<MigrationResult, Ros2MigrateError> {
    let notes = python::skim_notes(source);
    let manifest = empty_manifest();
    let base_yaml = manifest
        .to_yaml()
        .map_err(|source| Ros2MigrateError::Render { source })?;
    let yaml = render::render_python(&base_yaml, &notes);
    Ok(MigrationResult { yaml, notes })
}

fn migrate_xml_str(
    input: &str,
    path: &str,
    base_dir: Option<&Path>,
) -> Result<MigrationResult, Ros2MigrateError> {
    let tree = xml::parse(input).map_err(|message| Ros2MigrateError::Xml {
        path: path.to_string(),
        message,
    })?;
    let walked = walk::walk(&tree, base_dir)?;
    let (manifest, notes) = convert::convert(&walked);
    let base_yaml = manifest
        .to_yaml()
        .map_err(|source| Ros2MigrateError::Render { source })?;
    let yaml = render::render(&base_yaml, &notes);
    Ok(MigrationResult { yaml, notes })
}

/// A manifest with AstRS's own documented defaults and no nodes -- the
/// Python skeleton's entire output before notes are rendered on top of
/// it. See `crate::dora::convert::convert`'s identical comment for why
/// `astrs`/`health_check_interval` are set explicitly rather than left to
/// `Manifest::default()`'s derived (non-serde-aware) zero values.
fn empty_manifest() -> astrs_manifest::Manifest {
    astrs_manifest::Manifest {
        astrs: astrs_manifest::DEFAULT_MANIFEST_FORMAT.to_string(),
        health_check_interval: astrs_manifest::default_health_check_interval(),
        nodes: Vec::new(),
        ..astrs_manifest::Manifest::default()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn looks_like_python_detects_xml_by_leading_angle_bracket() {
        assert!(!looks_like_python("<launch></launch>"));
        assert!(!looks_like_python("  \n <?xml version=\"1.0\"?><launch/>"));
    }

    #[test]
    fn looks_like_python_treats_anything_else_as_python() {
        assert!(looks_like_python("import launch\n"));
        assert!(looks_like_python("#!/usr/bin/env python3\n"));
        assert!(looks_like_python(""));
    }

    #[test]
    fn migrate_str_rejects_malformed_xml() {
        let err = migrate_ros2_launch_str("<launch><node>").unwrap_err();
        assert!(matches!(err, Ros2MigrateError::Xml { .. }));
    }

    #[test]
    fn migrate_str_on_a_nodeless_launch_file_validates_clean_with_no_notes() {
        let result = migrate_ros2_launch_str("<launch/>").unwrap();
        assert!(result.notes.is_empty(), "notes: {:?}", result.notes);
        assert!(
            result
                .yaml
                .starts_with("# Migrated from a ROS 2 launch file")
        );
        let manifest = astrs_manifest::Manifest::from_yaml_str(&result.yaml).unwrap();
        manifest.validate().unwrap();
        assert!(manifest.nodes.is_empty());
    }

    #[test]
    fn migrate_str_on_a_single_node_always_needs_attention() {
        let result =
            migrate_ros2_launch_str(r#"<launch><node pkg="p" exec="e" name="n"/></launch>"#)
                .unwrap();
        assert_eq!(result.needs_attention_count(), 1);
        assert_eq!(result.dropped_count(), 0);
        let manifest = astrs_manifest::Manifest::from_yaml_str(&result.yaml).unwrap();
        manifest.validate().unwrap();
    }

    #[test]
    fn migrate_str_never_follows_an_include_and_notes_it() {
        let result =
            migrate_ros2_launch_str(r#"<launch><include file="child.xml"/></launch>"#).unwrap();
        assert!(
            result
                .notes
                .iter()
                .any(|n| n.message.contains("no base directory"))
        );
    }

    #[test]
    fn migrate_str_on_python_source_yields_an_empty_manifest_and_a_note() {
        let result = migrate_ros2_launch_str("import launch\n").unwrap();
        assert!(result.yaml.starts_with("# Skeleton scaffold"));
        assert!(!result.notes.is_empty());
        let manifest = astrs_manifest::Manifest::from_yaml_str(&result.yaml).unwrap();
        manifest.validate().unwrap();
        assert!(manifest.nodes.is_empty());
    }

    #[test]
    fn migrate_file_reports_io_error_for_a_missing_path() {
        let path = std::env::temp_dir().join("astrs-migrate-ros2-mod-test-does-not-exist.xml");
        let err = migrate_ros2_launch_file(&path).unwrap_err();
        assert!(matches!(err, Ros2MigrateError::Io { .. }));
    }

    #[test]
    fn migrate_file_dispatches_python_by_extension_even_without_sniffable_content() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-migrate-ros2-mod-test-{}-py-ext",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Content alone would not sniff as XML *or* obviously as Python;
        // the `.py` extension must still be authoritative.
        let path = dir.join("weird.py");
        std::fs::write(&path, "Node(name='n')").unwrap();
        let result = migrate_ros2_launch_file(&path).unwrap();
        assert!(result.yaml.starts_with("# Skeleton scaffold"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_file_follows_a_real_relative_include() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-migrate-ros2-mod-test-{}-include",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("child.xml"),
            r#"<launch><node pkg="p" exec="e" name="child"/></launch>"#,
        )
        .unwrap();
        let root_path = dir.join("root.xml");
        std::fs::write(
            &root_path,
            r#"<launch><include file="child.xml"/></launch>"#,
        )
        .unwrap();

        let result = migrate_ros2_launch_file(&root_path).unwrap();
        let manifest = astrs_manifest::Manifest::from_yaml_str(&result.yaml).unwrap();
        assert_eq!(manifest.nodes.len(), 1);
        assert_eq!(manifest.nodes[0].id, "child");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
