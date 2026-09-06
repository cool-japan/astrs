//! Keeps `include/astrs.h` honest against `src/*.rs` without cbindgen: every
//! `#[no_mangle]` `extern "C"` function in the Rust source must be declared
//! in the header, and every `astrs_*` prototype the header declares must
//! name a real one — checked bidirectionally, as sets. The two enums'
//! numeric values are checked the same way, against the Rust discriminants
//! themselves, so a value edited on only one side fails the build rather
//! than silently drifting.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use astrs_capi::{AstrsEventType, AstrsStatus};

/// The header, embedded at compile time — not a runtime path, so this stays
/// correct regardless of the process's current working directory.
const HEADER: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/include/astrs.h"));

/// `src/`, resolved from this crate's own manifest directory at compile
/// time (the same `CARGO_MANIFEST_DIR` mechanism `astrs-manifest/tests/
/// expand.rs` uses for its fixture directory) — not a hardcoded developer
/// path, and not the process's working directory, which a test runner is
/// free to set to anything.
fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    files
}

/// Every `#[no_mangle]` (bare or `#[unsafe(no_mangle)]`) `extern "C" fn`
/// name declared anywhere under `src/`.
///
/// Line-oriented on purpose: this crate's own style always puts the
/// attribute on its own line, immediately (within a few lines, past any
/// `pub`/`unsafe`) followed by the `fn` line — see any function in
/// `src/node.rs` or `src/event.rs` for the shape being matched.
fn no_mangle_fn_names_in_src() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for path in rust_files(&src_dir()) {
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed != "#[unsafe(no_mangle)]" && trimmed != "#[no_mangle]" {
                continue;
            }
            // The `fn` line itself is within the next few lines (past
            // `pub`/`unsafe` on their own, never past a blank line or
            // another item).
            for candidate in &lines[index + 1..(index + 5).min(lines.len())] {
                if let Some(name) = extract_fn_name(candidate) {
                    names.insert(name);
                    break;
                }
            }
        }
    }
    names
}

