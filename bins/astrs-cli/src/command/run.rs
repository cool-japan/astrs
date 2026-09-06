//! `astrs run` — the single-process mode, and this binary's flagship verb.
//!
//! > *`astrs run` — the single-process mode: CLI embeds an in-process daemon
//! > (and no coordinator socket), runs the whole graph under one supervisor
//! > with the orphan guard (parent-pid + process-group kill).* — blueprint
//! > §4.2
//!
//! ```text
//!   manifest ─► build lines (§16 scrubbed env, unless --skip-build)
//!                    │
//!                    ▼
//!         astrs_daemon::run_dataflow_with   ← the embedded daemon
//!            │              │            │
//!            │              │            └── cancel  ◄── SIGINT / SIGTERM
//!            │              └── report sink ─────────► terminal log stream
//!            ▼
//!       DataflowResult ─► exit code (§17: "exit code = result severity")
//! ```
//!
//! # What this module does *not* do
//!
//! It does not re-implement any part of the daemon. Building, spawning,
//! supervising, the restart budgets, the `SIGTERM`→`SIGKILL` finish ladder
//! and the orphan guard all live in `astrs-daemon` and are reached through
//! one call to [`astrs_daemon::run_dataflow_with`] — the function that
//! exists for exactly this caller. What is genuinely this module's own is
//! the *terminal experience*: turning the daemon's upward event stream into
//! readable, filtered, colored lines while the graph runs; translating two
//! kinds of interrupt into the daemon's own graceful-stop path; and turning
//! a [`astrs_wire::DataflowResult`] into a process exit code.
//!
//! # Ctrl-C
//!
//! The first `SIGINT` (or `SIGTERM`) asks every node to stop, exactly as
//! `astrs stop` would: the daemon's finish grace runs, then its escalation
//! ladder. The second abandons the wait after the grace elapses and reports
//! the run as incomplete — a user pressing Ctrl-C twice wants their shell
//! back, and the orphan guard (`ASTRS_RUN_PARENT_PID`, set for every child
//! by the daemon) plus per-node process groups are what stop that from
//! leaking processes.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use astrs_daemon::{RunOptions, run_dataflow_with};
use astrs_manifest::Manifest;
use astrs_wire::{DataflowResult, DataflowStatus, LogLevel, NodeExitCause, NodeId};
use tokio::sync::watch;

use crate::command::log_stream::{
    LogFilter, LogStyle, StreamItem, TerminalLogSink, prefix_width, render,
};
use crate::command::signals::Signals;
use crate::error::CliError;

/// The exit code for a run that finished with every node healthy.
pub const EXIT_OK: i32 = 0;

/// The exit code for a run in which at least one node failed.
pub const EXIT_FAILED: i32 = 1;

/// The exit code for a run that never reached a terminal state — the wait
/// was abandoned (a second Ctrl-C) or the daemon returned a still-running
/// result.
pub const EXIT_INCOMPLETE: i32 = 2;

/// How long a second interrupt waits for the daemon's own escalation ladder
/// before the wait is abandoned.
///
/// Long enough for `SIGTERM`→`SIGKILL` to have happened for a node that is
/// merely slow, short enough that a user who pressed Ctrl-C twice does not
/// wonder whether it worked.
pub const ABANDON_AFTER: Duration = Duration::from_secs(2);

