//! Shared helpers for this crate's integration tests.
//!
//! The compatibility tests all need the same two things: a way to render a
//! value from *either* parser into one comparable text, and a way to find
//! every YAML file in the repository. Both live here so the oracle table and
//! the corpus sweep cannot drift apart in what they consider "equal".
//!
//! # Why render rather than convert
//!
//! Comparing an `astrs_yaml::Value` with a `serde_yaml::Value` by converting
//! one into the other would bury exactly the differences the test exists to
//! catch — a conversion has to decide what an integer that does not fit is,
//! and that decision is the thing under test. Rendering both sides into an
//! explicit, discriminated text (`PosInt(1)` is not `Float(1.0)` and neither
//! is `String("1")`) keeps every distinction visible, and makes a failure
//! readable without a debugger.

#![allow(dead_code)] // One binary per tests/*.rs file; not every helper is used by every one.

use std::path::{Path, PathBuf};

/// Render an `astrs_yaml::Value` into the comparable text form.
pub fn render_mine(value: &astrs_yaml::Value) -> String {
    use astrs_yaml::Value;
    match value {
        Value::Null => "Null".to_owned(),
        Value::Bool(inner) => format!("Bool({inner})"),
        Value::Number(number) => {
            if let Some(inner) = number.as_u64() {
                format!("PosInt({inner})")
            } else if let Some(inner) = number.as_i64() {
                format!("NegInt({inner})")
            } else {
                format!("Float({:?})", number.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(inner) => format!("String({inner:?})"),
        Value::Sequence(items) => format!(
            "Seq[{}]",
            items.iter().map(render_mine).collect::<Vec<_>>().join(", ")
        ),
        Value::Mapping(mapping) => format!(
            "Map{{{}}}",
            mapping
                .iter()
                .map(|(key, value)| format!("{} => {}", render_mine(key), render_mine(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Tagged(tagged) => {
            format!("Tagged({} {})", tagged.tag, render_mine(&tagged.value))
        }
    }
}

/// Render a `serde_yaml::Value` into the same comparable text form.
pub fn render_theirs(value: &serde_yaml::Value) -> String {
    use serde_yaml::Value;
    match value {
        Value::Null => "Null".to_owned(),
        Value::Bool(inner) => format!("Bool({inner})"),
        Value::Number(number) => {
            if let Some(inner) = number.as_u64() {
                format!("PosInt({inner})")
            } else if let Some(inner) = number.as_i64() {
                format!("NegInt({inner})")
            } else {
                format!("Float({:?})", number.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(inner) => format!("String({inner:?})"),
        Value::Sequence(items) => format!(
            "Seq[{}]",
            items
                .iter()
                .map(render_theirs)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Mapping(mapping) => format!(
            "Map{{{}}}",
            mapping
                .iter()
                .map(|(key, value)| format!("{} => {}", render_theirs(key), render_theirs(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Tagged(tagged) => {
            format!("Tagged({} {})", tagged.tag, render_theirs(&tagged.value))
        }
    }
}

/// Both parsers' answer for one input, rendered.
pub struct Outcomes {
    /// What `astrs-yaml` produced, or `ERR(...)`.
    pub mine: String,
    /// What `serde_yaml` produced, or `ERR(...)`.
    pub theirs: String,
    /// True when both succeeded with the same value, or both failed.
    pub agree: bool,
}

/// Run `input` through both parsers.
pub fn compare(input: &str) -> Outcomes {
    let mine = astrs_yaml::parse_str(input);
    let theirs = serde_yaml::from_str::<serde_yaml::Value>(input);
    let agree = match (&mine, &theirs) {
        (Ok(left), Ok(right)) => render_mine(left) == render_theirs(right),
        (Err(_), Err(_)) => true,
        _ => false,
    };
    Outcomes {
        mine: match &mine {
            Ok(value) => render_mine(value),
            Err(error) => format!("ERR({error})"),
        },
        theirs: match &theirs {
            Ok(value) => render_theirs(value),
            Err(error) => format!("ERR({error})"),
        },
        agree,
    }
}

/// The workspace root, found relative to this crate rather than to the
/// process's working directory (which `cargo` does not promise).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

/// Every `.yml`/`.yaml` file committed to the repository, sorted, excluding
/// build output.
pub fn repo_yaml_files() -> Vec<PathBuf> {
    let root = repo_root();
    let mut found = Vec::new();
    for pattern in ["**/*.yml", "**/*.yaml"] {
        let glob_pattern = format!("{}/{pattern}", root.display());
        let Ok(entries) = glob::glob(&glob_pattern) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.components().any(|part| part.as_os_str() == "target") {
                continue;
            }
            found.push(entry);
        }
    }
    found.sort();
    found.dedup();
    found
}
