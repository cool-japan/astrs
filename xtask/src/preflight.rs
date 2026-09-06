//! `cargo xtask preflight`: the release gate (blueprint §20, §22).
//!
//! §20.3: *"repo policy allows no CI workflows beyond the publish ones, so
//! `scripts/ci-local.sh` is the per-change gate."* This module is what
//! that gate actually runs. It orchestrates every §20 quality gate as one
//! ordered list of steps, cheapest and most structural first (a broken
//! layer boundary or a stale schema fails in well under a second; a full
//! `cargo nextest run --workspace --all-features` is the most expensive
//! thing in this list and runs after everything cheaper already passed),
//! runs every step regardless of an earlier one failing, and reports
//! pass/fail per step so a run tells you everything that's wrong in one
//! pass rather than one thing at a time.
//!
//! # What this module does and does not test
//!
//! The four structural checks ([`crate::layer_lint`], [`crate::schema`],
//! the file-size audit, [`crate::version_pins`]) are pure, filesystem-only
//! logic and are exercised directly by this crate's own test suite,
//! including against the real repository. [`crate::sys_sweep`] sits
//! between the two categories below: it does spawn a subprocess (`cargo
//! tree`), but -- graph resolution only, no compilation -- that subprocess
//! is cheap enough that its own real-workspace test runs it for real too,
//! the same as the four pure checks. The `cargo fmt`/`clippy`/`nextest`/
//! `doc`/`deny` steps are thin subprocess wrappers around well-known
//! external tools; this module's tests cover the *wrapper* (exact argv and
//! env per step, and the spawn/exit-status plumbing against trivial
//! non-cargo commands) but deliberately never invoke `cargo clippy
//! --workspace` or `cargo nextest run --workspace` for real from a test,
//! because doing so from inside `cargo nextest run -p xtask` would itself
//! be exactly the workspace-wide build this crate's own development
//! process avoids running concurrently with other in-flight work.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::XtaskError;
use crate::{layer_lint, schema, snapshot_protocol, sys_sweep, version_pins, workspace};

/// The §20.1 file-size ceiling: `rslines 50`'s audit, reimplemented here so
/// `preflight` does not depend on a tool outside the workspace.
pub const MAX_FILE_LINES: usize = 2000;

/// One `.rs` file over [`MAX_FILE_LINES`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OversizedFile {
    /// The file's path.
    pub path: PathBuf,
    /// How many lines it has.
    pub lines: usize,
}

/// How one step of the preflight ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    /// The step succeeded.
    Pass,
    /// The step failed; `detail` is a one-line summary (a subprocess's
    /// exit status, or the first violation this crate's own check found).
    Fail {
        /// What went wrong.
        detail: String,
    },
    /// The step did not run (currently only `--publish-dry-run`'s absence).
    Skipped {
        /// Why.
        reason: String,
    },
}

/// The outcome of one preflight step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult {
    /// A human-readable name for this step.
    pub name: String,
    /// How it ended.
    pub status: StepStatus,
}

/// The outcome of a full `cargo xtask preflight` run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightReport {
    /// Every step, in the order it ran.
    pub steps: Vec<StepResult>,
}

impl PreflightReport {
    /// Whether every step either passed or was deliberately skipped --
    /// the condition [`crate::main`] maps to a zero exit code.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.steps
            .iter()
            .all(|step| !matches!(step.status, StepStatus::Fail { .. }))
    }

    /// A human-readable, one-line-per-step summary ending in a totals
    /// line.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut out = String::new();
        for step in &self.steps {
            let marker = match &step.status {
                StepStatus::Pass => "PASS",
                StepStatus::Fail { .. } => "FAIL",
                StepStatus::Skipped { .. } => "SKIP",
            };
            let _ = writeln!(out, "[{marker}] {}", step.name);
            match &step.status {
                StepStatus::Fail { detail } => {
                    let _ = writeln!(out, "       {detail}");
                }
                StepStatus::Skipped { reason } => {
                    let _ = writeln!(out, "       {reason}");
                }
                StepStatus::Pass => {}
            }
        }
        let failed = self
            .steps
            .iter()
            .filter(|step| matches!(step.status, StepStatus::Fail { .. }))
            .count();
        let skipped = self
            .steps
            .iter()
            .filter(|step| matches!(step.status, StepStatus::Skipped { .. }))
            .count();
        let _ = writeln!(
            out,
            "\n{} step(s): {failed} failed, {skipped} skipped, {} passed",
            self.steps.len(),
            self.steps.len() - failed - skipped
        );
        out
    }
}

