//! Turning a committed example manifest into one a test can run.
//!
//! # Why staging is needed at all
//!
//! The committed manifests carry repository-relative paths
//! (`../../target/debug/hello-timer`) because that is what a reader running
//! `astrs run examples/hello-timer/dataflow.yml` needs. A test that ran them
//! verbatim would depend on the profile and on `CARGO_TARGET_DIR` being unset.
//!
//! [`stage_manifest`] therefore *rewrites* them: it parses the committed
//! manifest with `astrs-manifest`, replaces each node's `path:` with the
//! absolute path [`crate::binary`] resolved for that file's own basename,
//! applies any environment overrides the test needs (a per-run temp file to
//! read results from), drops the `build:` lines, and writes the result into a
//! temporary directory. The staged copy is re-parsed and re-validated before
//! it is run, so a field lost in the round trip surfaces as a parse error here
//! rather than as a mysterious spawn failure later.
//!
//! ```text
//!   examples/x/dataflow.yml ──parse──► Manifest ──rewrite paths+env──► YAML
//!                                                                       │
//!   <target>/<profile>/x  ◄── binary("x") ◄── current_exe()/../..       │
//!                                                                       ▼
//!                                            temp_dir/x.yml ──► astrs run
//! ```
//!
//! Staging is *not* a substitute for running the committed file as written:
//! `tests/m1_cli_process.rs` does that too, unstaged, so the command in the
//! README stays true.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use astrs_manifest::{EnvValue, Manifest};

use crate::error::FixtureError;
use crate::paths::{binary, example_manifest, example_package};

/// A staged copy of a committed manifest, ready to run.
#[derive(Debug)]
pub struct Fixture {
    /// The directory holding the staged manifest; also the run's working
    /// directory and runtime directory.
    pub dir: PathBuf,
    /// The staged manifest.
    pub manifest: PathBuf,
}

impl Fixture {
    /// Removes the staging directory.
    ///
    /// Best effort: a leftover temporary directory is not worth failing a
    /// conformance run over.
    pub fn clean(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }

    /// The staged manifest as text, for a failure message.
    ///
    /// # Errors
    ///
    /// [`FixtureError::Io`] when the staged file cannot be read back.
    pub fn text(&self) -> Result<String, FixtureError> {
        std::fs::read_to_string(&self.manifest).map_err(|source| FixtureError::Io {
            what: format!("read {}", self.manifest.display()),
            source,
        })
    }
}

/// How a committed manifest should be rewritten before it is run.
///
/// Every knob exists because some test needs to change one fact about a graph
/// while keeping the rest of the committed file authoritative — which is the
/// difference between testing the example and testing a copy of it.
#[derive(Debug, Clone)]
pub struct StageOptions {
    /// The cargo package every node's binary comes from.
    package: String,
    /// Merged over each node's own `env:` block.
    env: BTreeMap<String, String>,
    /// Replaces a named node's `path:` outright, bypassing the binary search.
    ///
    /// Used to point a node at something that is deliberately not there, so
    /// the suite can prove `astrs run` *reports* a broken graph rather than
    /// only that it runs a working one.
    node_paths: BTreeMap<String, String>,
    /// Overrides a named node's shared-memory pool size, in bytes.
    shm_pool_sizes: BTreeMap<String, u64>,
    /// Keeps the manifest's `build:` lines instead of dropping them.
    keep_build: bool,
}

impl StageOptions {
    /// Options that resolve every `path:` inside `package` and drop `build:`.
    #[must_use]
    pub fn new(package: impl Into<String>) -> Self {
        Self {
            package: package.into(),
            env: BTreeMap::new(),
            node_paths: BTreeMap::new(),
            shm_pool_sizes: BTreeMap::new(),
            keep_build: false,
        }
    }

    /// Sets an environment variable on every node.
    #[must_use]
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Sets an environment variable naming a path on every node.
    #[must_use]
    pub fn with_env_path(self, key: impl Into<String>, value: &Path) -> Self {
        self.with_env(key, value.display().to_string())
    }

    /// Points one node at an explicit binary, whether or not it exists.
    #[must_use]
    pub fn with_node_path(mut self, node: impl Into<String>, path: impl Into<String>) -> Self {
        self.node_paths.insert(node.into(), path.into());
        self
    }