/// Pulls the identifier out of a line of the shape `...extern "C" fn
/// NAME(...`, if it has one.
fn extract_fn_name(line: &str) -> Option<String> {
    let after = line.split_once("extern \"C\" fn ")?.1;
    let name: String = after
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

/// Strips `//` line comments from `text` (this header contains no `/* */`
/// blocks and no string literals that could contain `//`, so this simple
/// pass is exact, not just an approximation).
fn strip_line_comments(text: &str) -> String {
    text.lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every `astrs_*` function name the header declares a prototype for.
///
/// Comment-stripped first, then newlines collapsed to spaces so a
/// prototype wrapped across multiple lines (`astrs_send_output`'s seven
/// parameters) is still one contiguous string to scan — this header has no
/// parameter list containing a nested `(`, so "an `astrs_name` immediately
/// followed by `(`" is unambiguous.
fn declared_fn_names_in_header() -> BTreeSet<String> {
    let cleaned = strip_line_comments(HEADER).replace('\n', " ");
    let mut names = BTreeSet::new();
    let bytes = cleaned.as_bytes();
    let mut search_from = 0usize;
    while let Some(offset) = cleaned[search_from..].find("astrs_") {
        let start = search_from + offset;
        let mut end = start;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        let name = &cleaned[start..end];
        if cleaned[end..].trim_start().starts_with('(') {
            names.insert(name.to_owned());
        }
        search_from = end.max(start + 1);
    }
    names
}

#[test]
fn every_no_mangle_function_is_declared_in_the_header() {
    let names = no_mangle_fn_names_in_src();
    assert!(
        !names.is_empty(),
        "the scan itself must find something real"
    );
    for name in &names {
        assert!(
            HEADER.contains(name.as_str()),
            "`{name}` is `#[no_mangle]` in src/ but does not appear anywhere in include/astrs.h"
        );
    }
}

#[test]
fn every_header_prototype_names_a_real_no_mangle_function() {
    let declared = declared_fn_names_in_header();
    let real = no_mangle_fn_names_in_src();
    assert!(
        !declared.is_empty(),
        "the header scan itself must find something real"
    );
    for name in &declared {
        assert!(
            real.contains(name),
            "`include/astrs.h` declares a prototype for `{name}`, but src/ has no \
             `#[no_mangle] extern \"C\" fn {name}`"
        );
    }
}

#[test]
fn the_header_and_src_declare_exactly_the_same_function_set() {
    let declared = declared_fn_names_in_header();
    let real = no_mangle_fn_names_in_src();
    assert_eq!(
        declared, real,
        "include/astrs.h and src/*.rs disagree on the exported function set"
    );
}

// ---------------------------------------------------------------------------
// Numeric parity: the enum values written in the header must equal the
// Rust discriminants a C caller actually receives.
// ---------------------------------------------------------------------------

/// Parses every `NAME = value` pair out of comment-stripped `text` — this
/// header writes both `AstrsStatus` and `AstrsEventType` in exactly that
/// shape, one enumerator per line.
fn parse_named_values(text: &str) -> Vec<(String, i64)> {
    let mut pairs = Vec::new();
    for line in strip_line_comments(text).lines() {
        let trimmed = line.trim().trim_end_matches(',');
        let Some((name, value)) = trimmed.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if !name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            || name.is_empty()
        {
            continue;
        }
        if let Ok(parsed) = value.parse::<i64>() {
            pairs.push((name.to_owned(), parsed));
        }
    }
    pairs
}

#[test]
fn astrs_status_values_match_the_rust_discriminants() {
    // `parse_named_values` scans the whole header, so it returns both
    // enums' enumerators in one pass; every `AstrsStatus` name happens to
    // exclude "EVENT", which every `AstrsEventType` name includes, so this
    // filter recovers exactly the `AstrsStatus` subset without needing to
    // slice the header text by section.
    let header_values: std::collections::BTreeMap<String, i64> = parse_named_values(HEADER)
        .into_iter()
        .filter(|(name, _)| name.starts_with("ASTRS_") && !name.contains("EVENT"))
        .collect();

    let expected: &[(&str, i32)] = &[
        ("ASTRS_OK", AstrsStatus::Ok as i32),
        (
            "ASTRS_INVALID_ARGUMENT",
            AstrsStatus::InvalidArgument as i32,
        ),
        ("ASTRS_NOT_CONNECTED", AstrsStatus::NotConnected as i32),
        ("ASTRS_CLOSED", AstrsStatus::Closed as i32),
        ("ASTRS_TIMEOUT", AstrsStatus::Timeout as i32),
        ("ASTRS_UNKNOWN_PORT", AstrsStatus::UnknownPort as i32),
        ("ASTRS_TYPE_MISMATCH", AstrsStatus::TypeMismatch as i32),
        ("ASTRS_INTERNAL", AstrsStatus::Internal as i32),
        ("ASTRS_PANIC", AstrsStatus::Panic as i32),
    ];
    assert_eq!(
        expected.len(),
        header_values.len(),
        "expected exactly the AstrsStatus variants above, found {header_values:?}"
    );
    for (name, value) in expected {
        assert_eq!(
            header_values.get(*name).copied(),
            Some(i64::from(*value)),
            "ASTRS_STATUS mismatch for {name}"
        );
    }
}

#[test]
fn astrs_event_type_values_match_the_rust_discriminants() {
    let header_values: std::collections::BTreeMap<String, i64> =
        parse_named_values(HEADER).into_iter().collect();

    let expected: &[(&str, i32)] = &[
        ("ASTRS_EVENT_INPUT", AstrsEventType::Input as i32),
        (
            "ASTRS_EVENT_INPUT_CLOSED",
            AstrsEventType::InputClosed as i32,
        ),
        (
            "ASTRS_EVENT_INPUT_RECOVERED",
            AstrsEventType::InputRecovered as i32,
        ),
        ("ASTRS_EVENT_STOP", AstrsEventType::Stop as i32),
        ("ASTRS_EVENT_RELOAD", AstrsEventType::Reload as i32),
        (
            "ASTRS_EVENT_ALL_INPUTS_CLOSED",
            AstrsEventType::AllInputsClosed as i32,
        ),
        (
            "ASTRS_EVENT_PARAM_UPDATE",
            AstrsEventType::ParamUpdate as i32,
        ),
        (
            "ASTRS_EVENT_PARAM_DELETED",
            AstrsEventType::ParamDeleted as i32,
        ),
        ("ASTRS_EVENT_NODE_FAILED", AstrsEventType::NodeFailed as i32),
        ("ASTRS_EVENT_RESTARTED", AstrsEventType::Restarted as i32),
        ("ASTRS_EVENT_EXT_DROPPED", AstrsEventType::ExtDropped as i32),
        ("ASTRS_EVENT_ERROR", AstrsEventType::Error as i32),
        ("ASTRS_EVENT_UNKNOWN", AstrsEventType::Unknown as i32),
    ];
    for (name, value) in expected {
        assert_eq!(
            header_values.get(*name).copied(),
            Some(i64::from(*value)),
            "AstrsEventType mismatch for {name}"
        );
    }
    // Every `ASTRS_EVENT_*` entry the header defines is one of the above —
    // exact-set, not just superset, so a header-only addition is caught too.
    let event_names: BTreeSet<&str> = header_values
        .keys()
        .map(String::as_str)
        .filter(|name| name.starts_with("ASTRS_EVENT_"))
        .collect();
    let expected_names: BTreeSet<&str> = expected.iter().map(|(name, _)| *name).collect();
    assert_eq!(event_names, expected_names);
}

#[test]
fn astrs_timeout_infinite_matches_the_rust_constant() {
    let define_line = HEADER
        .lines()
        .find(|line| {
            line.trim_start()
                .starts_with("#define ASTRS_TIMEOUT_INFINITE")
        })
        .expect("the header must #define ASTRS_TIMEOUT_INFINITE");
    let value_token = define_line
        .split_whitespace()
        .nth(2)
        .expect("#define NAME VALUE");
    let digits = value_token.trim_end_matches(['u', 'U']);
    let parsed = u32::from_str_radix(digits.trim_start_matches("0x"), 16).unwrap();
    assert_eq!(parsed, astrs_capi::ASTRS_TIMEOUT_INFINITE);
    assert_eq!(astrs_capi::ASTRS_TIMEOUT_INFINITE, u32::MAX);
}