/// One `cargo <args>` invocation this preflight will run, named for the
/// summary and carrying whatever environment variables it needs.
///
/// A plain data description rather than an immediately-run closure so this
/// crate's tests can assert on the exact argv/env of each step -- the
/// cheapest way to catch a typo'd flag, which is the most likely defect in
/// a module that is otherwise mostly subprocess plumbing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CargoInvocation {
    step_name: String,
    args: Vec<String>,
    env: Vec<(&'static str, &'static str)>,
}

impl CargoInvocation {
    fn new(step_name: impl Into<String>, args: &[&str]) -> Self {
        Self {
            step_name: step_name.into(),
            args: args.iter().map(|s| (*s).to_owned()).collect(),
            env: Vec::new(),
        }
    }

    fn with_env(mut self, key: &'static str, value: &'static str) -> Self {
        self.env.push((key, value));
        self
    }

    fn with_arg(mut self, value: impl Into<String>) -> Self {
        self.args.push(value.into());
        self
    }
}

fn fmt_check_invocation() -> CargoInvocation {
    CargoInvocation::new("cargo fmt --check", &["fmt", "--check"])
}

fn deny_bans_invocation() -> CargoInvocation {
    CargoInvocation::new("cargo deny check bans", &["deny", "check", "bans"])
}

/// The default-features run above resolves `astrs-data`'s `arrow-interop`
/// dependencies (`arrow-array`/`arrow-buffer`/`arrow-data`/`arrow-schema`)
/// out of the graph entirely -- they are `optional = true` and nothing
/// activates them -- so it never exercises the `wrappers` containment
/// deny.toml carries for them (see its "Arrow" section). Only a run that
/// activates the feature does, which is what makes this a separate step
/// rather than a flag folded into the one above: `--all-features` is a
/// `cargo-deny` option, not a `bans` one, so it has to precede `check`
/// rather than follow `bans` (`cargo deny --all-features check bans`, not
/// `cargo deny check bans --all-features`, which `cargo-deny`'s own arg
/// parser rejects outright).
fn deny_bans_all_features_invocation() -> CargoInvocation {
    CargoInvocation::new(
        "cargo deny --all-features check bans",
        &["deny", "--all-features", "check", "bans"],
    )
}

fn clippy_invocation() -> CargoInvocation {
    // §20.1: "cargo clippy --workspace --all-features -- -D warnings".
    // `--all-targets` is this crate's own addition (also required of every
    // wave by this task's own verification instructions): it reaches
    // tests/benches/examples too, which `--all-features` alone does not.
    CargoInvocation::new(
        "cargo clippy (workspace, all-targets, all-features, -D warnings)",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ],
    )
}

/// The packages whose *binaries* `astrs-conformance` (§20.3) spawns, and
/// which nothing else in this gate builds.
///
/// Cargo builds a dev-dependency's *library*, never its binaries, and
/// `cargo nextest run` builds test targets rather than bin artefacts -- so a
/// `nextest` step on a clean checkout finds no `target/<profile>/hello-timer`
/// and every M1/M2/M3 conformance test fails with `MissingBinary` before it
/// has proved anything. The suite refuses to paper over that by skipping
/// (`tests/conformance/README.md`: *"one that silently skipped would prove
/// nothing"*) and compiles nothing itself, so satisfying the precondition is
/// the harness's job -- this step.
///
/// The list is exactly the closure of what the suite spawns: the `astrs`
/// binary the CLI-process lane types, `astrs-replay-node` for M3's replay
/// substitution, and the eight example packages whose manifests name a
/// `path:` the suite stages. It is deliberately not `--workspace --bins`:
/// the other dozen example binaries are never spawned here, and linking
/// them would cost the gate minutes to prove nothing.
const CONFORMANCE_BINARY_PACKAGES: [&str; 10] = [
    "astrs-cli",
    "astrs-replay-node",
    "hello-timer",
    "rust-pipeline",
    "service-roundtrip",
    "shm-zero-copy-probe",
    "record-replay",
    "multi-daemon-cluster",
    "restart-policies",
    "error-propagation",
];

