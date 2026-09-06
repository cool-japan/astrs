//! Build steps — `build:` lines, run with the same hygiene as a node (§16).
//!
//! > *No shell by default: `build:`/`path:` exec directly (argv split by shlex
//! > rules).*
//!
//! A build line is not a node, but it *is* a process the daemon starts on a
//! user's behalf, so it gets exactly the same treatment: the scrubbed
//! environment, the shlex argv split, its own process group, captured output.
//! The one difference is that it runs to completion before anything is
//! spawned, and its output becomes the [`BuildReport`] the coordinator shows
//! rather than a log fan-out.
//!
//! ```text
//!   for each build step, in manifest order:
//!       scrub env → split argv → spawn → capture stdout+stderr → wait
//!           │                                                     │
//!           └── a non-zero exit stops the build ──────────────────┘
//! ```
//!
//! # Sequential on purpose
//!
//! Build steps run one after another, not in parallel. Two `cargo build`
//! invocations in the same workspace contend on the same target directory
//! lock; running them concurrently makes a build *slower* and its output
//! interleaved to the point of uselessness. A manifest that wants parallelism
//! has one build line that does it.
//!
//! # Examples
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use astrs_daemon::dataflow::{BuildStep, run_build};
//! use astrs_daemon::spawn::Spawner;
//! use astrs_wire::NodeId;
//!
//! let steps = vec![BuildStep {
//!     node: NodeId::new("camera")?,
//!     command: "cargo build --release -p camera".into(),
//!     working_dir: None,
//! }];
//!
//! let spawner = Spawner::new(std::env::current_dir()?);
//! let report = run_build(&spawner, &steps).await;
//! assert!(report.is_success() || report.failed_step().is_some());
//! # Ok(()) }
//! ```

use std::path::PathBuf;
use std::process::Stdio;

use astrs_wire::NodeId;

use crate::dataflow::plan::BuildStep;
use crate::error::{DaemonError, DaemonResult};
use crate::spawn::{CommandLine, Spawner};

/// The output of one build step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutcome {
    /// The node the step belongs to.
    pub node: NodeId,
    /// The command line that ran.
    pub command: String,
    /// Its exit code, or `None` if a signal ended it.
    pub exit_code: Option<i32>,
    /// Everything it wrote, both streams merged in arrival order per stream.
    pub output: String,
}

impl StepOutcome {
    /// Whether the step succeeded.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// The result of running every build step.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildReport {
    /// The steps that ran, in order, up to and including any failure.
    pub steps: Vec<StepOutcome>,
    /// The step that could not be started at all, if one could not.
    pub start_failure: Option<String>,
}

impl BuildReport {
    /// Whether every step succeeded and none failed to start.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.start_failure.is_none() && self.steps.iter().all(StepOutcome::is_success)
    }

    /// The first step that failed, if any did.
    #[must_use]
    pub fn failed_step(&self) -> Option<&StepOutcome> {
        self.steps.iter().find(|step| !step.is_success())
    }

    /// Everything every step wrote, in order.
    #[must_use]
    pub fn combined_output(&self) -> String {
        let mut out = String::new();
        for step in &self.steps {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&step.output);
        }
        out
    }

    /// Turns a failure into the crate's error type.
    ///
    /// # Errors
    ///
    /// [`DaemonError::BuildFailed`] naming the first failing step, or
    /// [`DaemonError::Manifest`] if a step could not be started.
    pub fn into_result(self) -> DaemonResult<Self> {
        if let Some(reason) = &self.start_failure {
            return Err(DaemonError::Manifest(reason.clone()));
        }
        match self.failed_step() {
            Some(step) => Err(DaemonError::BuildFailed {
                node: step.node.clone(),
                step: step.command.clone(),
                code: match step.exit_code {
                    Some(code) => format!("exit code {code}"),
                    None => "a signal".to_string(),
                },
            }),
            None => Ok(self),
        }
    }
}

