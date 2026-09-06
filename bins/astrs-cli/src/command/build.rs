//! `astrs build` — run a manifest's `build:` lines, without running the
//! graph (blueprint §17).
//!
//! The same engine `astrs run`'s build phase uses, reached the same way:
//! [`astrs_daemon::plan_dataflow`] turns the manifest into the ordered
//! [`astrs_daemon::BuildStep`] list, and
//! [`astrs_daemon::dataflow::run_step`] executes one — with §16's scrubbed
//! environment, shlex argv splitting, no shell, its own process group, and
//! both streams captured. Nothing about *how a build line runs* is decided
//! in this crate.
//!
//! What this verb adds over the run's own build phase is what a standalone
//! build verb is for:
//!
//! - `--node <id>`, to rebuild one node without touching the rest;
//! - `--release`, appended to each line for the common `cargo build` case;
//! - **per-node log capture** into `<runtime dir>/build/<node>.log`, so a
//!   build that scrolled past is still readable afterwards (§24.2);
//! - a typed report with `--json`, so CI can read step outcomes rather than
//!   scrape text.
//!
//! ```text
//!   manifest ─► plan_dataflow ─► [BuildStep] ──filter(--node)──┐
//!                                                              ▼
//!         <runtime>/build/<node>.log ◄── capture ── run_step (§16 hygiene)
//!                                                              │
//!                            stop at the first failure ◄───────┘
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};

use astrs_daemon::dataflow::{StepOutcome, run_step};
use astrs_daemon::spawn::{EnvPolicy, Spawner};
use astrs_daemon::{BuildStep, plan_dataflow};
use astrs_manifest::Manifest;
use astrs_wire::{DataflowId, NodeId};

use crate::error::CliError;

/// `astrs build`'s arguments, already parsed and resolved.
#[derive(Debug, Clone)]
pub struct BuildArgs {
    /// The manifest whose nodes to build.
    pub manifest_path: PathBuf,
    /// Append `--release` to every line.
    pub release: bool,
    /// Only build this node.
    pub node: Option<String>,
    /// Where the lines run; the manifest's directory by default.
    pub working_dir: Option<PathBuf>,
    /// Where per-node logs are captured.
    pub runtime_dir: Option<PathBuf>,
    /// Emit JSON rather than a human summary.
    pub json: bool,
}

impl BuildArgs {
    /// The arguments for building `manifest_path` with every default.
    #[must_use]
    pub fn new(manifest_path: impl Into<PathBuf>) -> Self {
        Self {
            manifest_path: manifest_path.into(),
            release: false,
            node: None,
            working_dir: None,
            runtime_dir: None,
            json: false,
        }
    }
}

/// One node's build, as this verb reports it.
#[derive(Debug, Clone)]
pub struct NodeBuild {
    /// The node.
    pub node: NodeId,
    /// The command line that ran.
    pub command: String,
    /// Its exit code, or `None` when a signal ended it.
    pub exit_code: Option<i32>,
    /// Where its output was captured.
    pub log_path: Option<PathBuf>,
    /// Everything it wrote.
    pub output: String,
}

impl NodeBuild {
    /// Whether the step succeeded.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// What one `astrs build` did.
#[derive(Debug, Clone, Default)]
pub struct BuildCliReport {
    /// The steps that ran, in order, up to and including any failure.
    pub steps: Vec<NodeBuild>,
    /// Nodes named in the manifest that declared no `build:` line at all.
    pub skipped: Vec<NodeId>,
    /// The step that could not be started at all, when one could not.
    pub start_failure: Option<String>,
}

impl BuildCliReport {
    /// Whether every step that ran succeeded.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.start_failure.is_none() && self.steps.iter().all(NodeBuild::is_success)
    }

    /// The process exit code: `0` for a build that succeeded, `1` for one
    /// that did not.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        i32::from(!self.is_success())
    }

    /// The first step that failed, when one did.
    #[must_use]
    pub fn failed_step(&self) -> Option<&NodeBuild> {
        self.steps.iter().find(|step| !step.is_success())
    }

    /// The `--json` form.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let steps: Vec<serde_json::Value> = self
            .steps
            .iter()
            .map(|step| {
                serde_json::json!({
                    "node": step.node.as_str(),
                    "command": step.command,
                    "exit_code": step.exit_code,
                    "success": step.is_success(),
                    "log": step.log_path.as_ref().map(|p| p.display().to_string()),
                })
            })
            .collect();
        serde_json::json!({
            "success": self.is_success(),
            "exit_code": self.exit_code(),
            "start_failure": self.start_failure,
            "skipped": self.skipped.iter().map(NodeId::as_str).collect::<Vec<_>>(),
            "steps": steps,
        })
    }
}