/// Builds [`CONFORMANCE_BINARY_PACKAGES`] in **one** invocation, under the
/// same `--all-features` resolution [`nextest_invocation`] uses.
///
/// Both properties are load-bearing. One invocation because these binaries
/// talk to each other over the wire and a mix of stale and fresh ones fails
/// as a decode error a long way from its cause; the same feature resolution
/// because a differently-featured `astrs-node-api` underneath a node is that
/// same skew by another route -- and because alternating feature sets would
/// otherwise rebuild the world on every run.
fn conformance_binaries_invocation() -> CargoInvocation {
    let mut invocation = CargoInvocation::new(
        "cargo build --all-features (astrs-conformance node binaries)",
        &["build", "--all-features"],
    );
    for package in CONFORMANCE_BINARY_PACKAGES {
        invocation = invocation.with_arg("-p").with_arg(package);
    }
    invocation
}

fn nextest_invocation() -> CargoInvocation {
    // §20.2: "cargo nextest run --workspace --all-features green at every
    // wave gate".
    CargoInvocation::new(
        "cargo nextest run (workspace, all-features)",
        &["nextest", "run", "--workspace", "--all-features"],
    )
}

fn doctest_invocation() -> CargoInvocation {
    // nextest cannot run doctests; §20.2's "doc tests compile" is this
    // separate step.
    CargoInvocation::new(
        "cargo test --doc (workspace, all-features)",
        &["test", "--doc", "--workspace", "--all-features"],
    )
}

fn doc_invocation() -> CargoInvocation {
    CargoInvocation::new(
        "cargo doc --no-deps (workspace, all-features, RUSTDOCFLAGS=-D warnings)",
        &["doc", "--no-deps", "--workspace", "--all-features"],
    )
    .with_env("RUSTDOCFLAGS", "-D warnings")
}

fn publish_dry_run_invocation(name: &str) -> CargoInvocation {
    CargoInvocation::new(
        format!("cargo publish --dry-run -p {name}"),
        &["publish", "--dry-run", "-p"],
    )
    .with_arg(name)
}

/// Spawn `program args...` in `cwd` with `env` set, and report whether it
/// exited successfully. Split out from [`run_invocation`] (which always
/// spawns `"cargo"`) so this crate's tests can exercise the actual
/// spawn/exit-status plumbing against trivial, always-available commands
/// (`true`/`false`) instead of a real, slow `cargo` invocation.
///
/// # Errors
///
/// [`XtaskError::Spawn`] if `program` could not be started at all.
fn spawn_and_capture(
    program: &str,
    args: &[String],
    env: &[(&'static str, &'static str)],
    cwd: &Path,
) -> Result<bool, XtaskError> {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd);
    for (key, value) in env {
        command.env(key, value);
    }
    let status = command
        .status()
        .map_err(|source| XtaskError::spawn(format!("{program} {}", args.join(" ")), source))?;
    Ok(status.success())
}

/// Run one [`CargoInvocation`] and turn its exit status into a
/// [`StepResult`]. Cargo's own output streams straight to this process's
/// stdout/stderr (inherited by default), so a failure's real diagnostic --
/// clippy's warning text, nextest's failing test names -- reaches whoever
/// ran `cargo xtask preflight` without this function re-rendering it.
fn run_invocation(root: &Path, invocation: &CargoInvocation) -> Result<StepResult, XtaskError> {
    let passed = spawn_and_capture("cargo", &invocation.args, &invocation.env, root)?;
    Ok(StepResult {
        name: invocation.step_name.clone(),
        status: if passed {
            StepStatus::Pass
        } else {
            StepStatus::Fail {
                detail: format!("`cargo {}` exited non-zero", invocation.args.join(" ")),
            }
        },
    })
}

/// Recursively collect every `.rs` file under `dir`, skipping `target/`
/// and any dot-directory (`.git`, `.github`, ...).
fn collect_rs_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), XtaskError> {
    let entries = std::fs::read_dir(dir).map_err(|source| XtaskError::io(dir, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| XtaskError::io(dir, source))?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name == "target" || name.starts_with('.') {
                continue;
            }
            collect_rs_files(&path, files)?;
        } else if name.ends_with(".rs") {
            files.push(path);
        }
    }
    Ok(())
}