    /// Overrides one node's `shm_pool_size:`.
    #[must_use]
    pub fn with_shm_pool_size(mut self, node: impl Into<String>, bytes: u64) -> Self {
        self.shm_pool_sizes.insert(node.into(), bytes);
        self
    }

    /// Keeps the manifest's `build:` lines.
    ///
    /// Off by default: the suite runs what cargo already built and never
    /// shells out to a compiler from inside a test.
    #[must_use]
    pub const fn keeping_build(mut self, keep: bool) -> Self {
        self.keep_build = keep;
        self
    }
}

/// Stages a committed example manifest so it can be run from a temporary
/// directory against freshly built binaries.
///
/// `package` is the cargo package every node's binary comes from and `env` is
/// merged over each node's own `env:` block, so a test can point a node at a
/// per-run result file. Equivalent to [`stage_manifest_with`] over
/// [`StageOptions::new`].
///
/// # Errors
///
/// [`FixtureError`] when a binary is missing, the manifest does not parse,
/// re-parse or validate, or the staging directory cannot be written.
pub fn stage_manifest(
    source: &Path,
    package: &str,
    env: &BTreeMap<String, String>,
) -> Result<Fixture, FixtureError> {
    let mut options = StageOptions::new(package);
    for (key, value) in env {
        options = options.with_env(key.clone(), value.clone());
    }
    stage_manifest_with(source, &options)
}

/// Stages the committed manifest of `example`, whose package shares its name.
///
/// # Errors
///
/// As [`stage_manifest_with`].
pub fn stage_example(example: &str, options: &StageOptions) -> Result<Fixture, FixtureError> {
    stage_manifest_with(&example_manifest(example), options)
}

/// Stages the committed manifest of `example` with no override but the ones in
/// `env`.
///
/// # Errors
///
/// As [`stage_manifest_with`].
pub fn stage_example_env(
    example: &str,
    env: &BTreeMap<String, String>,
) -> Result<Fixture, FixtureError> {
    stage_manifest(&example_manifest(example), example_package(example), env)
}

/// Stages a committed manifest under explicit [`StageOptions`].
///
/// # Errors
///
/// [`FixtureError`] when a binary is missing, the manifest does not parse,
/// re-parse or validate, or the staging directory cannot be written.
pub fn stage_manifest_with(source: &Path, options: &StageOptions) -> Result<Fixture, FixtureError> {
    let mut manifest =
        Manifest::from_yaml_file(source).map_err(|error| FixtureError::Manifest {
            path: source.to_path_buf(),
            reason: error.to_string(),
        })?;

    for node in &mut manifest.nodes {
        if let Some(replacement) = options.node_paths.get(&node.id) {
            node.path = Some(replacement.clone());
        } else if let Some(path) = node.path.clone() {
            let name = Path::new(&path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or(path);
            let resolved = binary(&name, &options.package)?;
            node.path = Some(resolved.display().to_string());
        }
        if !options.keep_build {
            node.build = None;
        }
        if let Some(bytes) = options.shm_pool_sizes.get(&node.id) {
            node.shm_pool_size = Some(*bytes);
        }
        for (key, value) in &options.env {
            node.env
                .insert(key.clone(), EnvValue::String(value.clone()));
        }
    }

    let rendered = astrs_yaml::to_string(&manifest).map_err(|error| FixtureError::Manifest {
        path: source.to_path_buf(),
        reason: format!("the staged manifest could not be serialised: {error}"),
    })?;
    // Re-parse and re-validate before anything is spawned: a field lost in
    // the round trip is a bug in the staging, and it must not look like a bug
    // in the dataflow.
    let staged = Manifest::from_yaml_str(&rendered).map_err(|error| FixtureError::Manifest {
        path: source.to_path_buf(),
        reason: format!("the staged manifest does not re-parse: {error}"),
    })?;
    staged.validate().map_err(|error| FixtureError::Manifest {
        path: source.to_path_buf(),
        reason: format!("the staged manifest does not validate: {error}"),
    })?;

    let stem = source.file_stem().map_or_else(
        || "dataflow".to_owned(),
        |stem| stem.to_string_lossy().into_owned(),
    );
    let dir = staging_dir();
    std::fs::create_dir_all(&dir).map_err(|source| FixtureError::Io {
        what: format!("create {}", dir.display()),
        source,
    })?;
    let manifest_path = dir.join(format!("{stem}.yml"));
    std::fs::write(&manifest_path, &rendered).map_err(|source| FixtureError::Io {
        what: format!("write {}", manifest_path.display()),
        source,
    })?;

    Ok(Fixture {
        dir,
        manifest: manifest_path,
    })
}

/// One node's `path:` as the committed manifest writes it, and where that
/// resolves to on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedNodeBinary {
    /// The node's id in the manifest.
    pub node: String,
    /// The `path:` exactly as committed, e.g. `../../target/debug/camera-sim`.
    pub declared: String,
    /// `declared` resolved against the manifest's own directory, which is what
    /// `astrs run` does when `--working-dir` is not given.
    pub resolved: PathBuf,
}

