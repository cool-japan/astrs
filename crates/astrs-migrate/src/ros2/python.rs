//! `astrs migrate from-ros2`'s Python launch-file path (blueprint §8.6,
//! §17): Python launch files are **never executed or parsed** -- there is
//! no Python interpreter or AST parser anywhere in this pure-Rust
//! workspace (blueprint §18), and even a full Python parser could not
//! know what `generate_launch_description()` actually returns without
//! running it. Instead this module emits an empty manifest skeleton
//! (`nodes: []`) plus a note explaining that Python launch files need
//! manual porting, and -- best-effort, clearly marked unverified -- a
//! `TODO(astrs migrate)` comment per `Node(...)`/`ComposableNode(...)`/
//! `LifecycleNode(...)` call found by a light source-text skim.
//!
//! # The skim, precisely
//!
//! Not a regular expression: extracting a Python call's keyword arguments
//! needs to find the matching close paren for the call's open paren, and a
//! multi-line, arbitrarily-nested `Node(...)` call (the overwhelmingly
//! common real-world shape -- one kwarg per line) is not something a
//! single regular expression does reliably. This module instead does the
//! minimum extra structure that makes that tractable: for each `Node(`
//! (or `ComposableNode(`/`LifecycleNode(`) substring found in the source
//! text, [`find_call_body`] scans forward counting `(`/`[`/`{` depth
//! (skipping the contents of `'...'`/`"..."` string literals, so a stray
//! bracket inside a quoted value never confuses it) until that depth
//! returns to zero, and [`extract_kwarg`] then looks for
//! `key = 'literal'`/`key = "literal"` inside that span. A keyword whose
//! value is not a plain quoted string literal (a variable, an f-string, a
//! list) is not extracted -- this is a skim, not a Python parser, and a
//! wrong guess is worse than an absent one (blueprint §8.6: "never guess
//! silently").
//!
//! Every candidate found is rendered as a **YAML comment only** -- never a
//! live node in `nodes:` -- since nothing this skim finds was verified
//! against the real, executed launch description.

use super::convert::MigrationNote;

/// One `Node`/`ComposableNode`/`LifecycleNode` constructor call found by
/// the skim.
struct Candidate {
    /// The exact identifier the call used, e.g. `"Node"` or
    /// `"ComposableNode"` (module-qualified prefixes such as
    /// `launch_ros.actions.` are not included -- see [`identifier_at`]).
    kind: String,
    /// The 1-based source line the call starts on, for a human to locate
    /// it quickly.
    line: usize,
    package: Option<String>,
    /// `executable=` for `Node`/`LifecycleNode`, `plugin=` for
    /// `ComposableNode` (ROS 2's own constructor uses a different keyword
    /// for each; both are looked up and whichever is found wins).
    executable_or_plugin: Option<String>,
    name: Option<String>,
    namespace: Option<String>,
}

/// Skim `source` for constructor calls and return one [`MigrationNote`]
/// explaining that Python is not parsed, followed by one note per
/// candidate found (in source order) -- always root-scoped, since nothing
/// here becomes a real manifest node.
pub(crate) fn skim_notes(source: &str) -> Vec<MigrationNote> {
    let mut notes = vec![MigrationNote::root(
        "this is a Python launch file; AstRS does not execute or parse Python launch \
         descriptions (blueprint §18: this workspace is pure Rust, and even a full Python \
         parser could not know what generate_launch_description() returns without running it) \
         -- port its nodes by hand. The candidate(s) below (if any) were found by a best-effort \
         source-text skim, are UNVERIFIED, and may be wrong or incomplete."
            .to_string(),
    )];
    notes.extend(
        find_candidates(source)
            .into_iter()
            .map(render_candidate_note),
    );
    notes
}

fn render_candidate_note(candidate: Candidate) -> MigrationNote {
    let label = candidate
        .name
        .clone()
        .or_else(|| candidate.executable_or_plugin.clone())
        .unwrap_or_else(|| "node".to_string());
    let plugin_key = if candidate.kind == "ComposableNode" {
        "plugin"
    } else {
        "executable"
    };
    let mut message = format!(
        "UNVERIFIED candidate from `{}(...)` at line ~{}: package=`{}`, {plugin_key}=`{}`, \
         name=`{}`{}. Found by a source-text skim, not by running the launch file -- verify \
         against the real launch description before trusting any of it. Suggested stanza:\n\
         - id: {label}\n\
         \x20 ros2:\n\
         \x20   compat: humble",
        candidate.kind,
        candidate.line,
        candidate.package.as_deref().unwrap_or("?"),
        candidate.executable_or_plugin.as_deref().unwrap_or("?"),
        candidate.name.as_deref().unwrap_or("?"),
        candidate
            .namespace
            .as_deref()
            .map(|ns| format!(", namespace=`{ns}`"))
            .unwrap_or_default(),
    );
    if candidate.name.is_some() {
        message.push_str(&format!("\n    node_name: {label}"));
    }
    if let Some(ns) = &candidate.namespace {
        message.push_str(&format!("\n    namespace: {ns}"));
    }
    if candidate.kind == "ComposableNode" {
        message.push_str(
            "\n  # composable nodes are not scaffolded as bridge stanzas -- port by hand once \
             you know the container and topics it publishes/subscribes",
        );
    }
    MigrationNote::root(message)
}