/// Runs every build step in order, stopping at the first failure.
///
/// Never returns an error: a failed build is *data* (a [`BuildReport`] whose
/// output the operator needs to read), not an exception. Call
/// [`BuildReport::into_result`] to turn it into one where that is what the
/// caller wants.
pub async fn run_build(spawner: &Spawner, steps: &[BuildStep]) -> BuildReport {
    let mut report = BuildReport::default();
    for step in steps {
        match run_step(spawner, step).await {
            Ok(outcome) => {
                let failed = !outcome.is_success();
                report.steps.push(outcome);
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
    report
}

/// Runs one build step.
///
/// # Errors
///
/// - [`DaemonError::BadArgv`] if the command line cannot be split.
/// - [`DaemonError::Spawn`] if the process cannot be started.
/// - [`DaemonError::io`] if waiting on it fails.
pub async fn run_step(spawner: &Spawner, step: &BuildStep) -> DaemonResult<StepOutcome> {
    let command_line = CommandLine::parse(&step.command).map_err(|_| DaemonError::BadArgv {
        node: step.node.clone(),
        input: step.command.clone(),
    })?;

    let working_dir: PathBuf = match &step.working_dir {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            if dir.is_absolute() {
                dir
            } else {
                spawner.working_dir().join(dir)
            }
        }
        None => spawner.working_dir().to_path_buf(),
    };

    // The same scrubbed base a node would get: a build line that reads a
    // secret from the daemon's environment is exactly the leak §16 closes.
    let env = spawner.policy().scrub_process_env();

    let mut command = tokio::process::Command::new(command_line.resolved_program(&working_dir));
    command.args(command_line.args());
    command.current_dir(&working_dir);
    command.env_clear();
    command.envs(&env);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.process_group(0);

    let output = command
        .output()
        .await
        .map_err(|source| DaemonError::Spawn {
            node: step.node.clone(),
            program: command_line.program().to_string(),
            source,
        })?;

    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&stderr);
    }

    Ok(StepOutcome {
        node: step.node.clone(),
        command: step.command.clone(),
        exit_code: output.status.code(),
        output: text,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::BTreeMap;

    use super::*;

    fn node(name: &str) -> NodeId {
        NodeId::new(name).unwrap()
    }

    fn spawner() -> Spawner {
        Spawner::new(std::env::temp_dir()).with_inherited(BTreeMap::from([
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("SECRET".to_string(), "leaked".to_string()),
        ]))
    }

    fn step(command: &str) -> BuildStep {
        BuildStep {
            node: node("builder"),
            command: command.to_string(),
            working_dir: None,
        }
    }

    #[tokio::test]
    async fn a_successful_step_reports_its_output() {
        let outcome = run_step(&spawner(), &step("/bin/echo built ok"))
            .await
            .unwrap();
        assert!(outcome.is_success());
        assert_eq!(outcome.exit_code, Some(0));
        assert!(outcome.output.contains("built ok"), "{}", outcome.output);
        assert_eq!(outcome.node, node("builder"));
    }

    #[tokio::test]
    async fn a_failing_step_keeps_its_exit_code() {
        let outcome = run_step(&spawner(), &step("/bin/sh -c 'exit 3'"))
            .await
            .unwrap();
        assert!(!outcome.is_success());
        assert_eq!(outcome.exit_code, Some(3));
    }

    #[tokio::test]
    async fn standard_error_is_captured_too() {
        let outcome = run_step(&spawner(), &step("/bin/sh -c 'echo oops >&2; exit 1'"))
            .await
            .unwrap();
        assert!(outcome.output.contains("oops"), "{}", outcome.output);
    }

    #[tokio::test]
    async fn a_build_step_runs_with_the_scrubbed_environment() {
        let outcome = run_step(&spawner(), &step("/usr/bin/env")).await.unwrap();
        assert!(outcome.output.contains("PATH="), "{}", outcome.output);
        assert!(
            !outcome.output.contains("SECRET="),
            "the scrub applies to build lines too: {}",
            outcome.output
        );
    }

    #[tokio::test]
    async fn an_unsplittable_command_is_refused() {
        let error = run_step(&spawner(), &step(r#"/bin/echo "unterminated"#))
            .await
            .unwrap_err();
        assert!(matches!(error, DaemonError::BadArgv { .. }), "{error}");
    }

    #[tokio::test]
    async fn a_missing_program_is_a_spawn_error() {
        let error = run_step(&spawner(), &step("/nonexistent/astrs-builder"))
            .await
            .unwrap_err();
        assert!(matches!(error, DaemonError::Spawn { .. }), "{error}");
    }

    #[tokio::test]
    async fn steps_run_in_order_and_stop_at_the_first_failure() {
        let steps = vec![
            step("/bin/echo first"),
            step("/bin/sh -c 'exit 7'"),
            step("/bin/echo never"),
        ];
        let report = run_build(&spawner(), &steps).await;

        assert!(!report.is_success());
        assert_eq!(report.steps.len(), 2, "the third never ran");
        assert_eq!(report.failed_step().and_then(|s| s.exit_code), Some(7));
        assert!(report.combined_output().contains("first"));
        assert!(!report.combined_output().contains("never"));
    }

    #[tokio::test]
    async fn an_all_green_build_succeeds() {
        let steps = vec![step("/bin/echo a"), step("/bin/echo b")];
        let report = run_build(&spawner(), &steps).await;
        assert!(report.is_success());
        assert!(report.failed_step().is_none());
        assert_eq!(report.steps.len(), 2);
        let combined = report.combined_output();
        assert!(
            combined.contains('a') && combined.contains('b'),
            "{combined}"
        );
    }

    #[tokio::test]
    async fn an_empty_build_is_a_success() {
        let report = run_build(&spawner(), &[]).await;
        assert!(report.is_success());
        assert!(report.steps.is_empty());
        assert_eq!(report.combined_output(), "");
        assert!(report.into_result().is_ok());
    }

    #[tokio::test]
    async fn a_step_that_cannot_start_is_recorded_as_a_start_failure() {
        let report = run_build(&spawner(), &[step("/nonexistent/astrs-builder")]).await;
        assert!(!report.is_success());
        assert!(report.start_failure.is_some());
        assert!(report.steps.is_empty());
        assert!(matches!(
            report.into_result(),
            Err(DaemonError::Manifest(_))
        ));
    }

    #[tokio::test]
    async fn a_failed_build_converts_to_a_build_failed_error() {
        let report = run_build(&spawner(), &[step("/bin/sh -c 'exit 2'")]).await;
        match report.into_result() {
            Err(DaemonError::BuildFailed {
                node: got, code, ..
            }) => {
                assert_eq!(got, node("builder"));
                assert!(code.contains('2'), "{code}");
            }
            other => panic!("expected a build failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_relative_working_directory_resolves_against_the_dataflow_root() {
        let root = std::env::temp_dir().join(format!("astrs-build-{}", std::process::id()));
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        let spawner = Spawner::new(&root).with_inherited(BTreeMap::from([(
            "PATH".to_string(),
            "/usr/bin:/bin".into(),
        )]));
        let mut step = step("/bin/pwd");
        step.working_dir = Some("sub".into());

        let outcome = run_step(&spawner, &step).await.unwrap();
        assert!(outcome.is_success());
        assert!(outcome.output.contains("sub"), "{}", outcome.output);

        let _ = std::fs::remove_dir_all(&root);
    }
}