/// The §20.1 file-size audit: every `.rs` file under `root`, `target/` and
/// dot-directories excluded, that is over [`MAX_FILE_LINES`] lines.
/// Sorted largest first, ties broken by path, so the output is
/// deterministic regardless of directory-listing order.
///
/// # Errors
///
/// [`XtaskError::Io`] if a directory cannot be listed or a file cannot be
/// read.
pub fn audit_file_sizes(root: &Path) -> Result<Vec<OversizedFile>, XtaskError> {
    let mut files = Vec::new();
    collect_rs_files(root, &mut files)?;

    let mut offenders = Vec::new();
    for path in files {
        let text =
            std::fs::read_to_string(&path).map_err(|source| XtaskError::io(&path, source))?;
        let lines = text.lines().count();
        if lines > MAX_FILE_LINES {
            offenders.push(OversizedFile { path, lines });
        }
    }
    offenders.sort_by(|a, b| b.lines.cmp(&a.lines).then_with(|| a.path.cmp(&b.path)));
    Ok(offenders)
}

fn file_size_step(root: &Path) -> Result<StepResult, XtaskError> {
    let offenders = audit_file_sizes(root)?;
    let status = if offenders.is_empty() {
        StepStatus::Pass
    } else {
        let detail = offenders
            .iter()
            .map(|file| format!("{} ({} lines)", file.path.display(), file.lines))
            .collect::<Vec<_>>()
            .join(", ");
        StepStatus::Fail { detail }
    };
    Ok(StepResult {
        name: format!("file-size audit (.rs files <= {MAX_FILE_LINES} lines)"),
        status,
    })
}

fn layer_lint_step(root: &Path) -> Result<StepResult, XtaskError> {
    let report = layer_lint::run(root, layer_lint::PRODUCTION_LAYERS)?;
    let status = if report.is_clean() {
        StepStatus::Pass
    } else {
        StepStatus::Fail {
            detail: report
                .violations
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        }
    };
    Ok(StepResult {
        name: "layer-lint".to_owned(),
        status,
    })
}

fn schema_check_step(root: &Path) -> Result<StepResult, XtaskError> {
    let outcome = schema::check(root)?;
    let status = if outcome.is_match() {
        StepStatus::Pass
    } else {
        StepStatus::Fail {
            detail: outcome.to_string(),
        }
    };
    Ok(StepResult {
        name: "schema --check".to_owned(),
        status,
    })
}

fn version_pins_step(root: &Path) -> Result<StepResult, XtaskError> {
    let report = version_pins::run(root)?;
    let status = if report.is_clean() {
        StepStatus::Pass
    } else {
        StepStatus::Fail {
            detail: report
                .violations
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        }
    };
    Ok(StepResult {
        name: "no-inline-version-pins".to_owned(),
        status,
    })
}

/// One [`sys_sweep::run`] call turned into a [`StepResult`] -- called once
/// per [`sys_sweep::FeatureMode`] (see [`run`]'s own doc comment), each
/// becoming its own named, independently pass/fail step, mirroring how
/// [`deny_bans_invocation`] and [`deny_bans_all_features_invocation`] are
/// two separate steps rather than one step covering both modes.
///
/// [`sys_sweep::run`] can fail two different ways, and this function
/// deliberately treats them differently -- the same triage
/// [`run_invocation`] already does for `fmt`/`deny`/`clippy`/`nextest`/
/// `doc`, just spelled out explicitly here because [`sys_sweep::run`]
/// returns a `Result` rather than the plain pass/fail
/// [`spawn_and_capture`] gives those: [`crate::error::XtaskError::Spawn`]
/// (cargo itself missing from `PATH`) still aborts the whole preflight run
/// via `?`, because at that point *no* step in this list can run, cargo
/// subprocess or not; but [`crate::error::XtaskError::CommandFailed`]
/// (cargo spawned fine and exited non-zero -- typically an unresolvable
/// dependency graph) is downgraded to an ordinary [`StepStatus::Fail`] for
/// this one step, exactly as `run_invocation` already treats every sibling
/// cargo subprocess's non-zero exit. Without this, a broken `Cargo.lock`
/// would make this step abort the entire report with a bare one-line
/// message on `main`'s stderr instead of the full per-step summary this
/// module's own docs promise -- silently swallowing whatever the cheaper
/// structural steps before it already found, and never even attempting
/// the steps after it.
fn sys_sweep_step(root: &Path, mode: sys_sweep::FeatureMode) -> Result<StepResult, XtaskError> {
    let name = format!("*-sys sweep ({})", mode.label());
    let status = match sys_sweep::run(root, mode) {
        Ok(report) if report.is_clean() => StepStatus::Pass,
        Ok(report) => StepStatus::Fail {
            detail: format!(
                "disallowed *-sys/libc crate(s) outside the verified allowlist: {}",
                report.disallowed.join(", ")
            ),
        },
        Err(XtaskError::CommandFailed { command, stderr }) => StepStatus::Fail {
            detail: format!("`{command}` could not resolve the dependency graph: {stderr}"),
        },
        Err(other) => return Err(other),
    };
    Ok(StepResult { name, status })
}

