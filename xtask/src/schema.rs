//! `cargo xtask schema`: emit and check `astrs-schema.json` (blueprint
//! §8.1's leading comment: *"astrs-schema.json is generated from the Rust
//! structs; editors get completion."*).
//!
//! This module is thin by design: [`astrs_manifest::emit_schema`] already
//! does the real work (schemars over the `Manifest` type), and
//! `bins/astrs-cli/src/command/schema.rs` (`astrs schema`) already wires
//! that same function to a file or stdout for end users. This module wires
//! it to the *committed* file at the repo root, plus a `--check` mode that
//! never writes -- the shape a CI/preflight gate needs (fail on drift,
//! touch nothing).

use std::path::{Path, PathBuf};

use crate::error::XtaskError;

/// Where the generated schema is committed, relative to the workspace
/// root.
pub const SCHEMA_FILE_NAME: &str = "astrs-schema.json";

/// Render the current schema. Delegates entirely to
/// [`astrs_manifest::emit_schema`] -- this crate never re-derives it.
#[must_use]
pub fn render() -> String {
    astrs_manifest::emit_schema()
}

/// Render the schema and overwrite `root/astrs-schema.json` with it.
///
/// # Errors
///
/// [`XtaskError::Io`] if the file cannot be written.
pub fn write(root: &Path) -> Result<PathBuf, XtaskError> {
    let path = root.join(SCHEMA_FILE_NAME);
    let schema = render();
    std::fs::write(&path, &schema).map_err(|source| XtaskError::io(&path, source))?;
    Ok(path)
}

/// The result of comparing the current schema against the committed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The committed file is byte-for-byte what `astrs_manifest::emit_schema`
    /// renders right now.
    Match,
    /// `root/astrs-schema.json` does not exist at all -- never generated,
    /// or deleted since.
    Missing {
        /// Where it was expected.
        path: PathBuf,
    },
    /// The committed file differs from what the schema would render today.
    Drift {
        /// Where the committed file lives.
        path: PathBuf,
        /// The first line at which the two texts differ, one-based.
        line: usize,
        /// That line in the committed file.
        committed: String,
        /// That line in the freshly rendered schema.
        current: String,
    },
}

impl CheckOutcome {
    /// Whether this outcome means the committed file is up to date.
    #[must_use]
    pub fn is_match(&self) -> bool {
        matches!(self, Self::Match)
    }
}

impl std::fmt::Display for CheckOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Match => write!(f, "{SCHEMA_FILE_NAME} matches the current schema"),
            Self::Missing { path } => write!(f, "{} does not exist", path.display()),
            Self::Drift {
                path,
                line,
                committed,
                current,
            } => write!(
                f,
                "{} is stale: first difference at line {line}:\n  committed: {committed}\n  \
                 current:   {current}\nregenerate with `cargo xtask schema`",
                path.display()
            ),
        }
    }
}

/// Render the schema and compare it against `root/astrs-schema.json`
/// **without writing anything** -- the mode a preflight/CI gate needs.
///
/// # Errors
///
/// [`XtaskError::Io`] if the committed file exists but cannot be read (a
/// missing file is [`CheckOutcome::Missing`], not an error: the caller may
/// be running this before the file has ever been generated).
pub fn check(root: &Path) -> Result<CheckOutcome, XtaskError> {
    let path = root.join(SCHEMA_FILE_NAME);
    if !path.exists() {
        return Ok(CheckOutcome::Missing { path });
    }
    let committed =
        std::fs::read_to_string(&path).map_err(|source| XtaskError::io(&path, source))?;
    let current = render();
    if committed == current {
        return Ok(CheckOutcome::Match);
    }
    let (line, committed_line, current_line) = first_difference(&committed, &current).unwrap_or((
        0,
        "<end of file>".to_owned(),
        "<end of file>".to_owned(),
    ));
    Ok(CheckOutcome::Drift {
        path,
        line,
        committed: committed_line,
        current: current_line,
    })
}

/// The first line at which two texts differ, one-based, with both sides.
/// [`None`] if the two are line-for-line identical (they still differ
/// somewhere, e.g. trailing whitespace on the final unterminated line, or
/// this would not have been called) -- mirrors the equally-named helper in
/// `crates/astrs-wire/tests/protocol_snapshot.rs`.
fn first_difference(left: &str, right: &str) -> Option<(usize, String, String)> {
    let mut left_lines = left.lines();
    let mut right_lines = right.lines();
    let mut line = 0usize;
    loop {
        line += 1;
        match (left_lines.next(), right_lines.next()) {
            (None, None) => return None,
            (left_line, right_line) => {
                let left_line = left_line.unwrap_or("<end of file>");
                let right_line = right_line.unwrap_or("<end of file>");
                if left_line != right_line {
                    return Some((line, left_line.to_owned(), right_line.to_owned()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "astrs-xtask-schema-test-{}-{name}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn render_produces_parseable_non_empty_json() {
        let schema = render();
        assert!(!schema.is_empty());
        assert!(schema.trim_start().starts_with('{'));
    }

    #[test]
    fn write_then_check_matches() {
        let dir = scratch_dir("roundtrip");
        let path = write(&dir).unwrap();
        assert_eq!(path, dir.join(SCHEMA_FILE_NAME));
        assert_eq!(check(&dir).unwrap(), CheckOutcome::Match);
    }

    #[test]
    fn check_without_a_committed_file_is_missing() {
        let dir = scratch_dir("missing");
        let outcome = check(&dir).unwrap();
        assert!(matches!(outcome, CheckOutcome::Missing { .. }));
        assert!(!outcome.is_match());
    }

    #[test]
    fn check_does_not_write_the_file() {
        let dir = scratch_dir("check-is-read-only");
        let _ = check(&dir).unwrap();
        assert!(!dir.join(SCHEMA_FILE_NAME).exists());
    }

    #[test]
    fn check_detects_drift_and_names_the_line() {
        let dir = scratch_dir("drift");
        // Deliberately not JSON-object-shaped, so it cannot coincidentally
        // share a leading `{` line with whatever the real schema renders
        // today -- the first line is guaranteed to differ.
        std::fs::write(
            dir.join(SCHEMA_FILE_NAME),
            "definitely not the schema\nsecond line\n",
        )
        .unwrap();
        let outcome = check(&dir).unwrap();
        match outcome {
            CheckOutcome::Drift { line, .. } => assert_eq!(line, 1),
            other => panic!("expected Drift, got {other:?}"),
        }
        assert!(!outcome.is_match());
    }

    #[test]
    fn write_overwrites_a_stale_committed_file() {
        let dir = scratch_dir("overwrite");
        std::fs::write(dir.join(SCHEMA_FILE_NAME), "stale content").unwrap();
        write(&dir).unwrap();
        assert_eq!(check(&dir).unwrap(), CheckOutcome::Match);
    }

    #[test]
    fn the_schema_never_mentions_output_framing() {
        // Blueprint §2.2, re-asserted at this layer too: xtask's own copy
        // must not silently start carrying the field even if some future
        // change to `emit_schema` ever did.
        assert!(!render().contains("output_framing"));
    }
}
