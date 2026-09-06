//! `astrs validate [--prove]` (blueprint §17).
//!
//! Runs the full pipeline a manifest goes through before anything is
//! spawned — parse, structural validation, module expansion, structural
//! validation again on the flattened result, graph construction, graph
//! diagnostics — and reports **every** observation found along the way as
//! one flat, ordered [`CliDiagnostic`] list, stopping early only when a
//! later stage would be meaningless (there is nothing to expand from a
//! manifest that never parsed).
//!
//! Exit code convention (blueprint §17): `0` clean, `1` warnings only,
//! `2` any error — see [`crate::diagnostic::exit_code_for`].
//!
//! # `--prove`
//!
//! With `--prove`, the graph is additionally handed to `astrs-verify`,
//! which discharges the obligations of blueprint §15 through an SMT solver
//! and returns a [`VerificationReport`]. Its results fold into the same
//! exit-code scale rather than inventing a second one:
//!
//! - a **refuted** obligation is an [`Severity::Error`] diagnostic, so the
//!   command exits `2` — a graph proven to deadlock is not a warning;
//! - an obligation that reached **no verdict** (a missing service time, an
//!   undecided solver run) is a [`Severity::Warning`], so the command exits
//!   `1`. "Nothing failed" and "everything was proved" are different
//!   claims, and the exit code says which one this run earned;
//! - a build **without the `verify` feature** still encodes and prints
//!   every obligation, then returns [`CliError::ProofUnavailable`]
//!   ([`crate::error::EXIT_UNAVAILABLE`], `69`), so a script can tell a
//!   missing solver from a failed proof by exit code alone.

use std::io::Write;
use std::path::{Path, PathBuf};

use astrs_graph::{DataflowGraph, Severity};
use astrs_manifest::Manifest;
use astrs_manifest::expand::FsModuleLoader;
use astrs_verify::{
    Profile, ProveOptions, RenderOptions, VerificationReport, prove, render_human as render_proof,
};
use serde::Serialize;

use crate::diagnostic::{CliDiagnostic, exit_code_for, render_human};
use crate::error::CliError;

/// Arguments for `astrs validate`.
#[derive(Debug, Clone)]
pub struct ValidateArgs {
    /// The manifest file to validate.
    pub manifest_path: PathBuf,
    /// Prove the graph's obligations — deadlock freedom, queue
    /// boundedness, rate consistency, latency budgets and type-rule
    /// consistency (blueprint §15) — through the SMT solver.
    pub prove: bool,
    /// An optional verification profile supplying per-node service times
    /// and named end-to-end latency budgets. Only consulted when
    /// [`Self::prove`] is set.
    pub profile: Option<PathBuf>,
    /// Emit the report as JSON instead of human-readable text.
    pub json: bool,
    /// Colorize human-readable output with plain ANSI SGR codes.
    pub color: bool,
}

/// The result of validating one manifest.
#[derive(Debug, Clone, Serialize)]
pub struct ValidateReport {
    /// The path that was validated, rendered with [`Path::display`].
    pub manifest_path: String,
    /// Every observation found, in pipeline order.
    pub diagnostics: Vec<CliDiagnostic>,
    /// The full proof report, when `--prove` reached the proving stage.
    ///
    /// Nested rather than flattened into [`Self::diagnostics`] on purpose:
    /// the counterexamples, the per-obligation SMT-LIB2 systems and the
    /// caveats are the part of a proof worth having, and folding them into
    /// one-line strings would throw exactly that away.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof: Option<VerificationReport>,
}

impl ValidateReport {
    /// This report's process exit code — see [`exit_code_for`].
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        exit_code_for(&self.diagnostics)
    }
}

/// Run `astrs validate`, writing a human or JSON report to `out`.
///
/// Every problem with the manifest itself becomes a [`CliDiagnostic`] in
/// the rendered report rather than an `Err` — see this module's docs for
/// why a diagnostic tool reports rather than aborts. The proof results
/// join the same list on the same terms; see this module's header for how
/// they map onto the exit-code scale.
///
/// # Errors
///
/// Returns [`CliError::Io`] if writing to `out` fails, or
/// [`CliError::ProofUnavailable`] if `--prove` reached the proving stage on
/// a build without the `verify` feature. In the latter case the report —
/// including every encoded obligation — has already been written to `out`.
/// A `--prove` run that never got that far (an unparseable manifest, a
/// profile that does not match) reports *that* instead, on the ordinary
/// exit-code scale.
pub fn run(out: &mut dyn Write, args: &ValidateArgs) -> Result<ValidateReport, CliError> {
    let (mut diagnostics, graph) = collect_diagnostics(args);
    let proof = if args.prove {
        run_proof(args, graph.as_ref(), &mut diagnostics)
    } else {
        None
    };
    let report = ValidateReport {
        manifest_path: args.manifest_path.display().to_string(),
        diagnostics,
        proof,
    };
    render(out, args, &report)?;
    // Only once the obligations were actually *encoded*: if the proof never
    // got that far — the manifest did not parse, the profile did not match —
    // the real problem is upstream, and reporting a missing solver instead
    // would bury it behind exit code 69.
    if report.proof.is_some() && !astrs_verify::solver_available() {
        return Err(CliError::ProofUnavailable);
    }
    Ok(report)
}

