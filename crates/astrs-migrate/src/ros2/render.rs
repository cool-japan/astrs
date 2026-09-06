//! Turning `Manifest::to_yaml()` output plus a [`MigrationNote`] list into
//! the final text `astrs migrate from-ros2` prints or writes.
//!
//! Deliberately parallel to [`crate::dora::render`] rather than sharing
//! code with it: the algorithm is identical (anchor each node-scoped note
//! as a comment block directly above that node's `- id: <id>` line, root
//! notes as a header block), but it operates on this module's own
//! [`MigrationNote`] type, not `crate::dora`'s -- see this crate's ownership
//! split (`crate::dora` and `crate::ros2` are maintained independently).
//! See [`crate::dora::render`]'s own docs for why line-matching on `- id:
//! <id>` is reliable here too: [`astrs_manifest::Node::id`] is declared
//! first in the struct and never `skip_serializing_if`, so it is always
//! the first line of a node's YAML block. Unlike `crate::dora::render`
//! (whose input ids are always valid AstRS id-charset strings),
//! this importer's own ids can legitimately contain an unresolved
//! `$(...)` substitution (blueprint §8.6, this module's parent docs) --
//! confirmed against `astrs_yaml`'s actual output, `$`/`(`/`)` do **not**
//! force quoting (a plain scalar tolerates them), so the common case
//! needs no special handling; [`node_id_of_line`] still strips a
//! surrounding quote defensively for the id shapes that genuinely do get
//! quoted (numeric-looking, or containing a YAML-significant character
//! such as `:` or a leading `-`), in case a future launch construct
//! produces one of those.

use std::collections::BTreeMap;

use super::convert::{MigrationNote, NoteSeverity};

/// The banner placed at the very top of every migrated XML-launch-file
/// output, regardless of whether any notes were produced.
const XML_HEADER: &str = "# Migrated from a ROS 2 launch file by `astrs migrate from-ros2`.";

/// The banner placed at the top of a Python-launch-file skeleton (see
/// [`super::python`]) -- distinct wording since that path never actually
/// parses its input.
const PYTHON_HEADER: &str = "# Skeleton scaffold from a ROS 2 Python launch file by `astrs \
migrate from-ros2` -- Python launch files are not executed or parsed by this importer.";

/// Render the final migrated YAML text for the XML launch-file path:
/// [`XML_HEADER`] plus a summary line, every root-scoped note, then `yaml`
/// verbatim with a `TODO(astrs migrate)` comment block inserted directly
/// above each node's `- id: <id>` line for every note scoped to that node.
pub(crate) fn render(yaml: &str, notes: &[MigrationNote]) -> String {
    render_with_header(XML_HEADER, yaml, notes)
}

/// [`render`], but with [`PYTHON_HEADER`] instead -- used only by
/// [`super::python`], whose output is always `nodes: []` plus root-scoped
/// notes (a Python-derived candidate is never a real manifest node; see
/// that module's docs), so the per-node insertion logic below is exercised
/// only by its shared tests, not by that call site.
pub(crate) fn render_python(yaml: &str, notes: &[MigrationNote]) -> String {
    render_with_header(PYTHON_HEADER, yaml, notes)
}