/// Every `path:` in a committed example manifest, resolved the way `astrs run`
/// resolves it.
///
/// Used by the tests that run a manifest **verbatim** — those need to know
/// whether the file the committed path names is actually there before they
/// spawn anything, so a missing build is reported as a missing build rather
/// than as a dataflow failure.
///
/// # Errors
///
/// [`FixtureError::Manifest`] when the committed manifest does not parse.
pub fn committed_node_binaries(example: &str) -> Result<Vec<CommittedNodeBinary>, FixtureError> {
    let source = example_manifest(example);
    let manifest = Manifest::from_yaml_file(&source).map_err(|error| FixtureError::Manifest {
        path: source.clone(),
        reason: error.to_string(),
    })?;
    let base = source.parent().map_or_else(PathBuf::new, Path::to_path_buf);
    Ok(manifest
        .nodes
        .iter()
        .filter_map(|node| {
            node.path.as_ref().map(|path| CommittedNodeBinary {
                node: node.id.clone(),
                declared: path.clone(),
                resolved: normalise(&base.join(path)),
            })
        })
        .collect())
}

/// Collapses `.` and `..` in a path lexically.
///
/// Not [`std::fs::canonicalize`]: that requires the file to exist, and the
/// whole point of resolving a committed path is to find out whether it does.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// A fresh scratch directory under the platform temporary directory.
///
/// Deliberately terse, and this is not a style choice. A Unix socket path may
/// be **103 bytes** on Darwin, `std::env::temp_dir()` is already ~50 of them
/// under a sandboxed user, and the daemon puts its node socket *inside* this
/// directory — so a descriptive name here costs the run rather than costing
/// readability. Anything that needs a directory the daemon will live in should
/// use this rather than inventing a name.
#[must_use]
pub fn scratch_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "astrs-cf-{}-{}",
        std::process::id(),
        FIXTURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

/// A fresh scratch directory, created.
///
/// # Errors
///
/// [`FixtureError::Io`] when the directory cannot be created.
pub fn make_scratch_dir() -> Result<PathBuf, FixtureError> {
    let dir = scratch_dir();
    std::fs::create_dir_all(&dir).map_err(|source| FixtureError::Io {
        what: format!("create {}", dir.display()),
        source,
    })?;
    Ok(dir)
}

/// A fresh staging directory: a [`scratch_dir`] by another name, so the
/// reasoning above has one home.
fn staging_dir() -> PathBuf {
    scratch_dir()
}