fn snapshot_protocol_step(root: &Path) -> Result<StepResult, XtaskError> {
    // Preflight is the release gate: always the authoritative mechanism,
    // never `--frozen-only`'s fast path (see that module's docs).
    let outcome = snapshot_protocol::run_full(root)?;
    let status = if outcome.passed {
        StepStatus::Pass
    } else {
        StepStatus::Fail {
            detail: outcome.detail,
        }
    };
    Ok(StepResult {
        name: "snapshot-protocol".to_owned(),
        status,
    })
}

/// Every publishable workspace member (`publish != false`), as one
/// `cargo publish --dry-run -p <name>` invocation apiece, in the
/// dependency order [`workspace::topological_order`] computes -- so a
/// crate is never dry-run before something it depends on.
///
/// # Errors
///
/// Whatever [`workspace::discover_members`] or
/// [`workspace::topological_order`] returns.
fn publish_dry_run_invocations(root: &Path) -> Result<Vec<CargoInvocation>, XtaskError> {
    let members = workspace::discover_members(root)?;
    let publishable: Vec<workspace::Member> = members
        .into_iter()
        .filter(|member| member.publish)
        .collect();
    let order = workspace::topological_order(&publishable)?;
    Ok(order
        .iter()
        .map(|name| publish_dry_run_invocation(name))
        .collect())
}

