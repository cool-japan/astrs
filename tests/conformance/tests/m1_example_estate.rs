//! The M1 example estate's own contract (blueprint §5.4, §18.1, §20).
//!
//! The other two files in this suite *run* the examples. This one checks the
//! committed files they run, because a graph that passes is not the same thing
//! as an estate a reader can copy from. Everything here is a fact that would
//! silently rot: a manifest naming a binary the package stopped building, a
//! `build:` line naming the wrong package, an example missing from the
//! workspace members or from the index README, a dependency pinned locally
//! instead of at the workspace.
//!
//! None of these need a process. They are fast, they run on every `cargo test
//! -p astrs-conformance`, and each one has failed for real at some point in a
//! repository's life.
//!
//! # Why the Cargo manifests are scanned by hand
//!
//! There is no TOML parser in the workspace's closed dependency list (§18.1)
//! and this suite is not a reason to open it. The scan below is deliberately
//! small: it reads `[package] name`, `[[bin]] name` and a handful of literal
//! lines, which is exactly what the assertions need and nothing more.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::Path;

use astrs_conformance::{
    EXAMPLES, committed_node_binaries, example_dir, example_manifest, workspace_root,
};
use astrs_manifest::Manifest;

/// Reads a committed file, naming it if that fails.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The `[package] name` and every `[[bin]] name` a cargo manifest declares.
///
/// A hand scan rather than a TOML parse — see this file's module docs.
#[derive(Debug, Default, PartialEq, Eq)]
struct CargoFacts {
    /// The package name.
    package: String,
    /// Every binary target name, in declaration order.
    binaries: Vec<String>,
    /// Whether the package declares `publish = false`.
    publish_false: bool,
    /// Whether the package inherits the workspace lint table.
    workspace_lints: bool,
    /// Every dependency line that is *not* `{ workspace = true }`.
    local_pins: Vec<String>,
}

impl CargoFacts {
    /// Scans a `Cargo.toml`.
    fn scan(text: &str) -> Self {
        let mut facts = Self::default();
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
            let key = key.trim();
            let value = value.trim();
            match (section.as_str(), key) {
                ("[package]", "name") => facts.package = unquote(value),
                ("[package]", "publish") => facts.publish_false = value == "false",
                ("[lints]", "workspace") => facts.workspace_lints = value == "true",
                ("[dependencies]" | "[dev-dependencies]" | "[build-dependencies]", _)
                    if !value.contains("workspace = true") =>
                {
                    facts.local_pins.push(line.to_owned());
                }
                _ if in_bin && key == "name" => facts.binaries.push(unquote(value)),
                _ => {}
            }
        }
        facts
    }

    /// The binaries this package produces, defaulting to the package name when
    /// no `[[bin]]` table is declared (cargo's own `src/main.rs` rule).
    fn binary_names(&self) -> BTreeSet<String> {
        if self.binaries.is_empty() {
            BTreeSet::from([self.package.clone()])
        } else {
            self.binaries.iter().cloned().collect()
        }
    }
}

/// Strips one layer of quotes from a TOML scalar.
fn unquote(value: &str) -> String {
    value
        .trim()
        .trim_start_matches(['"', '\''])
        .trim_end_matches(['"', '\''])
        .to_owned()
}

