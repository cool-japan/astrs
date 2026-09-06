//! Turning `Manifest::to_yaml()` output plus a [`MigrationNote`] list into
//! the final text `astrs migrate from-dora` prints or writes.
//!
//! YAML comments cannot ride inside a `serde`-derived struct — there is no
//! field to hang one off of — so "never silently drop an unmapped
//! construct" (blueprint §8.6) has to happen as a **textual**
//! post-processing pass over the already-serialized manifest, anchored on
//! each node's `- id: <id>` line. That anchor is reliable because
//! [`astrs_manifest::Node::id`] is declared first in the struct and is
//! never `skip_serializing_if`, so it is always the first line of that
//! node's YAML sequence-item block (see [`crate::dora::convert`]'s own
//! docs for the one place this line-matching approach cannot see through:
//! a node id that YAML itself is forced to single-quote rather than
//! double-quote or leave bare — not reachable through AstRS's own
//! `[a-zA-Z0-9_.-]+` id charset, so not a concern for output this crate
//! itself produced from a *valid* migrated manifest).

use std::collections::BTreeMap;

use super::convert::{MigrationNote, NoteSeverity};

/// The banner placed at the very top of every migrated file, regardless of
/// whether any notes were produced.
const HEADER: &str = "# Migrated from a dora-rs dataflow descriptor by `astrs migrate from-dora`.";

/// Render the final migrated YAML text: [`HEADER`] plus a summary line,
/// every root-scoped note, then `yaml` verbatim with a `TODO(astrs
/// migrate)` (or `TODO(astrs migrate) [dropped]`) comment block inserted
/// directly above each node's `- id: <id>` line for every note scoped to
/// that node.
///
/// Deterministic: the header and root notes always render in the same
/// order notes were pushed during conversion, and every per-node insertion
/// point is derived purely from `yaml`'s own (already-deterministic, per
/// [`astrs_manifest::Manifest::to_yaml`]) line order.
pub(crate) fn render(yaml: &str, notes: &[MigrationNote]) -> String {
    let (root_notes, by_node) = group_by_node(notes);

    let mut out = String::new();
    out.push_str(HEADER);
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
/// return `<id>` with at most one layer of double-quoting stripped —
/// `astrs_yaml` double-quotes a plain scalar that would otherwise
/// round-trip as a different YAML type (a purely numeric id, for
/// instance).
fn node_id_of_line(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("- id: ")?;
    Some(
        rest.strip_prefix('"')
            .and_then(|inner| inner.strip_suffix('"'))
            .unwrap_or(rest),
    )
}

/// The leading whitespace of `line`, so an inserted comment lines up with
/// the sequence item it precedes.
fn leading_whitespace(line: &str) -> &str {
    let trimmed = line.trim_start();
    &line[..line.len() - trimmed.len()]
}

/// Append one note as an `indent`-prefixed YAML comment block: a tagged
/// first line, then a plain continuation line per remaining line of
/// `note.message` (a note's message is free-form prose and may itself
/// contain newlines, e.g. the `ros2:` block echo).
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
        // `convert::MigrationNote`'s constructors are private to that
        // module; parse one through a throwaway conversion so this test
        // stays black-box against `render` alone (this exercises
        // `MigrationNote`'s public field shape only, no crate-internal
        // access).
        MigrationNote {
            node: Some(node.to_string()),
            severity: NoteSeverity::NeedsAttention,
            message: message.to_string(),
        }
    }

    #[test]
    fn no_notes_renders_just_the_header_and_all_clear_line() {
        let out = render("nodes:\n  - id: x\n    path: ./x\n", &[]);
        assert!(out.starts_with(HEADER));
        assert!(out.contains("No unmapped constructs"));
        assert!(!out.contains("TODO(astrs migrate)"));
    }

    #[test]
    fn node_scoped_note_is_inserted_directly_above_its_id_line() {
        let yaml =
            "nodes:\n  - id: camera\n    path: ./camera\n  - id: planner\n    path: ./planner\n";
        let notes = vec![note("planner", "example note")];
        let out = render(yaml, &notes);
        let lines: Vec<&str> = out.lines().collect();
        let planner_idx = lines
            .iter()
            .position(|l| l.trim() == "- id: planner")
            .unwrap();
        assert!(lines[planner_idx - 1].contains("TODO(astrs migrate): example note"));
        // camera's block is untouched.
        let camera_idx = lines
            .iter()
            .position(|l| l.trim() == "- id: camera")
            .unwrap();
        assert!(!lines[camera_idx - 1].contains("TODO"));
    }

    #[test]
    fn root_scoped_note_appears_before_any_node() {
        let yaml = "nodes:\n  - id: x\n    path: ./x\n";
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
    fn dropped_severity_gets_its_own_tag() {
        let yaml = "nodes:\n  - id: x\n    path: ./x\n";
        let notes = vec![MigrationNote {
            node: Some("x".to_string()),
            severity: NoteSeverity::Dropped,
            message: "gone".to_string(),
        }];
        let out = render(yaml, &notes);
        assert!(out.contains("TODO(astrs migrate) [dropped]: gone"));
    }

    #[test]
    fn multiple_notes_on_the_same_node_all_appear_in_order() {
        let yaml = "nodes:\n  - id: x\n    path: ./x\n";
        let notes = vec![note("x", "first"), note("x", "second")];
        let out = render(yaml, &notes);
        let first_pos = out.find("first").unwrap();
        let second_pos = out.find("second").unwrap();
        assert!(first_pos < second_pos);
    }

    #[test]
    fn multiline_message_becomes_a_continuation_comment() {
        let yaml = "nodes:\n  - id: x\n    path: ./x\n";
        let notes = vec![note("x", "first line\nsecond line")];
        let out = render(yaml, &notes);
        assert!(out.contains("TODO(astrs migrate): first line"));
        assert!(out.contains("#   second line"));
    }

    #[test]
    fn quoted_numeric_id_is_still_recognized() {
        assert_eq!(node_id_of_line("  - id: \"123\""), Some("123"));
        assert_eq!(node_id_of_line("  - id: camera"), Some("camera"));
        assert_eq!(node_id_of_line("    path: ./x"), None);
    }

    #[test]
    fn rendering_is_deterministic_across_repeated_calls() {
        let yaml = "nodes:\n  - id: x\n    path: ./x\n  - id: y\n    path: ./y\n";
        let notes = vec![note("x", "a"), note("y", "b")];
        assert_eq!(render(yaml, &notes), render(yaml, &notes));
    }
}