/// Discharge the graph obligations, folding their results into
/// `diagnostics` and returning the full report.
///
/// Returns `None` when the pipeline never produced a graph (the manifest
/// did not get that far) or when the proof run could not be set up — both
/// already recorded as diagnostics by the time this returns.
fn run_proof(
    args: &ValidateArgs,
    graph: Option<&DataflowGraph>,
    diagnostics: &mut Vec<CliDiagnostic>,
) -> Option<VerificationReport> {
    let Some(graph) = graph else {
        diagnostics.push(CliDiagnostic::prove_error(
            "--prove needs a graph, and this manifest did not produce one",
        ));
        return None;
    };

    let profile = match &args.profile {
        Some(path) => match Profile::from_file(path) {
            Ok(profile) => profile,
            Err(err) => {
                diagnostics.push(CliDiagnostic::prove_error(err.to_string()));
                return None;
            }
        },
        None => Profile::empty(),
    };
    // Report *every* mismatch between the profile and the graph, not only
    // the first one `prove` would stop at: a profile is hand-written
    // configuration, and fixing it one error per run is a poor trade.
    let profile_errors = profile.validate_against(graph);
    if !profile_errors.is_empty() {
        for error in &profile_errors {
            diagnostics.push(CliDiagnostic::prove_error(error.to_string()));
        }
        return None;
    }

    let report = match prove(graph, &ProveOptions::with_profile(profile)) {
        Ok(report) => report,
        Err(err) => {
            diagnostics.push(CliDiagnostic::prove_error(err.to_string()));
            return None;
        }
    };

    for outcome in report.violations() {
        let headline = outcome
            .discharge
            .counterexample()
            .map_or("obligation refuted", |c| c.headline.as_str());
        diagnostics.push(CliDiagnostic::prove_violation(
            &outcome.obligation.heading(),
            headline,
        ));
    }
    if !astrs_verify::solver_available() {
        diagnostics.push(CliDiagnostic::prove_unavailable());
    } else if !report.everything_discharged() {
        let summary = report.summary();
        diagnostics.push(CliDiagnostic::prove_incomplete(
            report.gap_count().saturating_sub(summary.inconclusive),
            summary.inconclusive,
        ));
    }
    Some(report)
}

/// Run the parse → validate → expand → validate → graph → diagnostics
/// pipeline, stopping early after any stage that leaves nothing
/// meaningful for the next one to do.
fn collect_diagnostics(args: &ValidateArgs) -> (Vec<CliDiagnostic>, Option<DataflowGraph>) {
    let mut diagnostics = Vec::new();

    let content = match std::fs::read_to_string(&args.manifest_path) {
        Ok(content) => content,
        Err(err) => {
            diagnostics.push(CliDiagnostic::io(format!(
                "failed to read `{}`: {err}",
                args.manifest_path.display()
            )));
            return (diagnostics, None);
        }
    };

    let manifest = match Manifest::from_yaml_str(&content) {
        Ok(manifest) => manifest,
        Err(err) => {
            diagnostics.push(CliDiagnostic::parse(&err));
            return (diagnostics, None);
        }
    };

    if let Err(errors) = manifest.validate() {
        diagnostics.extend(errors.errors().iter().map(CliDiagnostic::structural));
        return (diagnostics, None);
    }

    let base_dir = base_dir_of(&args.manifest_path);
    let loader = FsModuleLoader;
    let expanded = match manifest.expand(&base_dir, &loader) {
        Ok(expanded) => expanded,
        Err(err) => {
            diagnostics.push(CliDiagnostic::expand(&err));
            return (diagnostics, None);
        }
    };

    // Module expansion can produce a shape `validate` has not already
    // checked (the flattened id namespace, boundary types transplanted
    // onto internal nodes) — see `astrs_manifest::expand`'s own
    // recommendation to re-validate the flattened result.
    if let Err(errors) = expanded.validate() {
        diagnostics.extend(errors.errors().iter().map(CliDiagnostic::structural));
        return (diagnostics, None);
    }

    match DataflowGraph::from_manifest(&expanded) {
        Ok((graph, construction_diagnostics)) => {
            diagnostics.extend(construction_diagnostics.iter().map(CliDiagnostic::graph));
            diagnostics.extend(graph.diagnostics().iter().map(CliDiagnostic::graph));
            (diagnostics, Some(graph))
        }
        Err(err) => {
            diagnostics.push(CliDiagnostic::graph_build_error(&err));
            (diagnostics, None)
        }
    }
}

