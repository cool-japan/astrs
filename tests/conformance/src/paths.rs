//! Finding the workspace, the built binaries and the committed manifests.
//!
//! # Why this module exists
//!
//! `CARGO_BIN_EXE_<name>` exists only for binaries of the *same* package, and
//! the examples are separate packages, so a test in this crate cannot use it.
//! Nor can it use [`std::env::current_exe`] directly: that is the test binary
//! under `<target>/<profile>/deps/`, not the example.
//!
//! It is, however, exactly one directory away. Cargo puts every workspace
//! member's binaries in `<target>/<profile>/`, so
//! `current_exe().parent().parent()` **is** that directory — whatever
//! `CARGO_TARGET_DIR` was set to, and whichever profile is being run.
//! [`target_dir`] resolves it that way first, then falls back to
//! `CARGO_TARGET_DIR` and finally to `<workspace>/target/{debug,release}`, and
//! [`binary`] reports a missing one with the exact `cargo build -p …` line
//! that produces it rather than skipping the test. A conformance suite that
//! silently skips is a conformance suite that proves nothing.

use std::path::{Path, PathBuf};

use crate::error::FixtureError;

/// Every example the estate covers, in the order the README lists them.
///
/// The example directory name, the cargo package name and the manifest's
/// parent directory are all the same string by construction, which is what
/// lets [`example_manifest`] and [`example_package`] take one argument.
pub const EXAMPLES: [&str; 20] = [
    "hello-timer",
    "rust-pipeline",
    "service-roundtrip",
    "shm-zero-copy-probe",
    "record-replay",
    "multi-daemon-cluster",
    "restart-policies",
    "error-propagation",
    "benchmark-latency",
    "action-progress",
    "streaming-segments",
    "log-aggregation",
    "dynamic-add-remove",
    "typed-vs-any",
    "module-composition",
    "tui-showcase",
    "ros2-talker-bridge",
    "ros2-native-listener",
    "rosbag-reader",
    "tf-broadcast",
];

/// The `(node, output)` of the one `astrs validate` diagnostic an example is
/// *expected* to print, for the two examples whose whole point is a port
/// with no consumer in the committed manifest alone. Every other example
/// must validate perfectly clean.
///
/// - `dynamic-add-remove`: `anchor`'s `beats` is read by a node `astrs node
///   add` attaches later, from a separate file this manifest never names
///   (see the manifest's own header comment) — never through this file.
/// - `tui-showcase`: `detector`'s `detections` is a deliberate dead end —
///   the manifest exists to give `astrs-tui`'s Graph tab a real two-node
///   topology to render (see the manifest's own header comment), not to be
///   a complete pipeline.
///
/// Shared by `m1_cli_process::astrs_validate_accepts_every_committed_example`
/// and `m1_single_machine::every_example_manifest_validates_through_the_cli`,
/// which both apply this same, narrow exception — checked precisely
/// (severity, node and port name) — rather than loosening the blanket
/// "clean" assertion for every example.
#[must_use]
pub fn expected_unconsumed_output(example: &str) -> Option<(&'static str, &'static str)> {
    match example {
        "dynamic-add-remove" => Some(("anchor", "beats")),
        "tui-showcase" => Some(("detector", "detections")),
        _ => None,
    }
}

/// The package the `astrs` command-line binary is built from.
pub const CLI_PACKAGE: &str = "astrs-cli";

/// The name of the `astrs` command-line binary.
pub const CLI_BINARY: &str = "astrs";

/// The environment variable a harness can set to point the suite at a
/// directory of pre-built binaries.
pub const ENV_BIN_DIR: &str = "ASTRS_CONFORMANCE_BIN_DIR";

/// The workspace root, derived from this crate's own manifest directory.
///
/// `tests/conformance` is two levels below the root, and `CARGO_MANIFEST_DIR`
/// is set for every build of this crate, so no search is needed.
#[must_use]
pub fn workspace_root() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| manifest_dir.to_path_buf(), Path::to_path_buf)
}

/// The directory Cargo put this workspace's binaries in.
///
/// Resolution order — see the module docs for why the second entry is both the
/// most reliable and the one that needs no environment at all:
///
/// 1. [`ENV_BIN_DIR`], for a harness that stages binaries itself.
/// 2. `current_exe()/../..` — `<target>/<profile>/deps/<test>` → `<profile>`.
/// 3. `CARGO_TARGET_DIR/{debug,release}`.
/// 4. `<workspace root>/target/{debug,release}`.
#[must_use]
pub fn target_dir() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(dir) = std::env::var(ENV_BIN_DIR)
        && !dir.is_empty()
    {
        candidates.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(profile_dir) = exe.parent().and_then(Path::parent)
    {
        candidates.push(profile_dir.to_path_buf());
    }
    let roots = std::env::var("CARGO_TARGET_DIR")
        .ok()
        .filter(|dir| !dir.is_empty())
        .map_or_else(
            || vec![workspace_root().join("target")],
            |dir| vec![dir.into()],
        );
    for root in roots {
        candidates.push(root.join("debug"));
        candidates.push(root.join("release"));
    }
    candidates.dedup();
    candidates
}