/// `astrs run`'s arguments, already parsed and resolved.
#[derive(Debug, Clone)]
pub struct RunArgs {
    /// The manifest to run.
    pub manifest_path: PathBuf,
    /// Drive the timer wheel from [`Self::from_recording`]'s HLC stream
    /// instead of the wall clock (§14). Refused with
    /// [`CliError::DeterministicNeedsRecording`] when
    /// [`Self::from_recording`] is unset.
    pub deterministic: bool,
    /// The `.arec` recording [`Self::deterministic`] replays as its clock
    /// source. Every node the recording covers is rewritten to `path:
    /// dynamic` (stood down, never spawned) and fed from the recording
    /// instead — see `stand_down_recorded_producers`.
    pub from_recording: Option<PathBuf>,
    /// Paces [`Self::deterministic`]'s wall-clock walk through the
    /// recording; `None` replays as fast as the loop can. Never changes
    /// which messages are delivered or their stamps — only how long the
    /// run takes.
    pub speed: Option<f64>,
    /// Override the manifest's `exit_when_nodes_finish:`.
    pub exit_when_nodes_finish: bool,
    /// Skip the `build:` lines.
    pub skip_build: bool,
    /// Hide streamed output below this level.
    pub level: Option<LogLevel>,
    /// Where node paths resolve against; the manifest's directory by
    /// default.
    pub working_dir: Option<PathBuf>,
    /// Where the embedded daemon's socket lives.
    pub runtime_dir: Option<PathBuf>,
    /// A ceiling on the whole run.
    pub timeout: Option<Duration>,
    /// How long a stopping node has before `SIGTERM`.
    pub grace: Option<Duration>,
    /// Emit the result as JSON instead of a human summary.
    pub json: bool,
    /// Colorize the streamed lines.
    pub color: bool,
}

impl RunArgs {
    /// The arguments for running `manifest_path` with every default.
    #[must_use]
    pub fn new(manifest_path: impl Into<PathBuf>) -> Self {
        Self {
            manifest_path: manifest_path.into(),
            deterministic: false,
            from_recording: None,
            speed: None,
            exit_when_nodes_finish: false,
            skip_build: false,
            level: None,
            working_dir: None,
            runtime_dir: None,
            timeout: None,
            grace: None,
            json: false,
            color: false,
        }
    }
}

/// What one `astrs run` did.
#[derive(Debug, Clone)]
pub struct RunReport {
    /// The daemon's own verdict.
    pub result: DataflowResult,
    /// How many streamed items the terminal could not keep up with.
    pub dropped: u64,
    /// How many lines reached the terminal.
    pub printed: usize,
    /// Whether the wait was abandoned rather than completed.
    pub abandoned: bool,
}

impl RunReport {
    /// The process exit code this run should produce (blueprint §17: "exit
    /// code = `DataflowResult` severity").
    ///
    /// Three outcomes, not two: a run that *finished* cleanly, a run that
    /// *failed*, and a run whose verdict never became terminal at all. The
    /// third is not a failure of the graph — nothing is known about the
    /// graph — so it gets its own code rather than being folded into
    /// either neighbour, and a script can tell "my dataflow crashed" apart
    /// from "I gave up waiting for it".
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if self.abandoned {
            return EXIT_INCOMPLETE;
        }
        match self.result.status {
            DataflowStatus::Finished if !self.result.has_failures() => EXIT_OK,
            DataflowStatus::Finished | DataflowStatus::Failed => EXIT_FAILED,
            _ => EXIT_INCOMPLETE,
        }
    }

    /// The one-line human summary printed when the run ends.
    #[must_use]
    pub fn summary(&self) -> String {
        let nodes = self.result.node_results.len();
        let failed = self
            .result
            .node_results
            .values()
            .filter(|cause| cause.is_failure())
            .count();
        let mut text = format!(
            "dataflow {} {} ({nodes} node(s), {failed} failed)",
            self.result.dataflow,
            status_word(self.result.status)
        );
        if !self.result.message.is_empty() {
            text.push_str(&format!(": {}", self.result.message));
        }
        if self.dropped > 0 {
            text.push_str(&format!(
                "\n{} log line(s) were dropped because the terminal could not keep up",
                self.dropped
            ));
        }
        text
    }

    /// The `--json` form.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let nodes: serde_json::Map<String, serde_json::Value> = self
            .result
            .node_results
            .iter()
            .map(|(node, cause)| {
                (
                    node.as_str().to_owned(),
                    serde_json::json!({
                        "cause": cause.to_string(),
                        "failed": cause.is_failure(),
                    }),
                )
            })
            .collect();
        serde_json::json!({
            "dataflow": self.result.dataflow.to_string(),
            "status": status_word(self.result.status),
            "message": self.result.message,
            "failed": self.result.has_failures(),
            "abandoned": self.abandoned,
            "exit_code": self.exit_code(),
            "printed_lines": self.printed,
            "dropped_lines": self.dropped,
            "nodes": nodes,
        })
    }
}