/// The constructor identifiers this skim recognizes -- see this module's
/// top-level docs for why only these three.
const RECOGNIZED_KINDS: [&str; 3] = ["Node", "ComposableNode", "LifecycleNode"];

fn find_candidates(source: &str) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = source[search_from..].find("Node(") {
        let name_end = search_from + rel + "Node".len();
        let open_paren = name_end; // index of the '(' itself
        let ident_start = identifier_start(source, name_end);
        let kind = &source[ident_start..name_end];

        if RECOGNIZED_KINDS.contains(&kind)
            && let Some(body) = find_call_body(source, open_paren)
        {
            candidates.push(Candidate {
                kind: kind.to_string(),
                line: line_number(source, ident_start),
                package: extract_kwarg(body, "package"),
                executable_or_plugin: extract_kwarg(body, "executable")
                    .or_else(|| extract_kwarg(body, "plugin")),
                name: extract_kwarg(body, "name"),
                namespace: extract_kwarg(body, "namespace"),
            });
        }
        search_from = open_paren + 1;
    }
    candidates
}

/// Walk backward from `end` (the index right after the last identifier
/// character, i.e. the position of `Node`'s own trailing `(`) to the start
/// of the full identifier, e.g. `"ComposableNode"` rather than just
/// `"Node"` for a source snippet reading `...ComposableNode(...`. Also
/// correctly stops at a `.` (a module-qualified call such as
/// `launch_ros.actions.Node(`), since `.` is not an identifier character.
fn identifier_start(source: &str, end: usize) -> usize {
    let bytes = source.as_bytes();
    let mut i = end;
    while i > 0 {
        let b = bytes[i - 1];
        if b.is_ascii_alphanumeric() || b == b'_' {
            i -= 1;
        } else {
            break;
        }
    }
    i
}

fn line_number(source: &str, byte_index: usize) -> usize {
    source[..byte_index].bytes().filter(|&b| b == b'\n').count() + 1
}