/// Every example directory that actually exists on disk.
fn example_directories() -> BTreeSet<String> {
    let root = workspace_root().join("examples");
    let mut found = BTreeSet::new();
    for entry in std::fs::read_dir(&root).unwrap_or_else(|error| panic!("{root:?}: {error}")) {
        let entry = entry.expect("a readable directory entry");
        if entry.path().is_dir() {
            found.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    found
}

/// The estate on disk is exactly the estate the suite knows about — a new
/// example directory that nobody wired into the tests is the commonest way an
/// example rots unnoticed.
#[test]
fn the_examples_on_disk_are_the_examples_the_suite_runs() {
    let on_disk = example_directories();
    let known: BTreeSet<String> = EXAMPLES.iter().map(|name| (*name).to_owned()).collect();
    assert_eq!(
        on_disk, known,
        "examples/ and astrs_conformance::EXAMPLES disagree"
    );
}

/// Every example is a workspace member, and so is this suite.
#[test]
fn every_example_is_a_workspace_member() {
    let root = read(&workspace_root().join("Cargo.toml"));
    for example in EXAMPLES {
        let member = format!("\"examples/{example}\"");
        assert!(
            root.contains(&member),
            "the root Cargo.toml does not list {member} in `members`"
        );
    }
    assert!(
        root.contains("\"tests/conformance\""),
        "the root Cargo.toml does not list the conformance suite in `members`"
    );
}

/// Each example carries the four files a reader expects to find beside it.
#[test]
fn every_example_ships_a_manifest_a_readme_and_a_cargo_package() {
    for example in EXAMPLES {
        let dir = example_dir(example);
        for file in ["Cargo.toml", "README.md", "dataflow.yml"] {
            let path = dir.join(file);
            assert!(path.is_file(), "{}", path.display());
        }
        assert!(dir.join("src").is_dir(), "{}/src", dir.display());
    }
}

/// Example packages follow the workspace's own policy: never published, all
/// metadata and lints inherited, no locally pinned dependency versions
/// (§18.1 — a version lives in the root manifest or nowhere).
#[test]
fn every_example_package_inherits_the_workspace_policy() {
    for example in EXAMPLES {
        let path = example_dir(example).join("Cargo.toml");
        let facts = CargoFacts::scan(&read(&path));
        assert_eq!(facts.package, example, "{}", path.display());
        assert!(
            facts.publish_false,
            "{} is missing `publish = false`",
            path.display()
        );
        assert!(
            facts.workspace_lints,
            "{} does not inherit `[lints] workspace = true`",
            path.display()
        );
        assert!(
            facts.local_pins.is_empty(),
            "{} pins dependencies locally: {:?}",
            path.display(),
            facts.local_pins
        );
    }
}

/// Every `path:` in a committed manifest names a binary its package actually
/// builds. This is the assertion that catches a renamed `[[bin]]`, which
/// otherwise surfaces as a spawn failure at the far end of a two-minute run.
#[test]
fn every_manifest_path_names_a_binary_the_package_builds() {
    for example in EXAMPLES {
        let facts = CargoFacts::scan(&read(&example_dir(example).join("Cargo.toml")));
        let declared = facts.binary_names();
        for binary in committed_node_binaries(example).expect("the manifest parses") {
            let name = Path::new(&binary.declared)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            assert!(
                declared.contains(&name),
                "{example}/dataflow.yml node `{}` runs `{name}`, but {example} builds {declared:?}",
                binary.node
            );
        }
    }
}

/// Every `build:` line names the package that produces the node's binary, so
/// `astrs run` without `--skip-build` really does build what it then spawns.
#[test]
fn every_build_line_names_the_owning_package() {
    for example in EXAMPLES {
        let path = example_manifest(example);
        let manifest = Manifest::from_yaml_file(&path).expect("the manifest parses");
        let expected = format!("cargo build -p {example}");
        for node in &manifest.nodes {
            let build = node
                .build
                .as_ref()
                .unwrap_or_else(|| panic!("{example}/{} declares no `build:`", node.id));
            assert_eq!(
                build.trim(),
                expected,
                "{example}/{} builds with the wrong package",
                node.id
            );
        }
    }
}

/// Every committed manifest declares the protocol version and a name that
/// matches its directory, which is what makes `astrs run examples/x/…` and
/// `astrs list` agree about what is running.
#[test]
fn every_manifest_declares_its_version_and_name() {
    for example in EXAMPLES {
        let path = example_manifest(example);
        let text = read(&path);
        assert!(
            text.contains("astrs: \"1\""),
            "{} does not declare `astrs: \"1\"`",
            path.display()
        );
        let manifest = Manifest::from_yaml_file(&path).expect("the manifest parses");
        assert_eq!(
            manifest.name.as_deref(),
            Some(example),
            "{} names a different dataflow",
            path.display()
        );
    }
}

/// No committed manifest carries an absolute path. An example that only runs
/// on the machine that wrote it is not an example.
#[test]
fn no_committed_manifest_carries_an_absolute_path() {
    for example in EXAMPLES {
        for binary in committed_node_binaries(example).expect("the manifest parses") {
            assert!(
                !Path::new(&binary.declared).is_absolute(),
                "{example}/{} declares the absolute path {}",
                binary.node,
                binary.declared
            );
        }
    }
}

/// The index README lists every example, and each example's own README shows
/// the command that runs it.
#[test]
fn the_readmes_cover_every_example() {
    let index = read(&workspace_root().join("examples").join("README.md"));
    for example in EXAMPLES {
        assert!(
            index.contains(example),
            "examples/README.md never mentions {example}"
        );
        let readme = read(&example_dir(example).join("README.md"));
        assert!(
            readme.contains(&format!("cargo build -p {example}")),
            "{example}/README.md does not show how to build it"
        );
        // Whitespace-tolerant: a README is free to align a column of commands
        // for readability, and a drift guard that forbade that would be
        // policing formatting rather than guarding drift.
        let wanted = format!("astrs run examples/{example}/dataflow.yml");
        assert!(
            readme
                .lines()
                .any(|line| collapse(line).starts_with(&wanted)),
            "{example}/README.md does not show `{wanted}`"
        );
    }
}

/// Collapses runs of whitespace in a line to single spaces, and trims it.
fn collapse(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every example that declares a node whose output is typed declares the type
/// on **both** ends, so `astrs validate` has something to check. A one-sided
/// type is not a type.
#[test]
fn typed_edges_are_declared_on_both_ends() {
    for example in EXAMPLES {
        let manifest = Manifest::from_yaml_file(example_manifest(example)).expect("parses");
        let mut produced = Vec::new();
        for node in &manifest.nodes {
            for (port, urn) in &node.output_types {
                produced.push((format!("{}/{port}", node.id), urn.clone()));
            }
        }
        for node in &manifest.nodes {
            for (port, urn) in &node.input_types {
                let Some(input) = node.inputs.get(port) else {
                    panic!("{example}: {}/{port} is typed but not wired", node.id);
                };
                let source = input.source.clone();
                // A virtual source (§8.4) has no producing node to type.
                if source.starts_with("astrs/") {
                    continue;
                }
                let Some((_, declared)) = produced.iter().find(|(from, _)| *from == source) else {
                    panic!(
                        "{example}: {}/{port} reads {source}, which declares no output type",
                        node.id
                    );
                };
                assert_eq!(
                    declared, urn,
                    "{example}: {}/{port} and {source} disagree",
                    node.id
                );
            }
        }
    }
}

/// The Cargo-manifest scanner's own tests.
///
/// A drift guard is only as good as the thing that reads the files, so the
/// reader is tested against inputs whose answers are obvious.
mod scanner_tests {
    use super::*;

    /// The scanner reads what the assertions rely on, and nothing it does not
    /// understand becomes a false positive.
    #[test]
    fn the_cargo_scanner_reads_the_fields_it_claims_to() {
        let text = "\
[package]
name = \"demo\"
publish = false
version.workspace = true

[[bin]]
name = \"demo-a\"
path = \"src/bin/a.rs\"

[[bin]]
name = \"demo-b\"
path = \"src/bin/b.rs\"

[dependencies]
astrs-node-api = { workspace = true }
serde = { workspace = true }

[lints]
workspace = true
";
        let facts = CargoFacts::scan(text);
        assert_eq!(facts.package, "demo");
        assert!(facts.publish_false);
        assert!(facts.workspace_lints);
        assert!(facts.local_pins.is_empty());
        assert_eq!(facts.binaries, vec!["demo-a", "demo-b"]);
        assert_eq!(
            facts.binary_names(),
            BTreeSet::from(["demo-a".to_owned(), "demo-b".to_owned()])
        );
    }

    /// A package with no `[[bin]]` table produces one binary named after
    /// itself — cargo's `src/main.rs` rule, which `hello-timer` relies on.
    #[test]
    fn a_package_without_bin_tables_builds_its_own_name() {
        let facts = CargoFacts::scan("[package]\nname = \"hello-timer\"\n");
        assert_eq!(
            facts.binary_names(),
            BTreeSet::from(["hello-timer".to_owned()])
        );
    }

    /// A locally pinned dependency is caught, which is the §18.1 rule the
    /// estate must not be the place that breaks.
    #[test]
    fn a_local_version_pin_is_reported() {
        let facts = CargoFacts::scan("[dependencies]\nserde = \"1.0\"\n");
        assert_eq!(facts.local_pins, vec!["serde = \"1.0\""]);
    }

    /// Comments and blank lines are ignored rather than mis-scanned.
    #[test]
    fn comments_are_not_facts() {
        let facts =
            CargoFacts::scan("# [package]\n# name = \"ghost\"\n\n[package]\nname = \"real\"\n");
        assert_eq!(facts.package, "real");
    }

    #[test]
    fn quotes_are_stripped_from_scalars() {
        assert_eq!(unquote("\"x\""), "x");
        assert_eq!(unquote("'x'"), "x");
        assert_eq!(unquote("x"), "x");
    }

    /// An aligned command column still reads as the command it is.
    #[test]
    fn whitespace_is_collapsed_before_matching() {
        assert_eq!(
            collapse("astrs run      examples/x/dataflow.yml"),
            "astrs run examples/x/dataflow.yml"
        );
        assert_eq!(collapse("  spaced  out  "), "spaced out");
        assert_eq!(collapse(""), "");
    }
}