/// A stable lowercase word per status, for both the human and JSON forms.
const fn status_word(status: DataflowStatus) -> &'static str {
    match status {
        DataflowStatus::Pending => "pending",
        DataflowStatus::Building => "building",
        DataflowStatus::Ready => "ready",
        DataflowStatus::Starting => "starting",
        DataflowStatus::Running => "running",
        DataflowStatus::Stopping => "stopping",
        DataflowStatus::Finished => "finished",
        DataflowStatus::Failed => "failed",
        // `DataflowStatus` is `#[non_exhaustive]`; an unknown status is
        // reported rather than mapped onto a neighbour it is not.
        _ => "unknown",
    }
}

/// Runs one dataflow to completion in this process, streaming its output to
/// `out`.
///
/// Builds its own multi-threaded tokio runtime: `main` is synchronous (every
/// other verb in this crate is), and a run needs real threads — the daemon's
/// merged event loop, the per-session actors, the per-child stdout readers
/// and this function's own printer all make progress concurrently.
///
/// # Errors
///
/// - [`CliError::DeterministicNeedsRecording`] if `--deterministic` is set
///   without [`RunArgs::from_recording`] — checked before anything is built
///   or spawned, so a user who asks for determinism never gets a
///   *non*-deterministic run they did not notice.
/// - [`CliError::Recording`] if [`RunArgs::from_recording`]'s file cannot be
///   opened.
/// - [`CliError::Manifest`] / [`CliError::Validation`] if the manifest is
///   not usable.
/// - [`CliError::Io`] if the tokio runtime cannot be built.
/// - [`CliError::Daemon`] for anything the embedded daemon refuses.
pub fn run(out: &mut dyn Write, args: &RunArgs) -> Result<RunReport, CliError> {
    if args.deterministic && args.from_recording.is_none() {
        return Err(CliError::DeterministicNeedsRecording);
    }

    let mut manifest = load_manifest(&args.manifest_path, args.exit_when_nodes_finish)?;
    if let Some(recording) = &args.from_recording {
        stand_down_recorded_producers(&mut manifest, recording)?;
    }
    let working_dir = args
        .working_dir
        .clone()
        .unwrap_or_else(|| manifest_dir(&args.manifest_path));
    let runtime_dir = crate::runtime_dir::runtime_dir(args.runtime_dir.as_deref());
    crate::runtime_dir::ensure_dir(&runtime_dir)?;

    let style = LogStyle {
        color: args.color,
        prefix_width: prefix_width(manifest.nodes.iter().map(|node| node.id.as_str())),
        elapsed: true,
    };
    let filter = LogFilter::new().with_min_level(args.level.unwrap_or(LogLevel::Trace));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|source| CliError::io(&args.manifest_path, source))?;

    let report = runtime.block_on(execute(
        out,
        &manifest,
        args,
        working_dir,
        runtime_dir,
        &filter,
        style,
    ))?;

    if args.json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&report.to_json()).unwrap_or_else(|_| "{}".to_owned())
        );
    } else {
        let _ = writeln!(out, "{}", report.summary());
    }
    Ok(report)
}

/// Reads and pre-validates the manifest, applying `--exit-when-nodes-finish`.
///
/// Validation happens here rather than being left to the daemon so the
/// failure a user sees for a broken manifest is the CLI's own typed one —
/// with the file path in it — rather than a daemon error about a plan.
fn load_manifest(path: &Path, exit_when_nodes_finish: bool) -> Result<Manifest, CliError> {
    let mut manifest = Manifest::from_yaml_file(path)?;
    manifest.validate()?;
    if exit_when_nodes_finish {
        manifest.exit_when_nodes_finish = true;
    }
    Ok(manifest)
}

