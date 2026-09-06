//! The estate rules M2 added, and the one that cost an hour to learn.
//!
//! `m1_example_estate.rs` guards each example against itself: its manifest
//! against its package, its README against its command line. This file guards
//! the examples against **each other**, and guards the manifests the M1 file
//! never looks at.
//!
//! | Test | The drift it catches |
//! |---|---|
//! | `no_two_workspace_packages_build_the_same_binary_name` | two packages writing the same `target/<profile>/<name>`, silently overwriting each other |
//! | `every_committed_manifest_parses_validates_and_is_relocatable` | a second manifest (`replay.yml`, `budget-exhausted.yml`) rotting unnoticed |
//! | `every_committed_manifest_path_names_a_binary_the_workspace_builds` | any manifest naming a binary nothing produces |
//!
//! # Why the binary-name rule needs a test
//!
//! Cargo puts *every* workspace member's binaries in one
//! `target/<profile>/` directory. Two packages that both declare `[[bin]]
//! name = "detector-sim"` therefore produce one file, and which package wrote
//! it depends on which was built last. Nothing warns: `cargo build` succeeds,
//! the manifest spawns the binary it named, and the process that starts is
//! some *other* example's node — which then fails to decode the payloads it is
//! given, a hundred lines away from the cause.
//!
//! That happened while `record-replay` was being written (its `detector-sim`
//! collided with `rust-pipeline`'s), and it presented as a conformance suite
//! that alternated between two unrelated failures depending on the order the
//! packages had last been compiled in. One assertion makes it impossible.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use astrs_conformance::{EXAMPLES, example_dir, workspace_root};
use astrs_manifest::Manifest;

/// Reads a committed file, naming it if that fails.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Every `[[bin]] name` a cargo manifest declares, defaulting to the package
/// name when it declares no `[[bin]]` table (cargo's own `src/main.rs` rule).
///
/// A hand scan rather than a TOML parse: there is no TOML parser in the
/// workspace's closed dependency list (§18.1) and this suite is not a reason
/// to open it. `m1_example_estate.rs`'s own scanner has the same shape and its
/// own tests; this one is smaller because it needs two fields, not five.
fn binary_names(text: &str) -> Vec<String> {
    let mut package = String::new();
    let mut binaries = Vec::new();
    let mut section = String::new();
    let mut in_bin = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            section = line.to_owned();
            in_bin = line == "[[bin]]";
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), unquote(value));
        if section == "[package]" && key == "name" {
            package = value;
        } else if in_bin && key == "name" {
            binaries.push(value);
        }
    }
    if binaries.is_empty() && !package.is_empty() {
        binaries.push(package);
    }
    binaries
}

/// Strips one layer of quotes from a TOML scalar.
fn unquote(value: &str) -> String {
    value
        .trim()
        .trim_start_matches(['"', '\''])
        .trim_end_matches(['"', '\''])
        .to_owned()
}

/// Every directory holding a workspace package that produces binaries.
fn binary_producing_packages() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut dirs = Vec::new();
    for parent in ["examples", "bins"] {
        let base = root.join(parent);
        let Ok(entries) = std::fs::read_dir(&base) else {
            continue;
        };
        for entry in entries {
            let path = entry.expect("a readable directory entry").path();
            if path.join("Cargo.toml").is_file() {
                dirs.push(path);
            }
        }
    }
    assert!(
        !dirs.is_empty(),
        "no packages found under examples/ or bins/"
    );
    dirs
}

/// Every committed manifest of every example — `dataflow.yml` and any
/// companion beside it.
fn committed_manifests() -> Vec<PathBuf> {
    let mut found = Vec::new();
    for example in EXAMPLES {
        let dir = example_dir(example);
        let entries = std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("a readable directory entry").path();
            if path
                .extension()
                .is_some_and(|ext| ext == "yml" || ext == "yaml")
            {
                found.push(path);
            }
        }
    }
    assert!(found.len() >= EXAMPLES.len(), "{found:?}");
    found
}