/// Builds a manifest's nodes, writing progress to `out`.
///
/// Every step's captured output is written to the terminal *and* to its own
/// file: a build is the one place where "I want to see it now" and "I want
/// to read it after it scrolled away" are both true, and neither is served
/// by the other.
///
/// # Errors
///
/// - [`CliError::Manifest`] / [`CliError::Validation`] if the manifest is
///   not usable.
/// - [`CliError::Daemon`] if the manifest cannot be planned.
/// - [`CliError::BadArgument`] if `--node` names a node the manifest does
///   not have.
/// - [`CliError::Io`] if a capture file cannot be written.
///
/// A build line that *fails* is not an error: it is a [`BuildCliReport`]
/// whose output the operator needs to read, exactly as
/// [`astrs_daemon::BuildReport`]'s own docs argue.
pub fn run(out: &mut dyn Write, args: &BuildArgs) -> Result<BuildCliReport, CliError> {
    let manifest = Manifest::from_yaml_file(&args.manifest_path)?;
    manifest.validate()?;

    let working_dir = args
        .working_dir
        .clone()
        .unwrap_or_else(|| manifest_dir(&args.manifest_path));
    let runtime_dir = crate::runtime_dir::runtime_dir(args.runtime_dir.as_deref());
    let log_dir = crate::runtime_dir::build_log_dir(&runtime_dir);

    let policy = EnvPolicy::new();
    let plan = plan_dataflow(
        DataflowId::generate(),
        &manifest,
        &policy.scrub_process_env(),
    )?;

    let selected = select_steps(&plan.build_steps, args.node.as_deref(), &manifest)?;
    let skipped = nodes_without_build(&manifest, &plan.build_steps);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| CliError::io(&args.manifest_path, source))?;

    let spawner = Spawner::with_policy(working_dir, policy);
    let mut report = BuildCliReport {
        skipped,
        ..BuildCliReport::default()
    };

    for step in selected {
        let step = apply_release(&step, args.release);
        let _ = writeln!(out, "building {}: {}", step.node, step.command);
        match runtime.block_on(run_step(&spawner, &step)) {
            Ok(outcome) => {
                let log_path = capture(&log_dir, &outcome)?;
                if !outcome.output.is_empty() {
                    let _ = write!(out, "{}", ensure_newline(&outcome.output));
                }
                let failed = !outcome.is_success();
                report.steps.push(NodeBuild {
                    node: outcome.node,
                    command: outcome.command,
                    exit_code: outcome.exit_code,
                    log_path,
                    output: outcome.output,
                });
                if failed {
                    break;
                }
            }
            Err(error) => {
                report.start_failure = Some(error.to_string());
                break;
            }
        }
    }

    if args.json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&report.to_json()).unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        let _ = writeln!(out, "{}", summary(&report, &log_dir));
    }
    Ok(report)
}