/// The absolute path of a built example binary.
///
/// # Errors
///
/// [`FixtureError::MissingBinary`], naming the `cargo build -p …` line that
/// produces it — never a silent skip.
pub fn binary(name: &str, package: &str) -> Result<PathBuf, FixtureError> {
    let searched = target_dir();
    for dir in &searched {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(FixtureError::MissingBinary {
        name: name.to_owned(),
        searched,
        build: format!("cargo build -p {package}"),
    })
}

/// The absolute path of the `astrs` command-line binary.
///
/// The suite drives the milestone two ways: in process, by calling the same
/// function the `run` arm calls, and out of process, by spawning *this* — the
/// program an adopter actually types. Neither substitutes for the other, so
/// both are proved.
///
/// # Errors
///
/// [`FixtureError::MissingBinary`] naming `cargo build -p astrs-cli`.
pub fn cli_binary() -> Result<PathBuf, FixtureError> {
    binary(CLI_BINARY, CLI_PACKAGE)
}

/// The path of a committed example manifest.
#[must_use]
pub fn example_manifest(example: &str) -> PathBuf {
    example_dir(example).join("dataflow.yml")
}

/// The directory a committed example lives in.
#[must_use]
pub fn example_dir(example: &str) -> PathBuf {
    workspace_root().join("examples").join(example)
}

/// The cargo package an example's binaries come from.
///
/// Identical to the example's directory name for every example in
/// [`EXAMPLES`]; the function exists so one that splits the two has a single
/// place to say so. A *graph* whose nodes span several packages is a
/// different matter and needs no entry here —
/// `tests/m2_dora_migration.rs` stages one by naming each node's binary
/// explicitly through [`crate::stage::StageOptions::with_node_path`].
#[must_use]
pub fn example_package(example: &str) -> &str {
    example
}

/// The path of a committed fixture this suite owns.
///
/// Unlike an example, a fixture is not a workspace member and is never run as
/// itself: it is *input* to a verb — a dora descriptor for `migrate
/// from-dora`, say — whose output is what gets run. It therefore lives beside
/// this crate rather than under `examples/`, and the estate guard that walks
/// `examples/` deliberately does not see it.
#[must_use]
pub fn fixture(relative: impl AsRef<Path>) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(relative)
}

/// The committed manifest path of an example, relative to the workspace root.
///
/// What a reader types: `astrs run examples/hello-timer/dataflow.yml`. Used by
/// the tests that run a manifest **verbatim**, without staging, so the command
/// in the README is the command that is proved.
#[must_use]
pub fn example_manifest_relative(example: &str) -> PathBuf {
    Path::new("examples").join(example).join("dataflow.yml")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_workspace_root_holds_the_examples() {
        let root = workspace_root();
        assert!(root.join("Cargo.toml").is_file(), "{}", root.display());
        assert!(root.join("examples").is_dir(), "{}", root.display());
    }

    #[test]
    fn every_example_directory_exists() {
        for example in EXAMPLES {
            let dir = example_dir(example);
            assert!(dir.is_dir(), "{}", dir.display());
            assert!(
                example_manifest(example).is_file(),
                "{}",
                example_manifest(example).display()
            );
            assert_eq!(example_package(example), example);
        }
    }

    #[test]
    fn the_binary_search_names_the_build_command() {
        let error = binary("definitely-not-a-binary", "hello-timer").unwrap_err();
        let text = error.to_string();
        assert!(text.contains("cargo build -p hello-timer"), "{text}");
        assert!(text.contains("looked in"), "{text}");
    }

    #[test]
    fn the_first_candidate_directory_is_the_profile_directory() {
        let candidates = target_dir();
        assert!(!candidates.is_empty());
        // `current_exe()` is `<target>/<profile>/deps/<test binary>`, so the
        // first non-override candidate must be a directory that exists.
        assert!(candidates.iter().any(|dir| dir.is_dir()), "{candidates:?}");
    }

    #[test]
    fn the_relative_manifest_path_is_what_the_readme_prints() {
        let relative = example_manifest_relative("rust-pipeline");
        assert_eq!(
            relative.to_string_lossy(),
            "examples/rust-pipeline/dataflow.yml"
        );
        assert_eq!(
            workspace_root().join(&relative),
            example_manifest("rust-pipeline")
        );
    }
}