/// From `open_paren_idx` (the index of a call's opening `(`), scan forward
/// tracking bracket depth -- `(`/`[`/`{` increment, their matches
/// decrement -- skipping the contents of `'...'`/`"..."` string literals
/// entirely (including an escaped quote inside one), and return the
/// substring strictly between the opening and matching closing bracket
/// once depth returns to zero. Returns `None` if the source ends before
/// the matching bracket is found (a truncated snippet, or this skim's
/// heuristic simply being wrong about where the call started) -- the
/// caller then treats this occurrence as unextractable rather than
/// guessing at a body.
fn find_call_body(source: &str, open_paren_idx: usize) -> Option<&str> {
    let bytes = source.as_bytes();
    debug_assert_eq!(bytes.get(open_paren_idx), Some(&b'('));
    let mut depth: i32 = 0;
    let mut in_string: Option<u8> = None;
    let mut escaped = false;
    let mut i = open_paren_idx;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(quote) = in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == quote {
                in_string = None;
            }
        } else {
            match b {
                b'\'' | b'"' => in_string = Some(b),
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&source[open_paren_idx + 1..i]);
                    }
                }
                b'#' if depth == 1 => {
                    // A `#` comment inside the call's top-level argument
                    // list runs to end of line; skip past it so a `)` in a
                    // trailing comment can never be mistaken for the
                    // call's own close paren. No newline left to skip to
                    // means the call never closes at all, which is already
                    // this function's `None`.
                    i += source[i..].find('\n')?;
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Find `key = 'literal'` or `key = "literal"` inside `body` (a plain
/// substring search bounded by a word boundary before `key`, so
/// `executable` never matches inside `plugin_executable` or similar) and
/// return the literal's already-unescaped-enough text. Returns `None` if
/// `key` is absent, or present but not followed by a plain quoted string
/// literal (a variable, an expression, an f-string) -- extracting only
/// what is written verbatim, never evaluating anything.
fn extract_kwarg(body: &str, key: &str) -> Option<String> {
    let mut search_from = 0usize;
    while let Some(rel) = body[search_from..].find(key) {
        let start = search_from + rel;
        let before_ok = start == 0
            || !(body.as_bytes()[start - 1].is_ascii_alphanumeric()
                || body.as_bytes()[start - 1] == b'_');
        let after = &body[start + key.len()..];
        let after_key = after.trim_start();
        if before_ok && after_key.starts_with('=') && !after_key.starts_with("==") {
            let value_part = after_key[1..].trim_start();
            if let Some(literal) = read_quoted_literal(value_part) {
                return Some(literal);
            }
        }
        search_from = start + key.len();
    }
    None
}

/// If `text` starts with a `'...'` or `"..."` string literal, return its
/// (unescaped-for-the-quote-character-only) contents.
fn read_quoted_literal(text: &str) -> Option<String> {
    let quote = text.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut i = 1usize;
    let mut escaped = false;
    while i < bytes.len() {
        let b = bytes[i];
        if escaped {
            out.push(b as char);
            escaped = false;
        } else if b == b'\\' {
            escaped = true;
        } else if b == quote as u8 {
            return Some(out);
        } else {
            out.push(b as char);
        }
        i += 1;
    }
    None // unterminated literal
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn always_includes_the_general_python_note_first() {
        let notes = skim_notes("# nothing here");
        assert_eq!(notes.len(), 1);
        assert!(notes[0].message.contains("does not execute or parse"));
        assert_eq!(notes[0].node, None);
    }

    #[test]
    fn finds_a_single_line_node_call() {
        let src = "Node(package='demo_nodes_cpp', executable='talker', name='talker')";
        let candidates = find_candidates(src);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, "Node");
        assert_eq!(candidates[0].package.as_deref(), Some("demo_nodes_cpp"));
        assert_eq!(
            candidates[0].executable_or_plugin.as_deref(),
            Some("talker")
        );
        assert_eq!(candidates[0].name.as_deref(), Some("talker"));
    }

    #[test]
    fn finds_a_realistic_multiline_node_call() {
        let src = "\
def generate_launch_description():
    return LaunchDescription([
        Node(
            package='demo_nodes_cpp',
            executable='talker',
            name='talker',
            namespace='robot1',
            output='screen',
        ),
    ])
";
        let candidates = find_candidates(src);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].namespace.as_deref(), Some("robot1"));
        assert_eq!(candidates[0].line, 3);
    }

    #[test]
    fn distinguishes_composable_node_from_plain_node() {
        let src = "ComposableNode(package='image_proc', plugin='image_proc::RectifyNode', name='rectify')";
        let candidates = find_candidates(src);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, "ComposableNode");
        assert_eq!(
            candidates[0].executable_or_plugin.as_deref(),
            Some("image_proc::RectifyNode")
        );
    }

    #[test]
    fn module_qualified_call_is_still_recognized() {
        let src = "launch_ros.actions.Node(package='p', executable='e')";
        let candidates = find_candidates(src);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].kind, "Node");
    }

    #[test]
    fn a_bracket_inside_a_string_literal_does_not_confuse_the_scanner() {
        let src = "Node(package='p', executable='e', arguments=['--flag', 'a)b'])";
        let candidates = find_candidates(src);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].package.as_deref(), Some("p"));
    }

    #[test]
    fn a_hash_comment_inside_the_call_does_not_confuse_the_scanner() {
        let src = "Node(\n  package='p',  # a trailing ) comment\n  executable='e',\n)";
        let candidates = find_candidates(src);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].executable_or_plugin.as_deref(), Some("e"));
    }

    #[test]
    fn a_variable_value_is_not_extracted() {
        let src = "Node(package='p', executable=exec_name_var, name='n')";
        let candidates = find_candidates(src);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].executable_or_plugin, None);
        assert_eq!(candidates[0].name.as_deref(), Some("n"));
    }

    #[test]
    fn an_unterminated_call_is_skipped_not_panicked_on() {
        let src = "Node(package='p', executable='e'";
        assert_eq!(find_candidates(src).len(), 0);
    }

    #[test]
    fn a_call_with_no_recognized_kind_is_ignored() {
        let src = "SomeOtherNode(package='p')";
        // "SomeOtherNode(" contains "Node(" as a substring, but the full
        // identifier "SomeOtherNode" is not one of the three recognized
        // kinds, so nothing is extracted from it.
        assert_eq!(find_candidates(src).len(), 0);
    }

    #[test]
    fn empty_source_yields_no_candidates() {
        assert_eq!(find_candidates("").len(), 0);
    }

    #[test]
    fn multiple_candidates_are_found_in_source_order() {
        let src = "Node(name='a')\nNode(name='b')\n";
        let candidates = find_candidates(src);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].name.as_deref(), Some("a"));
        assert_eq!(candidates[1].name.as_deref(), Some("b"));
    }

    #[test]
    fn skim_notes_renders_a_suggested_stanza_for_each_candidate() {
        let notes = skim_notes("Node(package='p', executable='e', name='n')");
        assert_eq!(notes.len(), 2);
        assert!(notes[1].message.contains("- id: n"));
        assert!(notes[1].message.contains("compat: humble"));
        assert!(notes[1].message.contains("UNVERIFIED"));
    }
}