/// The directory `manifest_path`'s own `module:` references resolve
/// against — its parent directory, or `.` when `manifest_path` is a bare
/// filename with no parent segment at all (never the process's cwd
/// implicitly; `.` here is that same cwd made an explicit, documented
/// choice for the common "manifest lives in the invocation directory"
/// case).
fn base_dir_of(manifest_path: &Path) -> PathBuf {
    match manifest_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn render(
    out: &mut dyn Write,
    args: &ValidateArgs,
    report: &ValidateReport,
) -> Result<(), CliError> {
    if args.json {
        let json = serde_json::to_string_pretty(report).unwrap_or_else(|err| {
            format!("{{\"error\": \"failed to serialize validate report: {err}\"}}")
        });
        writeln!(out, "{json}").map_err(|e| CliError::io("<output>", e))?;
        return Ok(());
    }

    if report.diagnostics.is_empty() {
        writeln!(out, "{}: clean", report.manifest_path)
            .map_err(|e| CliError::io("<output>", e))?;
    } else {
        writeln!(out, "{}", render_human(&report.diagnostics, args.color))
            .map_err(|e| CliError::io("<output>", e))?;
        let error_count = report
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count();
        let warning_count = report
            .diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .count();
        writeln!(out, "{error_count} error(s), {warning_count} warning(s)")
            .map_err(|e| CliError::io("<output>", e))?;
    }

    // The proof report comes last and in full: the one-line diagnostics
    // above say *that* an obligation failed, and this says why, with the
    // counterexample.
    if let Some(proof) = &report.proof {
        writeln!(out).map_err(|e| CliError::io("<output>", e))?;
        write!(
            out,
            "{}",
            render_proof(
                proof,
                &RenderOptions {
                    color: args.color,
                    show_systems: false,
                }
            )
        )
        .map_err(|e| CliError::io("<output>", e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn write_temp(name: &str, content: &str) -> PathBuf {
        // A monotonic counter, not just `(pid, name)`, disambiguates the
        // directory -- see `command::graph`'s own test helper for the
        // observed race this guards against if a future test ever reuses
        // an existing `name` under cargo's multi-threaded test runner.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "astrs-cli-validate-test-{}-{name}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("manifest.yaml");
        std::fs::write(&path, content).unwrap();
        path
    }

    fn args(path: PathBuf) -> ValidateArgs {
        ValidateArgs {
            manifest_path: path,
            prove: false,
            profile: None,
            json: false,
            color: false,
        }
    }

    /// The clean two-node pipeline used by the `--prove` tests: a timer
    /// drives it, so nothing can starve.
    const LIVE: &str = "\
nodes:
  - id: camera
    path: ./camera
    inputs:
      tick:
        source: astrs/timer/hz/10
        queue_size: 1
    outputs: [frames]
  - id: sink
    path: ./sink
    inputs:
      frames:
        source: camera/frames
        queue_size: 1
";

    /// Two nodes waiting on each other, with nothing to start them.
    #[cfg(feature = "verify")]
    const DEADLOCKED: &str = "\
nodes:
  - id: a
    path: ./a
    inputs: { i: b/out }
    outputs: [out]
  - id: b
    path: ./b
    inputs: { i: a/out }
    outputs: [out]
";

    #[test]
    fn missing_file_is_an_io_diagnostic_with_exit_code_two() {
        let mut out = Vec::new();
        let a = args(std::env::temp_dir().join("astrs-cli-does-not-exist.yaml"));
        let report = run(&mut out, &a).unwrap();
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(report.diagnostics[0].source, crate::diagnostic::Source::Io);
    }

    #[test]
    fn malformed_yaml_is_a_parse_diagnostic() {
        let path = write_temp("malformed", "nodes: [");
        let mut out = Vec::new();
        let report = run(&mut out, &args(path)).unwrap();
        assert_eq!(report.exit_code(), 2);
        assert_eq!(
            report.diagnostics[0].source,
            crate::diagnostic::Source::Parse
        );
    }

    #[test]
    fn structural_violation_is_reported() {
        let path = write_temp(
            "dup-id",
            "nodes:\n  - id: x\n    path: ./x\n  - id: x\n    path: ./y\n",
        );
        let mut out = Vec::new();
        let report = run(&mut out, &args(path)).unwrap();
        assert_eq!(report.exit_code(), 2);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.source == crate::diagnostic::Source::Structural)
        );
    }

    #[test]
    fn clean_manifest_has_no_diagnostics_and_exit_code_zero() {
        let path = write_temp(
            "clean",
            "nodes:\n  - id: camera\n    path: ./camera\n    outputs: [frames]\n  - id: sink\n    path: ./sink\n    inputs:\n      frames: camera/frames\n",
        );
        let mut out = Vec::new();
        let report = run(&mut out, &args(path)).unwrap();
        assert_eq!(
            report.diagnostics.len(),
            0,
            "diagnostics: {:?}",
            report.diagnostics
        );
        assert_eq!(report.exit_code(), 0);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("clean"));
    }

    #[test]
    fn unconsumed_output_is_an_info_diagnostic_with_exit_code_zero() {
        // `UnconsumedOutput` is `Severity::Info` (it does not, by itself,
        // make the graph unsafe to run) -- reported, but does not fail
        // `validate`.
        let path = write_temp(
            "unconsumed",
            "nodes:\n  - id: camera\n    path: ./camera\n    outputs: [frames]\n",
        );
        let mut out = Vec::new();
        let report = run(&mut out, &args(path)).unwrap();
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(report.diagnostics[0].severity, astrs_graph::Severity::Info);
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn a_plain_cycle_is_a_warning_with_exit_code_one() {
        let path = write_temp(
            "cycle",
            "nodes:\n  - id: a\n    path: ./a\n    outputs: [out]\n    inputs:\n      i: b/out\n  - id: b\n    path: ./b\n    outputs: [out]\n    inputs:\n      i: a/out\n",
        );
        let mut out = Vec::new();
        let report = run(&mut out, &args(path)).unwrap();
        assert_eq!(report.exit_code(), 1);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.severity == astrs_graph::Severity::Warning)
        );
    }

    #[test]
    fn collect_diagnostics_hands_the_graph_back_for_proving() {
        let a = args(write_temp("graph-out", LIVE));
        let (diagnostics, graph) = collect_diagnostics(&a);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(graph.map(|g| g.node_count()), Some(2));
    }

    #[test]
    fn a_manifest_that_never_became_a_graph_cannot_be_proved() {
        let mut a = args(write_temp("no-graph", "nodes: ["));
        a.prove = true;
        let mut out = Vec::new();
        let report = run(&mut out, &a).expect("a parse failure is reported, not raised");
        assert!(report.proof.is_none());
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.source == crate::diagnostic::Source::Prove)
        );
    }

    #[test]
    fn a_profile_that_does_not_match_the_graph_reports_every_mismatch() {
        let dir = write_temp("bad-profile", LIVE);
        let profile = dir.with_file_name("verify.yaml");
        std::fs::write(
            &profile,
            "nodes:\n  ghost:\n    wcet: 0.001\n  phantom:\n    wcet: 0.002\n",
        )
        .unwrap();
        let mut a = args(dir);
        a.prove = true;
        a.profile = Some(profile);
        let mut out = Vec::new();
        let report = run(&mut out, &a).expect("a profile failure is reported, not raised");
        assert!(report.proof.is_none());
        let prove_errors = report
            .diagnostics
            .iter()
            .filter(|d| d.source == crate::diagnostic::Source::Prove)
            .count();
        assert_eq!(prove_errors, 2, "both mismatches must be reported at once");
    }

    #[test]
    fn a_missing_profile_file_is_reported_not_ignored() {
        let mut a = args(write_temp("missing-profile", LIVE));
        a.prove = true;
        a.profile = Some(std::env::temp_dir().join("astrs-cli-no-such-profile.yaml"));
        let mut out = Vec::new();
        let report = run(&mut out, &a).expect("a missing profile is reported, not raised");
        assert!(report.proof.is_none());
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.message.contains("verification profile"))
        );
    }

    #[test]
    #[cfg(not(feature = "verify"))]
    fn without_the_feature_prove_encodes_and_then_reports_no_solver() {
        let mut a = args(write_temp("prove-nofeature", LIVE));
        a.prove = true;
        let mut out = Vec::new();
        let err = run(&mut out, &a).unwrap_err();
        assert!(matches!(err, CliError::ProofUnavailable), "{err:?}");
        assert_eq!(err.exit_code(), crate::error::EXIT_UNAVAILABLE);
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("--features verify"), "{printed}");
        assert!(printed.contains("graph proofs"), "{printed}");
    }

    #[test]
    #[cfg(feature = "verify")]
    fn a_live_graph_without_a_profile_proves_clean_but_exits_one() {
        let mut a = args(write_temp("prove-clean", LIVE));
        a.prove = true;
        let mut out = Vec::new();
        let report = run(&mut out, &a).unwrap();
        let proof = report.proof.as_ref().expect("a proof was run");
        assert!(!proof.has_violations());
        // Without a profile, queue boundedness has no service time, so the
        // run is honest rather than clean: exit 1, not 0.
        assert_eq!(report.exit_code(), 1);
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("no obligation was violated"), "{printed}");
    }

    #[test]
    #[cfg(feature = "verify")]
    fn a_deadlocking_graph_is_an_error_with_exit_code_two() {
        let mut a = args(write_temp("prove-deadlock", DEADLOCKED));
        a.prove = true;
        let mut out = Vec::new();
        let report = run(&mut out, &a).unwrap();
        assert_eq!(report.exit_code(), 2);
        let proof = report.proof.as_ref().expect("a proof was run");
        assert!(proof.has_violations());
        assert!(
            report.diagnostics.iter().any(|d| {
                d.source == crate::diagnostic::Source::Prove
                    && d.severity == astrs_graph::Severity::Error
                    && d.message.contains("deadlock freedom")
            }),
            "{:?}",
            report.diagnostics
        );
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("can never fire"), "{printed}");
        assert!(printed.contains("fix:"), "{printed}");
    }

    #[test]
    #[cfg(feature = "verify")]
    fn a_full_profile_lets_the_run_reach_a_verdict_everywhere() {
        let manifest = write_temp("prove-profiled", LIVE);
        let profile = manifest.with_file_name("verify.yaml");
        std::fs::write(
            &profile,
            "nodes:\n  camera:\n    wcet: 0.001\n  sink:\n    wcet: 0.001\n",
        )
        .unwrap();
        let mut a = args(manifest);
        a.prove = true;
        a.profile = Some(profile);
        let mut out = Vec::new();
        let report = run(&mut out, &a).unwrap();
        let proof = report.proof.as_ref().expect("a proof was run");
        assert!(proof.everything_discharged(), "{:?}", proof.summary());
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn prove_output_is_appended_after_the_diagnostics() {
        let mut a = args(write_temp("prove-order", LIVE));
        a.prove = true;
        let mut out = Vec::new();
        let _ = run(&mut out, &a);
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("graph proofs"), "{printed}");
    }

    #[test]
    fn prove_json_nests_the_whole_report() {
        let mut a = args(write_temp("prove-json", LIVE));
        a.prove = true;
        a.json = true;
        let mut out = Vec::new();
        let _ = run(&mut out, &a);
        let text = String::from_utf8(out).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(value["proof"]["obligations"].is_array(), "{text}");
        assert!(value["proof"]["window_seconds"].is_number(), "{text}");
    }

    #[test]
    fn json_output_is_valid_json_and_contains_the_manifest_path() {
        let path = write_temp("json", "nodes:\n  - id: solo\n    path: ./solo\n");
        let mut out = Vec::new();
        let mut a = args(path.clone());
        a.json = true;
        let report = run(&mut out, &a).unwrap();
        assert_eq!(report.exit_code(), 0);
        let text = String::from_utf8(out).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["manifest_path"].as_str(),
            Some(path.display().to_string().as_str())
        );
    }

    #[test]
    fn module_that_fails_to_expand_is_an_expand_diagnostic() {
        let path = write_temp(
            "bad-module",
            "nodes:\n  - id: m\n    module: ./missing.yaml\n",
        );
        let mut out = Vec::new();
        let report = run(&mut out, &args(path)).unwrap();
        assert_eq!(report.exit_code(), 2);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.source == crate::diagnostic::Source::Expand)
        );
    }

    #[test]
    fn base_dir_of_bare_filename_is_dot() {
        assert_eq!(base_dir_of(Path::new("manifest.yaml")), PathBuf::from("."));
    }

    #[test]
    fn base_dir_of_nested_path_is_its_parent() {
        assert_eq!(
            base_dir_of(Path::new("/graphs/sub/manifest.yaml")),
            PathBuf::from("/graphs/sub")
        );
    }
}