/// Rewrites every node `recording` produced for to `path: dynamic` (§14).
///
/// A deterministic run's clock source doubles as *its* data: an entry the
/// daemon replays is only ever fanned out as a message when its producer is
/// a node this run stood down (`crate::server::replay`'s doc comment, in
/// `astrs-daemon`, names this exact rewrite). A node the recording never
/// produced for — `probe`, a tap, anything downstream — is left exactly as
/// the manifest wrote it and is spawned normally.
///
/// Runs on the already-validated, in-memory [`Manifest`] only; the file on
/// disk is never touched, and the rewrite is not re-validated — every shape
/// [`astrs_manifest::Node::path`] can already legally hold includes the
/// literal `"dynamic"` sentinel this substitutes.
///
/// # Errors
///
/// [`CliError::Recording`] if `recording` cannot be opened.
fn stand_down_recorded_producers(
    manifest: &mut Manifest,
    recording: &Path,
) -> Result<(), CliError> {
    let (reader, _recovery) = astrs_recording::Reader::open_or_recover(recording)?;
    let producers: std::collections::BTreeSet<&str> = reader
        .index()
        .iter()
        .map(|entry| entry.node.as_str())
        .collect();
    for node in &mut manifest.nodes {
        if producers.contains(node.id.as_str()) {
            node.path = Some(astrs_manifest::DYNAMIC_PATH_SENTINEL.to_owned());
        }
    }
    Ok(())
}