/// Run the full release gate.
///
/// Structural, in-process checks (layer-lint, `schema --check`, the
/// file-size audit, no-inline-version-pins) run first -- typically well
/// under a second combined -- followed by `cargo fmt --check` and `cargo
/// deny check bans` in both its default-features and `--all-features`
/// forms (cheap, no compilation -- see
/// [`deny_bans_all_features_invocation`] for why the arrow-interop
/// wrappers need the second one), then the `*-sys` sweep, itself run once
/// per feature mode for the identical reason ([`sys_sweep_step`]'s own doc
/// comment) and no more expensive than the deny steps just before it (also
/// graph resolution only), the wire-protocol freeze (compiles and tests
/// one crate), then the expensive workspace-wide steps (clippy, the
/// conformance suite's node binaries -- see
/// [`conformance_binaries_invocation`] for why `nextest` cannot produce
/// them itself -- nextest, doctests, docs), and finally, only when
/// `publish_dry_run` is set, a
/// `cargo publish --dry-run` per publishable crate in dependency order.
/// Every step runs regardless of an earlier failure; [`PreflightReport`]
/// carries the full picture, and [`PreflightReport::is_success`] is what a
/// caller should check for the pass/fail verdict.
///
/// # Errors
///
/// An [`XtaskError`] only for a failure in preflight's own machinery (a
/// Cargo.toml that will not parse, `cargo` itself not found on `PATH`) --
/// never for a step that ran and failed; that is a `Fail` entry in the
/// report, not an `Err` here.
pub fn run(root: &Path, publish_dry_run: bool) -> Result<PreflightReport, XtaskError> {
    let mut steps = vec![
        layer_lint_step(root)?,
        schema_check_step(root)?,
        file_size_step(root)?,
        version_pins_step(root)?,
        run_invocation(root, &fmt_check_invocation())?,
        run_invocation(root, &deny_bans_invocation())?,
        run_invocation(root, &deny_bans_all_features_invocation())?,
        sys_sweep_step(root, sys_sweep::FeatureMode::NoDefaultFeatures)?,
        sys_sweep_step(root, sys_sweep::FeatureMode::AllFeatures)?,
        snapshot_protocol_step(root)?,
        run_invocation(root, &clippy_invocation())?,
        run_invocation(root, &conformance_binaries_invocation())?,
        run_invocation(root, &nextest_invocation())?,
        run_invocation(root, &doctest_invocation())?,
        run_invocation(root, &doc_invocation())?,
    ];

    if publish_dry_run {
        for invocation in publish_dry_run_invocations(root)? {
            steps.push(run_invocation(root, &invocation)?);
        }
    } else {
        steps.push(StepResult {
            name: "cargo publish --dry-run (per publishable crate)".to_owned(),
            status: StepStatus::Skipped {
                reason: "pass --publish-dry-run to run this".to_owned(),
            },
        });
    }

    Ok(PreflightReport { steps })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "astrs-xtask-preflight-test-{}-{name}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // -- Invocation shape: catches a typo'd flag without spawning anything.

    #[test]
    fn fmt_check_is_a_plain_check_no_workspace_flag_needed() {
        assert_eq!(fmt_check_invocation().args, vec!["fmt", "--check"]);
    }

    #[test]
    fn clippy_covers_the_workspace_all_targets_all_features_and_denies_warnings() {
        let invocation = clippy_invocation();
        assert_eq!(
            invocation.args,
            vec![
                "clippy",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--",
                "-D",
                "warnings"
            ]
        );
        assert!(invocation.env.is_empty());
    }

    /// One invocation, `--all-features`, every package the conformance
    /// suite spawns -- and the `-p` before each of them, which is the flag
    /// most likely to be lost in an edit.
    #[test]
    fn the_conformance_binaries_are_built_in_one_all_features_invocation() {
        let invocation = conformance_binaries_invocation();
        let mut expected = vec!["build".to_owned(), "--all-features".to_owned()];
        for package in CONFORMANCE_BINARY_PACKAGES {
            expected.push("-p".to_owned());
            expected.push(package.to_owned());
        }
        assert_eq!(invocation.args, expected);
        assert!(invocation.env.is_empty());
    }

    /// Every package named there is a real workspace member: a typo would
    /// otherwise turn the whole step into a `cargo build` error and take the
    /// gate down for a reason that has nothing to do with the change under
    /// test.
    #[test]
    fn every_conformance_binary_package_is_a_workspace_member() {
        let root = workspace::workspace_root();
        let members = workspace::discover_members(&root).expect("the workspace members");
        let names: Vec<&str> = members.iter().map(|member| member.name.as_str()).collect();
        for package in CONFORMANCE_BINARY_PACKAGES {
            assert!(
                names.contains(&package),
                "{package} is not a member: {names:?}"
            );
        }
    }

    #[test]
    fn nextest_covers_the_workspace_and_all_features() {
        assert_eq!(
            nextest_invocation().args,
            vec!["nextest", "run", "--workspace", "--all-features"]
        );
    }

    #[test]
    fn doctest_covers_the_workspace_and_all_features() {
        assert_eq!(
            doctest_invocation().args,
            vec!["test", "--doc", "--workspace", "--all-features"]
        );
    }

    #[test]
    fn doc_step_denies_warnings_via_rustdocflags_env() {
        let invocation = doc_invocation();
        assert_eq!(
            invocation.args,
            vec!["doc", "--no-deps", "--workspace", "--all-features"]
        );
        assert_eq!(invocation.env, vec![("RUSTDOCFLAGS", "-D warnings")]);
    }

    #[test]
    fn deny_bans_is_exactly_that() {
        assert_eq!(deny_bans_invocation().args, vec!["deny", "check", "bans"]);
    }

    #[test]
    fn deny_bans_all_features_puts_the_flag_before_the_subcommand() {
        // `cargo-deny`'s own arg parser is `deny [OPTIONS] <COMMAND>`, so
        // `--all-features` has to precede `check`, not follow `bans` --
        // getting this backwards is a silent no-op (cargo-deny would reject
        // it outright, which nextest's `spawn_and_capture` coverage for a
        // genuinely bad argv already exercises; this test pins the argv
        // shape itself so a future edit cannot slide it out of order).
        assert_eq!(
            deny_bans_all_features_invocation().args,
            vec!["deny", "--all-features", "check", "bans"]
        );
    }

    #[test]
    fn publish_dry_run_names_the_crate() {
        let invocation = publish_dry_run_invocation("astrs-wire");
        assert_eq!(
            invocation.args,
            vec!["publish", "--dry-run", "-p", "astrs-wire"]
        );
    }

    // -- Spawn plumbing: real subprocesses, but trivial, fast, non-cargo
    // -- ones -- proves the runner's status-capture and error-conversion
    // -- logic without ever invoking `cargo` for real from a test.

    #[test]
    fn spawn_and_capture_reports_success_and_failure_honestly() {
        let cwd = std::env::temp_dir();
        assert!(spawn_and_capture("true", &[], &[], &cwd).unwrap());
        assert!(!spawn_and_capture("false", &[], &[], &cwd).unwrap());
    }

    #[test]
    fn spawn_and_capture_errors_when_the_program_does_not_exist() {
        let cwd = std::env::temp_dir();
        let err = spawn_and_capture("definitely-not-a-real-program-xyz-astrs", &[], &[], &cwd)
            .unwrap_err();
        assert!(matches!(err, XtaskError::Spawn { .. }));
    }

    // -- `sys_sweep_step`'s error triage: a `cargo tree` that spawns but
    // -- cannot resolve the graph must become an ordinary `Fail` for this
    // -- one step, never an `Err` that would abort the whole preflight run
    // -- (unlike a `cargo` that cannot even be spawned, which still must
    // -- propagate -- see the function's own doc comment for why the two
    // -- cases are deliberately not treated the same way).

    #[test]
    fn sys_sweep_step_fails_the_step_not_the_whole_run_when_cargo_tree_cannot_resolve() {
        // A directory with no `Cargo.toml` in it or any parent: `cargo
        // tree` spawns fine but exits non-zero (`XtaskError::CommandFailed`
        // -- see `sys_sweep`'s own identically-shaped test), the exact
        // condition this function exists to downgrade to a `Fail`.
        let dir = scratch_dir("sys-sweep-step-no-manifest");
        let result = sys_sweep_step(&dir, sys_sweep::FeatureMode::NoDefaultFeatures);
        let step = result.expect("a resolution failure must not propagate as an XtaskError");
        assert!(matches!(step.status, StepStatus::Fail { .. }), "{step:?}");
        assert!(step.name.contains("no-default-features"));
    }

    // -- File-size audit.

    #[test]
    fn a_file_at_or_under_the_limit_passes() {
        let dir = scratch_dir("under-limit");
        let body = "// a line\n".repeat(MAX_FILE_LINES);
        std::fs::write(dir.join("fine.rs"), body).unwrap();
        assert!(audit_file_sizes(&dir).unwrap().is_empty());
    }

    #[test]
    fn a_file_over_the_limit_is_flagged_with_its_line_count() {
        let dir = scratch_dir("over-limit");
        let body = "// a line\n".repeat(MAX_FILE_LINES + 1);
        std::fs::write(dir.join("big.rs"), body).unwrap();
        let offenders = audit_file_sizes(&dir).unwrap();
        assert_eq!(offenders.len(), 1);
        assert_eq!(offenders[0].lines, MAX_FILE_LINES + 1);
        assert!(offenders[0].path.ends_with("big.rs"));
    }

    #[test]
    fn target_and_dot_directories_are_never_walked() {
        let dir = scratch_dir("excluded-dirs");
        let big = "// a line\n".repeat(MAX_FILE_LINES + 1);
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("target/debug/generated.rs"), &big).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/oversized.rs"), &big).unwrap();
        std::fs::write(dir.join("small.rs"), "// fine\n").unwrap();
        assert!(audit_file_sizes(&dir).unwrap().is_empty());
    }

    #[test]
    fn non_rs_files_are_ignored_regardless_of_size() {
        let dir = scratch_dir("non-rs");
        let big = "x\n".repeat(MAX_FILE_LINES + 1);
        std::fs::write(dir.join("data.txt"), big).unwrap();
        assert!(audit_file_sizes(&dir).unwrap().is_empty());
    }

    #[test]
    fn offenders_sort_largest_first() {
        let dir = scratch_dir("sort-order");
        std::fs::write(dir.join("a.rs"), "// x\n".repeat(MAX_FILE_LINES + 5)).unwrap();
        std::fs::write(dir.join("b.rs"), "// x\n".repeat(MAX_FILE_LINES + 50)).unwrap();
        let offenders = audit_file_sizes(&dir).unwrap();
        assert_eq!(offenders.len(), 2);
        assert!(offenders[0].lines > offenders[1].lines);
        assert!(offenders[0].path.ends_with("b.rs"));
    }

    #[test]
    fn the_real_workspace_has_no_oversized_rs_files() {
        let root = workspace::workspace_root();
        let offenders = audit_file_sizes(&root).unwrap();
        assert!(
            offenders.is_empty(),
            "files over {MAX_FILE_LINES} lines: {offenders:?}"
        );
    }

    // -- Report formatting.

    fn step(name: &str, status: StepStatus) -> StepResult {
        StepResult {
            name: name.to_owned(),
            status,
        }
    }

    #[test]
    fn is_success_is_false_if_anything_failed() {
        let report = PreflightReport {
            steps: vec![
                step("a", StepStatus::Pass),
                step(
                    "b",
                    StepStatus::Fail {
                        detail: "broke".to_owned(),
                    },
                ),
            ],
        };
        assert!(!report.is_success());
    }

    #[test]
    fn is_success_is_true_with_only_pass_and_skip() {
        let report = PreflightReport {
            steps: vec![
                step("a", StepStatus::Pass),
                step(
                    "b",
                    StepStatus::Skipped {
                        reason: "not requested".to_owned(),
                    },
                ),
            ],
        };
        assert!(report.is_success());
    }

    #[test]
    fn summary_names_every_step_and_totals_correctly() {
        let report = PreflightReport {
            steps: vec![
                step("a", StepStatus::Pass),
                step(
                    "b",
                    StepStatus::Fail {
                        detail: "kaboom".to_owned(),
                    },
                ),
                step(
                    "c",
                    StepStatus::Skipped {
                        reason: "flag not set".to_owned(),
                    },
                ),
            ],
        };
        let summary = report.summary();
        assert!(summary.contains("[PASS] a"));
        assert!(summary.contains("[FAIL] b"));
        assert!(summary.contains("kaboom"));
        assert!(summary.contains("[SKIP] c"));
        assert!(summary.contains("flag not set"));
        assert!(summary.contains("3 step(s): 1 failed, 1 skipped, 1 passed"));
    }

    // -- Publish ordering, against the real repository (read-only,
    // -- no subprocess: parses Cargo.toml files and computes an order).

    #[test]
    fn publish_dry_run_invocations_cover_every_publishable_crate_in_dependency_order() {
        let root = workspace::workspace_root();
        let invocations = publish_dry_run_invocations(&root).unwrap();

        let names: Vec<String> = invocations
            .iter()
            .map(|invocation| invocation.args.last().unwrap().clone())
            .collect();

        // publish = false members must never appear.
        assert!(!names.contains(&"xtask".to_owned()));
        assert!(!names.contains(&"astrs-conformance".to_owned()));
        assert!(!names.contains(&"hello-timer".to_owned()));

        // A representative sample of real publishable crates must appear.
        for expected in [
            "astrs-wire",
            "astrs-time",
            "astrs-data",
            "astrs-cli",
            "astrs-tui",
        ] {
            assert!(names.contains(&expected.to_owned()), "missing {expected}");
        }

        // astrs-cli depends (directly and transitively) on nearly every
        // other publishable crate; it must be ordered after what it needs.
        let position = |name: &str| names.iter().position(|n| n == name).unwrap();
        assert!(position("astrs-wire") < position("astrs-cli"));
        assert!(position("astrs-tui") < position("astrs-cli"));
        assert!(position("astrs-daemon") < position("astrs-cli"));

        for invocation in &invocations {
            assert_eq!(invocation.args[0], "publish");
            assert_eq!(invocation.args[1], "--dry-run");
            assert_eq!(invocation.args[2], "-p");
        }
    }
}