fn render_with_header(header: &str, yaml: &str, notes: &[MigrationNote]) -> String {
    let (root_notes, by_node) = group_by_node(notes);

    let mut out = String::new();
    out.push_str(header);
    out.push('\n');
    if notes.is_empty() {
        out.push_str("# No unmapped constructs were found.\n");
    } else {
        let count = notes.len();
        let plural = if count == 1 { "" } else { "s" };
        out.push_str(&format!(
            "# {count} note{plural} below need attention -- search for \"TODO(astrs migrate)\".\n"
        ));
    }
    for note in &root_notes {
        push_comment_block(&mut out, "", note);
    }
    out.push('\n');

    for line in yaml.lines() {
        if let Some(id) = node_id_of_line(line)
            && let Some(notes_for_node) = by_node.get(id)
        {
            let indent = leading_whitespace(line);
            for note in notes_for_node {
                push_comment_block(&mut out, indent, note);
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Split `notes` into root-scoped notes (in original order) and a lookup
/// from node id to that node's notes (also in original order).
fn group_by_node(
    notes: &[MigrationNote],
) -> (Vec<&MigrationNote>, BTreeMap<&str, Vec<&MigrationNote>>) {
    let mut root = Vec::new();
    let mut by_node: BTreeMap<&str, Vec<&MigrationNote>> = BTreeMap::new();
    for note in notes {
        match note.node.as_deref() {
            None => root.push(note),
            Some(id) => by_node.entry(id).or_default().push(note),
        }
    }
    (root, by_node)
}

/// If `line` is a (possibly indented) `- id: <id>` sequence-item line,
/// return `<id>` with at most one layer of quoting stripped. `astrs_yaml`
/// quotes a plain scalar that would otherwise round-trip as a different
/// YAML type (a purely numeric id) or that starts with a
/// YAML-significant character; a `$(...)` substitution alone does *not*
/// trigger this (confirmed empirically -- see this module's top-level
/// docs), so it round-trips bare. Both single- and double-quoted forms
/// are stripped here regardless, matching [`crate::dora::render`]'s own
/// handling of the numeric-id case.
fn node_id_of_line(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("- id: ")?;
    for quote in ['"', '\''] {
        if let Some(inner) = rest.strip_prefix(quote).and_then(|r| r.strip_suffix(quote)) {
            return Some(inner);
        }
    }
    Some(rest)
}

/// The leading whitespace of `line`, so an inserted comment lines up with
/// the sequence item it precedes.
fn leading_whitespace(line: &str) -> &str {
    let trimmed = line.trim_start();
    &line[..line.len() - trimmed.len()]
}

/// Append one note as an `indent`-prefixed YAML comment block: a tagged
/// first line, then a plain continuation line per remaining line of
/// `note.message`.
fn push_comment_block(out: &mut String, indent: &str, note: &MigrationNote) {
    let tag = match note.severity {
        NoteSeverity::Dropped => "TODO(astrs migrate) [dropped]",
        NoteSeverity::NeedsAttention => "TODO(astrs migrate)",
    };
    for (i, line) in note.message.lines().enumerate() {
        if i == 0 {
            out.push_str(indent);
            out.push_str("# ");
            out.push_str(tag);
            out.push_str(": ");
            out.push_str(line);
        } else {
            out.push_str(indent);
            out.push_str("#   ");
            out.push_str(line);
        }
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn note(node: &str, message: &str) -> MigrationNote {
        MigrationNote {
            node: Some(node.to_string()),
            severity: NoteSeverity::NeedsAttention,
            message: message.to_string(),
        }
    }

    #[test]
    fn no_notes_renders_just_the_header_and_all_clear_line() {
        let out = render("nodes:\n  - id: x\n    ros2: {}\n", &[]);
        assert!(out.starts_with(XML_HEADER));
        assert!(out.contains("No unmapped constructs"));
        assert!(!out.contains("TODO(astrs migrate)"));
    }

    #[test]
    fn python_header_is_distinct() {
        let out = render_python("nodes: []\n", &[]);
        assert!(out.starts_with(PYTHON_HEADER));
    }

    #[test]
    fn node_scoped_note_is_inserted_directly_above_its_id_line() {
        let yaml = "nodes:\n  - id: camera\n    ros2: {}\n  - id: lidar\n    ros2: {}\n";
        let notes = vec![note("lidar", "example note")];
        let out = render(yaml, &notes);
        let lines: Vec<&str> = out.lines().collect();
        let lidar_idx = lines
            .iter()
            .position(|l| l.trim() == "- id: lidar")
            .unwrap();
        assert!(lines[lidar_idx - 1].contains("TODO(astrs migrate): example note"));
        let camera_idx = lines
            .iter()
            .position(|l| l.trim() == "- id: camera")
            .unwrap();
        assert!(!lines[camera_idx - 1].contains("TODO"));
    }

    #[test]
    fn root_scoped_note_appears_before_any_node() {
        let yaml = "nodes:\n  - id: x\n    ros2: {}\n";
        let notes = vec![MigrationNote {
            node: None,
            severity: NoteSeverity::NeedsAttention,
            message: "root level thing".to_string(),
        }];
        let out = render(yaml, &notes);
        let todo_pos = out.find("TODO(astrs migrate): root level thing").unwrap();
        let node_pos = out.find("- id: x").unwrap();
        assert!(todo_pos < node_pos);
    }

    #[test]
    fn a_bare_substitution_id_is_recognized_unquoted() {
        // The deliberate case this importer produces on purpose (see this
        // module's top-level docs): `astrs_yaml` does *not* quote `$`/`(`/
        // `)`, confirmed against real output in
        // `tests/from_ros2.rs::substitution_in_name_survives_into_id_and_fails_validation_honestly`.
        assert_eq!(
            node_id_of_line("  - id: $(var robot_name)_controller"),
            Some("$(var robot_name)_controller")
        );
    }

    #[test]
    fn a_quoted_id_is_still_recognized_if_astrs_yaml_ever_quotes_one() {
        // Defensive: not observed for this importer's own output today
        // (see the test above), but a numeric-looking id would trigger
        // quoting, exactly like `crate::dora::render`'s identical case.
        assert_eq!(node_id_of_line("  - id: '$(var x)'"), Some("$(var x)"));
        assert_eq!(node_id_of_line("  - id: \"123\""), Some("123"));
        assert_eq!(node_id_of_line("  - id: camera"), Some("camera"));
        assert_eq!(node_id_of_line("    ros2: {}"), None);
    }

    #[test]
    fn multiline_message_becomes_a_continuation_comment() {
        let yaml = "nodes:\n  - id: x\n    ros2: {}\n";
        let notes = vec![note("x", "first line\nsecond line")];
        let out = render(yaml, &notes);
        assert!(out.contains("TODO(astrs migrate): first line"));
        assert!(out.contains("#   second line"));
    }

    #[test]
    fn rendering_is_deterministic_across_repeated_calls() {
        let yaml = "nodes:\n  - id: x\n    ros2: {}\n  - id: y\n    ros2: {}\n";
        let notes = vec![note("x", "a"), note("y", "b")];
        assert_eq!(render(yaml, &notes), render(yaml, &notes));
    }
}
