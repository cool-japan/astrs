//! The switchover gate: every YAML file committed to this workspace, read
//! by both parsers and compared four ways.
//!
//! The oracle table (`tests/oracle.rs`) proves the *rules* agree; this proves
//! the rules add up to the same answer on the documents that actually exist —
//! manifest fixtures, example dataflows, `astrs migrate` golden outputs,
//! conformance scenarios. Each file is checked for:
//!
//! 1. **Value equality** — `astrs_yaml::parse_str` and
//!    `serde_yaml::from_str` build the same tree.
//! 2. **Typed equality** — the manifest-shaped files deserialize into the
//!    same [`astrs_manifest::Manifest`] through both parsers, which exercises
//!    `deny_unknown_fields`, `#[serde(untagged)]`, `Option`, `default`, and
//!    every custom `Deserialize` in that crate.
//! 3. **Byte-identical emission** — `astrs_yaml::to_string` and
//!    `serde_yaml::to_string` produce the same text, so the generated
//!    fixtures committed here (`expand/expected.yaml`,
//!    `*.expected.yaml`) do not change when the parser does.
//! 4. **Self round-trip** — re-reading this crate's own output reproduces the
//!    value it came from.
//!
//! `astrs-manifest` is a dev-dependency only, so nothing here creates a
//! dependency edge: this crate stays at layer 1 and the manifest crate keeps
//! its own parser until an integrator switches it deliberately.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::path::Path;

use support::{render_mine, render_theirs, repo_yaml_files};

/// How many YAML files this workspace is expected to hold, near enough that
/// a glob that silently matched nothing fails the test instead of passing
/// vacuously.
const MINIMUM_CORPUS: usize = 60;

#[test]
fn the_corpus_is_actually_found() {
    let files = repo_yaml_files();
    assert!(
        files.len() >= MINIMUM_CORPUS,
        "expected at least {MINIMUM_CORPUS} YAML files under {}, found {}",
        support::repo_root().display(),
        files.len()
    );
}

#[test]
fn every_repository_document_parses_to_the_same_value() {
    let mut failures = Vec::new();
    let mut compared = 0;
    for path in repo_yaml_files() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mine = astrs_yaml::parse_str(&text);
        let theirs = serde_yaml::from_str::<serde_yaml::Value>(&text);
        match (&mine, &theirs) {
            (Ok(left), Ok(right)) => {
                compared += 1;
                let (left, right) = (render_mine(left), render_theirs(right));
                if left != right {
                    failures.push(format!(
                        "{}\n    astrs-yaml: {left}\n    serde_yaml: {right}",
                        display(&path)
                    ));
                }
            }
            (Err(error), Ok(_)) => {
                failures.push(format!(
                    "{}: astrs-yaml refused it: {error}",
                    display(&path)
                ));
            }
            (Ok(_), Err(error)) => {
                failures.push(format!(
                    "{}: serde_yaml refused it: {error}",
                    display(&path)
                ));
            }
            (Err(_), Err(_)) => {}
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n  "));
    assert!(compared >= MINIMUM_CORPUS, "only {compared} files compared");
}

#[test]
fn every_repository_document_emits_byte_identically() {
    let mut failures = Vec::new();
    for path in repo_yaml_files() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (Ok(mine), Ok(theirs)) = (
            astrs_yaml::parse_str(&text),
            serde_yaml::from_str::<serde_yaml::Value>(&text),
        ) else {
            continue;
        };
        let Ok(theirs_out) = serde_yaml::to_string(&theirs) else {
            continue;
        };
        let mine_out = astrs_yaml::to_string(&mine).expect("astrs-yaml emits");
        if mine_out != theirs_out {
            failures.push(format!(
                "{}\n--- astrs-yaml ---\n{mine_out}--- serde_yaml ---\n{theirs_out}",
                display(&path)
            ));
        }
        let reread = astrs_yaml::parse_str(&mine_out).expect("astrs-yaml re-reads its own output");
        assert_eq!(reread, mine, "round trip changed {}", display(&path));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn every_manifest_deserializes_identically_through_both_parsers() {
    let mut failures = Vec::new();
    let mut compared = 0;
    for path in repo_yaml_files() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mine = astrs_yaml::from_str::<astrs_manifest::Manifest>(&text);
        let theirs = serde_yaml::from_str::<astrs_manifest::Manifest>(&text);
        match (&mine, &theirs) {
            (Ok(left), Ok(right)) => {
                compared += 1;
                if left != right {
                    failures.push(format!("{}: manifests differ", display(&path)));
                }
                let mine_out = astrs_yaml::to_string(left).expect("astrs-yaml emits");
                let theirs_out = serde_yaml::to_string(right).expect("serde_yaml emits");
                if mine_out != theirs_out {
                    failures.push(format!(
                        "{}\n--- astrs-yaml ---\n{mine_out}--- serde_yaml ---\n{theirs_out}",
                        display(&path)
                    ));
                }
                // The emitted manifest must read back into the same value —
                // this is the property `astrs expand`'s golden files rely on.
                let reread: astrs_manifest::Manifest =
                    astrs_yaml::from_str(&mine_out).expect("re-read");
                assert_eq!(
                    &reread,
                    left,
                    "manifest round trip changed {}",
                    display(&path)
                );
            }
            (Err(_), Err(_)) => {}
            (Err(error), Ok(_)) => {
                failures.push(format!(
                    "{}: astrs-yaml refused it: {error}",
                    display(&path)
                ));
            }
            (Ok(_), Err(error)) => {
                failures.push(format!(
                    "{}: serde_yaml refused it: {error}",
                    display(&path)
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n  "));
    assert!(
        compared >= 30,
        "only {compared} manifest-shaped files were compared"
    );
}

/// The four *generated* golden files in this workspace, called out by name.
///
/// These are the ones a switchover would break loudly: they were written by
/// `serde_yaml`'s emitter and are compared byte-for-byte by other crates'
/// tests. Naming them here means a future emitter change fails in this
/// crate, next to the code that caused it, rather than three crates away.
#[test]
fn the_generated_golden_files_re_emit_unchanged() {
    let goldens = [
        "bins/astrs-cli/tests/fixtures/expand/expected.yaml",
        "crates/astrs-manifest/tests/fixtures/modules/two_level/expected.yaml",
        "crates/astrs-migrate/tests/fixtures/dora/timer_restart_service.expected.yaml",
        "crates/astrs-migrate/tests/fixtures/ros2/basic.expected.yaml",
    ];
    for golden in goldens {
        let path = support::repo_root().join(golden);
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{golden}: {error}"));
        let value =
            astrs_yaml::parse_str(&text).unwrap_or_else(|error| panic!("{golden}: {error}"));
        let emitted = astrs_yaml::to_string(&value).expect("emit");
        // The migrate goldens carry a hand-written comment banner the
        // emitter cannot reproduce, so compare against `serde_yaml`'s own
        // re-emission rather than against the file's raw bytes.
        let theirs: serde_yaml::Value = serde_yaml::from_str(&text).expect("serde_yaml parses");
        assert_eq!(
            emitted,
            serde_yaml::to_string(&theirs).expect("serde_yaml emits"),
            "{golden}"
        );
    }
}

fn display(path: &Path) -> String {
    path.strip_prefix(support::repo_root())
        .unwrap_or(path)
        .display()
        .to_string()
}