/// The human one-liner (or two) a finished build prints.
fn summary(report: &BuildCliReport, log_dir: &Path) -> String {
    let mut text = if let Some(reason) = &report.start_failure {
        format!("build could not start: {reason}")
    } else if let Some(step) = report.failed_step() {
        format!(
            "build failed at `{}` ({})",
            step.node,
            match step.exit_code {
                Some(code) => format!("exit code {code}"),
                None => "ended by a signal".to_owned(),
            }
        )
    } else {
        format!("built {} node(s)", report.steps.len())
    };
    if !report.skipped.is_empty() {
        text.push_str(&format!(
            "\n{} node(s) declare no `build:` line: {}",
            report.skipped.len(),
            report
                .skipped
                .iter()
                .map(NodeId::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !report.steps.is_empty() {
        text.push_str(&format!("\nlogs: {}", log_dir.display()));
    }
    text
}

/// The build steps this invocation should run.
///
/// # Errors
///
/// [`CliError::BadArgument`] when `--node` names something the manifest does
/// not declare — deliberately distinguished from "that node declares no
/// `build:` line", which is an empty selection rather than a mistake.
fn select_steps(
    steps: &[BuildStep],
    node: Option<&str>,
    manifest: &Manifest,
) -> Result<Vec<BuildStep>, CliError> {
    let Some(wanted) = node else {
        return Ok(steps.to_vec());
    };
    if !manifest.nodes.iter().any(|node| node.id == wanted) {
        return Err(CliError::BadArgument {
            flag: "node",
            value: wanted.to_owned(),
            reason: format!(
                "the manifest declares no such node (it has: {})",
                manifest
                    .nodes
                    .iter()
                    .map(|node| node.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
    }
    Ok(steps
        .iter()
        .filter(|step| step.node.as_str() == wanted)
        .cloned()
        .collect())
}

/// The nodes with no `build:` line, so the report can say so rather than
/// leaving a user wondering why their node never appeared.
fn nodes_without_build(manifest: &Manifest, steps: &[BuildStep]) -> Vec<NodeId> {
    manifest
        .nodes
        .iter()
        .filter(|node| !steps.iter().any(|step| step.node.as_str() == node.id))
        .filter_map(|node| NodeId::new(&node.id).ok())
        .collect()
}

/// Appends `--release` to a step's command line when asked.
///
/// Appended to the *string*, before the shlex split `run_step` does, so it
/// becomes one more argument rather than being glued onto the last one.
fn apply_release(step: &BuildStep, release: bool) -> BuildStep {
    if !release {
        return step.clone();
    }
    BuildStep {
        node: step.node.clone(),
        command: format!("{} --release", step.command),
        working_dir: step.working_dir.clone(),
    }
}

/// Writes one step's output to `<log_dir>/<node>.log`, returning where.
///
/// A capture failure is an error rather than a warning: the whole point of
/// this verb over `astrs run`'s build phase is that the output survives, so
/// silently not saving it would be the one failure a user could not detect.
fn capture(log_dir: &Path, outcome: &StepOutcome) -> Result<Option<PathBuf>, CliError> {
    crate::runtime_dir::ensure_dir(log_dir)?;
    let path = log_dir.join(format!("{}.log", outcome.node));
    let header = format!(
        "# astrs build — node {}\n# command: {}\n# exit: {}\n",
        outcome.node,
        outcome.command,
        match outcome.exit_code {
            Some(code) => code.to_string(),
            None => "signal".to_owned(),
        }
    );
    std::fs::write(
        &path,
        format!("{header}{}", ensure_newline(&outcome.output)),
    )
    .map_err(|source| CliError::io(&path, source))?;
    Ok(Some(path))
}

/// `text` with a trailing newline, and nothing added when it already ends
/// in one (or is empty).
fn ensure_newline(text: &str) -> String {
    if text.is_empty() || text.ends_with('\n') {
        text.to_owned()
    } else {
        format!("{text}\n")
    }
}

/// The directory a manifest's relative paths resolve against.
fn manifest_dir(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("astrs-cli-build-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_manifest(dir: &Path, yaml: &str) -> PathBuf {
        let path = dir.join("dataflow.yml");
        std::fs::write(&path, yaml).unwrap();
        path
    }

    fn args(dir: &Path, path: PathBuf) -> BuildArgs {
        let mut args = BuildArgs::new(path);
        args.working_dir = Some(dir.to_path_buf());
        args.runtime_dir = Some(dir.to_path_buf());
        args
    }

    const TWO_NODES: &str = "\
nodes:
  - id: alpha
    path: /usr/bin/true
    build: /bin/sh -c 'echo alpha-built'
  - id: beta
    path: /usr/bin/true
    build: /bin/sh -c 'echo beta-built'
";

    #[test]
    fn every_build_line_runs_in_manifest_order_and_is_captured() {
        let dir = scratch("ok");
        let path = write_manifest(&dir, TWO_NODES);
        let mut out = Vec::new();
        let report = run(&mut out, &args(&dir, path)).unwrap();
        let text = String::from_utf8(out).unwrap();

        assert!(report.is_success(), "{report:?}");
        assert_eq!(report.exit_code(), 0);
        assert_eq!(report.steps.len(), 2);
        assert_eq!(report.steps[0].node.as_str(), "alpha");
        assert_eq!(report.steps[1].node.as_str(), "beta");
        assert!(text.contains("alpha-built"), "{text}");
        assert!(text.contains("beta-built"), "{text}");

        let log = crate::runtime_dir::build_log_dir(&dir).join("alpha.log");
        let captured = std::fs::read_to_string(&log).unwrap();
        assert!(captured.contains("alpha-built"), "{captured}");
        assert!(captured.contains("# exit: 0"), "{captured}");
    }

    #[test]
    fn a_failing_line_stops_the_build_and_is_reported_as_data() {
        let dir = scratch("failure");
        let path = write_manifest(
            &dir,
            "\
nodes:
  - id: alpha
    path: /usr/bin/true
    build: /bin/sh -c 'echo nope; exit 7'
  - id: beta
    path: /usr/bin/true
    build: /bin/sh -c 'echo never'
",
        );
        let mut out = Vec::new();
        let report = run(&mut out, &args(&dir, path)).unwrap();
        let text = String::from_utf8(out).unwrap();

        assert!(!report.is_success());
        assert_eq!(report.exit_code(), 1);
        assert_eq!(report.steps.len(), 1, "the second line must not have run");
        assert_eq!(report.failed_step().unwrap().exit_code, Some(7));
        assert!(text.contains("build failed at `alpha`"), "{text}");
        assert!(!text.contains("never"), "{text}");
    }

    #[test]
    fn one_node_can_be_built_alone() {
        let dir = scratch("one-node");
        let path = write_manifest(&dir, TWO_NODES);
        let mut a = args(&dir, path);
        a.node = Some("beta".to_owned());
        let mut out = Vec::new();
        let report = run(&mut out, &a).unwrap();
        assert_eq!(report.steps.len(), 1);
        assert_eq!(report.steps[0].node.as_str(), "beta");
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("alpha-built"), "{text}");
    }

    #[test]
    fn an_unknown_node_names_the_ones_that_do_exist() {
        let dir = scratch("unknown-node");
        let path = write_manifest(&dir, TWO_NODES);
        let mut a = args(&dir, path);
        a.node = Some("gamma".to_owned());
        let error = run(&mut Vec::new(), &a).unwrap_err();
        match error {
            CliError::BadArgument { flag, reason, .. } => {
                assert_eq!(flag, "node");
                assert!(reason.contains("alpha"), "{reason}");
                assert!(reason.contains("beta"), "{reason}");
            }
            other => panic!("expected BadArgument, got {other}"),
        }
    }

    #[test]
    fn nodes_with_no_build_line_are_reported_rather_than_silently_ignored() {
        let dir = scratch("no-build");
        let path = write_manifest(
            &dir,
            "nodes:\n  - id: alpha\n    path: /usr/bin/true\n  - id: beta\n    path: /usr/bin/true\n    build: /bin/sh -c 'true'\n",
        );
        let mut out = Vec::new();
        let report = run(&mut out, &args(&dir, path)).unwrap();
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].as_str(), "alpha");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("declare no `build:` line"), "{text}");
    }

    #[test]
    fn release_appends_one_more_argument() {
        let step = BuildStep {
            node: NodeId::new("alpha").unwrap(),
            command: "cargo build -p alpha".to_owned(),
            working_dir: None,
        };
        assert_eq!(apply_release(&step, false).command, "cargo build -p alpha");
        assert_eq!(
            apply_release(&step, true).command,
            "cargo build -p alpha --release"
        );
    }

    #[test]
    fn release_reaches_the_process_that_runs() {
        let dir = scratch("release");
        // `printf '%s\n' "$@"`-style: the line echoes whatever extra
        // arguments it was given, so the appended flag is observable.
        let path = write_manifest(
            &dir,
            "nodes:\n  - id: alpha\n    path: /usr/bin/true\n    build: /bin/echo built\n",
        );
        let mut a = args(&dir, path);
        a.release = true;
        let mut out = Vec::new();
        let report = run(&mut out, &a).unwrap();
        assert!(report.is_success(), "{report:?}");
        assert!(report.steps[0].output.contains("--release"), "{report:?}");
    }

    #[test]
    fn json_output_carries_every_step_and_the_exit_code() {
        let dir = scratch("json");
        let path = write_manifest(&dir, TWO_NODES);
        let mut a = args(&dir, path);
        a.json = true;
        let mut out = Vec::new();
        let report = run(&mut out, &a).unwrap();
        let text = String::from_utf8(out).unwrap();
        let start = text.find('{').expect("a JSON object");
        let value: serde_json::Value = serde_json::from_str(&text[start..]).unwrap();
        assert_eq!(value["success"], true);
        assert_eq!(value["exit_code"], 0);
        assert_eq!(value["steps"].as_array().unwrap().len(), 2);
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn a_manifest_that_does_not_validate_is_refused() {
        let dir = scratch("invalid");
        let path = write_manifest(
            &dir,
            "nodes:\n  - id: a\n    path: /usr/bin/true\n    inputs:\n      in: ghost/out\n",
        );
        let error = run(&mut Vec::new(), &args(&dir, path)).unwrap_err();
        assert!(matches!(error, CliError::Validation(_)), "{error}");
    }

    #[test]
    fn a_manifest_with_no_build_lines_at_all_succeeds_trivially() {
        let dir = scratch("empty");
        let path = write_manifest(&dir, "nodes:\n  - id: a\n    path: /usr/bin/true\n");
        let mut out = Vec::new();
        let report = run(&mut out, &args(&dir, path)).unwrap();
        assert!(report.is_success());
        assert!(report.steps.is_empty());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("built 0 node(s)"), "{text}");
    }

    #[test]
    fn a_trailing_newline_is_added_exactly_once() {
        assert_eq!(ensure_newline(""), "");
        assert_eq!(ensure_newline("a"), "a\n");
        assert_eq!(ensure_newline("a\n"), "a\n");
    }
}