/// The directory a manifest's relative paths resolve against.
fn manifest_dir(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// The async half: start the run, stream it, and translate interrupts.
async fn execute(
    out: &mut dyn Write,
    manifest: &Manifest,
    args: &RunArgs,
    working_dir: PathBuf,
    runtime_dir: PathBuf,
    filter: &LogFilter,
    style: LogStyle,
) -> Result<RunReport, CliError> {
    let (sink, mut receiver) = TerminalLogSink::new();
    let sink = Arc::new(sink);
    let (cancel, cancel_rx) = watch::channel(false);

    let mut options = RunOptions::new()
        .with_working_dir(working_dir)
        .with_runtime_dir(runtime_dir)
        .with_build(!args.skip_build)
        .with_report_sink(sink.clone())
        .with_cancel(cancel_rx)
        .with_deterministic(args.deterministic)
        .with_replay_recording(args.from_recording.clone(), args.speed);
    if let Some(timeout) = args.timeout {
        options = options.with_timeout(timeout);
    }
    if let Some(grace) = args.grace {
        options = options.with_finish_grace(grace);
    }

    let started = Instant::now();
    let mut printed = 0usize;
    let mut interrupts = 0u32;
    let mut abandon: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;

    let mut signals = Signals::install();
    let run_future = run_dataflow_with(manifest, options);
    tokio::pin!(run_future);

    let outcome: Option<Result<DataflowResult, astrs_daemon::DaemonError>> = loop {
        tokio::select! {
            // Biased so a finished run is observed before another log line
            // is waited for: the loop must not sit in `recv()` while the
            // result is already available.
            biased;
            result = &mut run_future => break Some(result),
            () = wait_for(&mut abandon) => break None,
            Some(item) = receiver.recv() => {
                printed += print_one(out, &item, filter, style, started);
            }
            () = signals.next() => {
                interrupts += 1;
                let (level, message) = interrupt_notice(interrupts);
                printed += print_one(
                    out,
                    &StreamItem::Notice { level, node: None, message },
                    filter,
                    style,
                    started,
                );
                if interrupts == 1 {
                    // Exactly `astrs stop`'s path: ask, grace, escalate.
                    let _ = cancel.send(true);
                } else if abandon.is_none() {
                    abandon = Some(Box::pin(tokio::time::sleep(ABANDON_AFTER)));
                }
            }
        }
    };

    // Whatever was queued when the run ended is still news; the sink is not
    // dropped until this function returns, so `recv()` would block forever
    // here and `try_recv` is the right drain.
    while let Ok(item) = receiver.try_recv() {
        printed += print_one(out, &item, filter, style, started);
    }

    let (result, abandoned) = match outcome {
        Some(result) => (result?, false),
        None => (abandoned_result(manifest), true),
    };
    Ok(RunReport {
        result,
        dropped: sink.as_ref().dropped_count(),
        printed,
        abandoned,
    })
}

/// Resolves when the abandon timer (armed by a second interrupt) fires, and
/// never when there is none — so the `select!` arm is always safe to poll.
async fn wait_for(timer: &mut Option<std::pin::Pin<Box<tokio::time::Sleep>>>) {
    match timer.as_mut() {
        Some(sleep) => sleep.as_mut().await,
        None => std::future::pending().await,
    }
}

/// The notice one interrupt produces.
fn interrupt_notice(count: u32) -> (LogLevel, String) {
    if count == 1 {
        (
            LogLevel::Warn,
            "interrupted: asking every node to stop (press again to give up waiting)".to_owned(),
        )
    } else {
        (
            LogLevel::Warn,
            format!(
                "interrupted again: abandoning the wait in {:.0}s; children are process-group \
                 killed and carry the orphan guard",
                ABANDON_AFTER.as_secs_f64()
            ),
        )
    }
}

/// Prints one item if the filter accepts it, returning how many lines that
/// was (0 or 1) so the caller's counter needs no branch of its own.
fn print_one(
    out: &mut dyn Write,
    item: &StreamItem,
    filter: &LogFilter,
    style: LogStyle,
    started: Instant,
) -> usize {
    if !filter.accepts(item) {
        return 0;
    }
    if writeln!(out, "{}", render(item, style, started.elapsed())).is_err() {
        return 0;
    }
    let _ = out.flush();
    1
}

/// The verdict for a run whose wait was abandoned.
///
/// Deliberately not a lie about what the nodes did: every node is recorded
/// as [`NodeExitCause::Killed`] with the signal a process group receives,
/// and the status stays `Stopping` — the last thing actually known to be
/// true — so [`RunReport::exit_code`] reports it as incomplete rather than
/// as a clean finish or a graph failure.
fn abandoned_result(manifest: &Manifest) -> DataflowResult {
    let mut result = DataflowResult::new(
        astrs_wire::DataflowId::generate(),
        astrs_time::HlcTimestamp::new(0, 0),
    );
    result.status = DataflowStatus::Stopping;
    result.message = "the wait was abandoned after a second interrupt".to_owned();
    for node in &manifest.nodes {
        if let Ok(id) = NodeId::new(&node.id) {
            result.record(
                id,
                NodeExitCause::Killed {
                    reason: "the CLI abandoned the wait after a second interrupt".to_owned(),
                },
            );
        }
    }
    result
}

/// Reading a sink's drop counter without importing the trait at every call
/// site.
trait DroppedCount {
    /// How many items the sink could not accept.
    fn dropped_count(&self) -> u64;
}

impl DroppedCount for TerminalLogSink {
    fn dropped_count(&self) -> u64 {
        astrs_daemon::health::ReportSink::dropped(self)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use astrs_wire::DataflowId;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("astrs-cli-run-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_manifest(dir: &Path, yaml: &str) -> PathBuf {
        let path = dir.join("dataflow.yml");
        std::fs::write(&path, yaml).unwrap();
        path
    }

    fn report(status: DataflowStatus, failures: &[(&str, NodeExitCause)]) -> RunReport {
        let mut result = DataflowResult::new(
            DataflowId::from_u128(1),
            astrs_time::HlcTimestamp::new(1, 0),
        );
        result.status = status;
        for (node, cause) in failures {
            result.record(NodeId::new(*node).unwrap(), cause.clone());
        }
        RunReport {
            result,
            dropped: 0,
            printed: 0,
            abandoned: false,
        }
    }

    #[test]
    fn a_clean_finish_exits_zero() {
        let report = report(DataflowStatus::Finished, &[("a", NodeExitCause::Success)]);
        assert_eq!(report.exit_code(), EXIT_OK);
    }

    #[test]
    fn a_failed_node_exits_one_even_when_the_status_says_finished() {
        let report = report(
            DataflowStatus::Finished,
            &[("a", NodeExitCause::ExitCode { code: 3 })],
        );
        assert_eq!(report.exit_code(), EXIT_FAILED);
    }

    #[test]
    fn a_failed_dataflow_exits_one() {
        let report = report(
            DataflowStatus::Failed,
            &[("a", NodeExitCause::ExitCode { code: 3 })],
        );
        assert_eq!(report.exit_code(), EXIT_FAILED);
    }

    #[test]
    fn a_non_terminal_status_exits_two() {
        for status in [
            DataflowStatus::Pending,
            DataflowStatus::Building,
            DataflowStatus::Ready,
            DataflowStatus::Starting,
            DataflowStatus::Running,
            DataflowStatus::Stopping,
        ] {
            let report = report(status, &[]);
            assert_eq!(report.exit_code(), EXIT_INCOMPLETE, "{status:?}");
        }
    }

    #[test]
    fn an_abandoned_wait_exits_two_whatever_the_status_says() {
        let mut report = report(DataflowStatus::Finished, &[("a", NodeExitCause::Success)]);
        report.abandoned = true;
        assert_eq!(report.exit_code(), EXIT_INCOMPLETE);
    }

    #[test]
    fn the_summary_names_the_status_and_the_failure_count() {
        let report = report(
            DataflowStatus::Failed,
            &[
                ("a", NodeExitCause::Success),
                ("b", NodeExitCause::ExitCode { code: 1 }),
            ],
        );
        let text = report.summary();
        assert!(text.contains("failed"), "{text}");
        assert!(text.contains("2 node(s)"), "{text}");
        assert!(text.contains("1 failed"), "{text}");
    }

    #[test]
    fn the_summary_mentions_dropped_lines_only_when_there_were_some() {
        let mut report = report(DataflowStatus::Finished, &[]);
        assert!(!report.summary().contains("dropped"));
        report.dropped = 5;
        assert!(report.summary().contains("5 log line(s)"));
    }

    #[test]
    fn the_json_form_carries_every_node_and_the_exit_code() {
        let report = report(
            DataflowStatus::Failed,
            &[("cam", NodeExitCause::ExitCode { code: 2 })],
        );
        let json = report.to_json();
        assert_eq!(json["status"], "failed");
        assert_eq!(json["exit_code"], EXIT_FAILED);
        assert_eq!(json["nodes"]["cam"]["failed"], true);
    }

    #[test]
    fn deterministic_without_a_recording_is_refused_before_anything_runs() {
        let dir = scratch("deterministic");
        let path = write_manifest(&dir, "nodes:\n  - id: a\n    path: /usr/bin/true\n");
        let mut args = RunArgs::new(path);
        args.deterministic = true;
        let error = run(&mut Vec::new(), &args).unwrap_err();
        assert!(
            matches!(error, CliError::DeterministicNeedsRecording),
            "expected DeterministicNeedsRecording, got {error}"
        );
        assert!(error.to_string().contains("--from-recording"), "{error}");
    }

    #[test]
    fn a_manifest_that_does_not_parse_is_reported_by_the_cli() {
        let dir = scratch("bad-manifest");
        let path = write_manifest(&dir, "nodes: [oops");
        let error = run(&mut Vec::new(), &RunArgs::new(path)).unwrap_err();
        assert!(matches!(error, CliError::Manifest(_)), "{error}");
    }

    #[test]
    fn a_manifest_that_fails_validation_is_reported_by_the_cli() {
        let dir = scratch("invalid-manifest");
        // An input naming a producer that does not exist.
        let path = write_manifest(
            &dir,
            "nodes:\n  - id: a\n    path: /usr/bin/true\n    inputs:\n      in: ghost/out\n",
        );
        let error = run(&mut Vec::new(), &RunArgs::new(path)).unwrap_err();
        assert!(matches!(error, CliError::Validation(_)), "{error}");
    }

    #[test]
    fn the_manifest_directory_is_the_default_working_directory() {
        assert_eq!(
            manifest_dir(Path::new("/a/b/dataflow.yml")),
            PathBuf::from("/a/b")
        );
        assert_eq!(manifest_dir(Path::new("dataflow.yml")), PathBuf::from("."));
    }

    #[test]
    fn exit_when_nodes_finish_overrides_the_manifest() {
        let dir = scratch("exit-flag");
        let path = write_manifest(&dir, "nodes:\n  - id: a\n    path: /usr/bin/true\n");
        let manifest = load_manifest(&path, false).unwrap();
        assert!(!manifest.exit_when_nodes_finish);
        let manifest = load_manifest(&path, true).unwrap();
        assert!(manifest.exit_when_nodes_finish);
    }

    #[test]
    fn an_abandoned_result_records_every_node_as_killed() {
        let manifest =
            Manifest::from_yaml_str("nodes:\n  - id: a\n    path: /usr/bin/true\n").unwrap();
        let result = abandoned_result(&manifest);
        assert_eq!(result.status, DataflowStatus::Stopping);
        assert_eq!(result.node_results.len(), 1);
        assert!(result.has_failures());
    }

    #[test]
    fn a_trivial_graph_runs_to_a_clean_finish_and_exits_zero() {
        let dir = scratch("trivial");
        let path = write_manifest(
            &dir,
            "exit_when_nodes_finish: true\nnodes:\n  - id: a\n    path: /bin/sh\n    args: [\"-c\", \"echo hello-from-run\"]\n",
        );
        let mut args = RunArgs::new(path);
        args.skip_build = true;
        args.timeout = Some(Duration::from_secs(20));
        args.grace = Some(Duration::from_millis(200));
        args.runtime_dir = Some(dir.clone());
        args.working_dir = Some(dir);

        let mut out: Vec<u8> = Vec::new();
        let report = run(&mut out, &args).expect("a report");
        let text = String::from_utf8(out).unwrap();
        assert_eq!(report.exit_code(), EXIT_OK, "{text}");
        assert!(
            text.contains("hello-from-run"),
            "the node's own output must reach the terminal: {text}"
        );
        assert!(text.contains("finished"), "{text}");
    }

    #[test]
    fn a_failing_build_line_stops_the_run_before_spawning() {
        let dir = scratch("bad-build");
        let path = write_manifest(
            &dir,
            "nodes:\n  - id: a\n    path: /usr/bin/true\n    build: /bin/sh -c 'exit 4'\n",
        );
        let mut args = RunArgs::new(path);
        args.timeout = Some(Duration::from_secs(20));
        args.runtime_dir = Some(dir.clone());
        args.working_dir = Some(dir);
        let error = run(&mut Vec::new(), &args).unwrap_err();
        assert!(matches!(error, CliError::Daemon(_)), "{error}");
    }

    #[test]
    fn skip_build_reaches_the_spawn_a_failing_build_would_have_blocked() {
        let dir = scratch("skip-build");
        let path = write_manifest(
            &dir,
            "exit_when_nodes_finish: true\nnodes:\n  - id: a\n    path: /usr/bin/true\n    build: /bin/sh -c 'exit 4'\n",
        );
        let mut args = RunArgs::new(path);
        args.skip_build = true;
        args.timeout = Some(Duration::from_secs(20));
        args.grace = Some(Duration::from_millis(200));
        args.runtime_dir = Some(dir.clone());
        args.working_dir = Some(dir);
        // `/usr/bin/true` never registers, so the spawn deadline is what
        // ends this run — the point is only that the *build* did not.
        let report = run(&mut Vec::new(), &args).expect("a report");
        assert_eq!(report.result.node_results.len(), 1);
    }
}