/// No two workspace packages may build a binary of the same name.
///
/// See this file's module docs for the failure it prevents; it is not
/// hypothetical.
#[test]
fn no_two_workspace_packages_build_the_same_binary_name() {
    let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for dir in binary_producing_packages() {
        let package = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        for binary in binary_names(&read(&dir.join("Cargo.toml"))) {
            owners.entry(binary).or_default().push(package.clone());
        }
    }

    let collisions: Vec<(&String, &Vec<String>)> = owners
        .iter()
        .filter(|(_, packages)| packages.len() > 1)
        .collect();
    assert!(
        collisions.is_empty(),
        "these binary names are built by more than one package, so they \
         overwrite each other in target/<profile>/: {collisions:?}"
    );
}

/// Every committed manifest — not only the `dataflow.yml` the M1 estate reads
/// — parses, validates, and carries no absolute path.
#[test]
fn every_committed_manifest_parses_validates_and_is_relocatable() {
    for path in committed_manifests() {
        let manifest = Manifest::from_yaml_file(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        manifest
            .validate()
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));

        for node in &manifest.nodes {
            let Some(node_path) = node.path.as_deref() else {
                continue;
            };
            assert!(
                !Path::new(node_path).is_absolute(),
                "{}: node `{}` declares the absolute path {node_path} — an \
                 example that only runs on the machine that wrote it is not an \
                 example",
                path.display(),
                node.id
            );
            assert!(
                node_path.starts_with("../../target/debug/"),
                "{}: node `{}` declares {node_path}, not a repository-relative \
                 path under target/debug",
                path.display(),
                node.id
            );
        }
    }
}

/// Every `path:` in every committed manifest names a binary some workspace
/// package actually builds.
///
/// Broader than `m1_example_estate.rs`'s own version of this, which checks a
/// `dataflow.yml` against *its own* package: a companion manifest may
/// legitimately name another package's binary, and this catches the one thing
/// that is never legitimate — naming a binary nothing produces.
#[test]
fn every_committed_manifest_path_names_a_binary_the_workspace_builds() {
    let built: BTreeSet<String> = binary_producing_packages()
        .into_iter()
        .flat_map(|dir| binary_names(&read(&dir.join("Cargo.toml"))))
        .collect();

    for path in committed_manifests() {
        let manifest = Manifest::from_yaml_file(&path).expect("parses");
        for node in &manifest.nodes {
            let Some(node_path) = node.path.as_deref() else {
                continue;
            };
            let name = Path::new(node_path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            assert!(
                built.contains(&name),
                "{}: node `{}` runs `{name}`, which no workspace package builds",
                path.display(),
                node.id
            );
        }
    }
}

/// The scanner's own tests: a drift guard is only as good as the thing that
/// reads the files.
mod scanner_tests {
    use super::*;

    #[test]
    fn explicit_bin_tables_are_read_in_order() {
        let text = "\
[package]
name = \"demo\"

[[bin]]
name = \"demo-a\"
path = \"src/bin/a.rs\"

[[bin]]
name = \"demo-b\"
path = \"src/bin/b.rs\"
";
        assert_eq!(binary_names(text), vec!["demo-a", "demo-b"]);
    }

    #[test]
    fn a_package_without_bin_tables_builds_its_own_name() {
        assert_eq!(
            binary_names("[package]\nname = \"hello-timer\"\n"),
            vec!["hello-timer"]
        );
    }

    #[test]
    fn a_lib_name_is_not_a_binary_name() {
        let text = "\
[package]
name = \"demo\"

[lib]
name = \"demo_lib\"

[[bin]]
name = \"demo-bin\"
";
        assert_eq!(binary_names(text), vec!["demo-bin"]);
    }

    #[test]
    fn comments_are_not_facts() {
        let text = "# [[bin]]\n# name = \"ghost\"\n[package]\nname = \"real\"\n";
        assert_eq!(binary_names(text), vec!["real"]);
    }
}