/// Distinguishes one staging directory from the next within a process.
static FIXTURES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::paths::EXAMPLES;

    #[test]
    fn every_committed_example_manifest_parses_and_validates() {
        for example in EXAMPLES {
            let path = example_manifest(example);
            let manifest = Manifest::from_yaml_file(&path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            manifest
                .validate()
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        }
    }

    /// Two stagings never share a directory, so two tests can run the same
    /// example concurrently.
    #[test]
    fn staging_directories_are_distinct() {
        let first = staging_dir();
        let second = staging_dir();
        assert_ne!(first, second);
        assert!(first.starts_with(std::env::temp_dir()));
    }

    /// A path override wins over the binary search, and does not need the
    /// binary to exist — that is what makes the failure-path test possible.
    #[test]
    fn a_path_override_bypasses_the_binary_search() {
        let options = StageOptions::new("hello-timer")
            .with_node_path("greeter", "/nonexistent/astrs-conformance-missing")
            .with_env("HELLO_TIMER_TICKS", "1");
        let fixture = stage_example("hello-timer", &options).expect("staged");
        let text = fixture.text().expect("readable");
        assert!(
            text.contains("/nonexistent/astrs-conformance-missing"),
            "{text}"
        );
        assert!(!text.contains("build:"), "{text}");
        assert!(text.contains("HELLO_TIMER_TICKS"), "{text}");
        fixture.clean();
    }

    /// The `shm_pool_size` override survives the YAML round trip, so a probe
    /// can be re-tuned without editing the committed manifest.
    #[test]
    fn a_pool_size_override_survives_the_round_trip() {
        let options = StageOptions::new("shm-zero-copy-probe")
            .with_node_path("producer", "/nonexistent/producer")
            .with_node_path("consumer", "/nonexistent/consumer")
            .with_shm_pool_size("producer", 12_582_912);
        let fixture = stage_example("shm-zero-copy-probe", &options).expect("staged");
        let staged = Manifest::from_yaml_file(&fixture.manifest).expect("re-parsed");
        let producer = staged
            .nodes
            .iter()
            .find(|node| node.id == "producer")
            .expect("the producer survived staging");
        assert_eq!(producer.shm_pool_size, Some(12_582_912));
        fixture.clean();
    }

    /// `build:` is dropped by default and kept on request.
    #[test]
    fn build_lines_are_dropped_unless_asked_for() {
        let base = StageOptions::new("rust-pipeline")
            .with_node_path("camera-sim", "/nonexistent/camera-sim")
            .with_node_path("detector-sim", "/nonexistent/detector-sim")
            .with_node_path("recorder-sim", "/nonexistent/recorder-sim");

        let dropped = stage_example("rust-pipeline", &base).expect("staged");
        let manifest = Manifest::from_yaml_file(&dropped.manifest).expect("re-parsed");
        assert!(manifest.nodes.iter().all(|node| node.build.is_none()));
        dropped.clean();

        let kept =
            stage_example("rust-pipeline", &base.clone().keeping_build(true)).expect("staged");
        let manifest = Manifest::from_yaml_file(&kept.manifest).expect("re-parsed");
        assert!(manifest.nodes.iter().all(|node| node.build.is_some()));
        kept.clean();
    }

    /// Every committed `path:` is repository-relative and resolves under the
    /// workspace's own `target/debug` — which is the claim the READMEs make.
    #[test]
    fn committed_paths_resolve_under_the_workspace_target_directory() {
        let expected = crate::paths::workspace_root().join("target").join("debug");
        for example in EXAMPLES {
            let binaries = committed_node_binaries(example).expect("parsed");
            assert!(!binaries.is_empty(), "{example} declares no node paths");
            for binary in binaries {
                assert!(
                    binary.declared.starts_with("../../target/debug/"),
                    "{example}/{}: {}",
                    binary.node,
                    binary.declared
                );
                assert_eq!(
                    binary.resolved.parent(),
                    Some(expected.as_path()),
                    "{example}/{}: {}",
                    binary.node,
                    binary.resolved.display()
                );
            }
        }
    }

    /// `..` is collapsed lexically, without touching the filesystem.
    #[test]
    fn paths_are_normalised_lexically() {
        assert_eq!(
            normalise(Path::new("/a/b/examples/x/../../target/debug/y")),
            PathBuf::from("/a/b/target/debug/y")
        );
        assert_eq!(normalise(Path::new("a/./b")), PathBuf::from("a/b"));
        assert_eq!(normalise(Path::new("../x")), PathBuf::from("../x"));
    }

    /// The legacy three-argument entry point still stages what it always did.
    #[test]
    fn the_map_based_entry_point_still_works() {
        let env = BTreeMap::from([("HELLO_TIMER_TICKS".to_owned(), "3".to_owned())]);
        let fixture = stage_example_env("hello-timer", &env);
        match fixture {
            Ok(fixture) => {
                let text = fixture.text().expect("readable");
                assert!(text.contains("HELLO_TIMER_TICKS"), "{text}");
                fixture.clean();
            }
            // The greeter has not been built in this checkout; the error must
            // still say exactly how to fix that.
            Err(error) => {
                let text = error.to_string();
                assert!(text.contains("cargo build -p hello-timer"), "{text}");
            }
        }
    }
}
