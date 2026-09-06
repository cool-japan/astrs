//! [`CliDiagnostic`]: the one flat, orderable diagnostic shape
//! `astrs validate` renders, unifying four different upstream types
//! (an I/O failure, [`astrs_manifest::ManifestError`],
//! [`astrs_manifest::ValidationError`], [`astrs_manifest::expand::ExpandError`]
//! and [`astrs_graph::Diagnostic`]) that otherwise share no common trait
//! beyond `Display` — blueprint §17's "human diagnostics (path, severity,
//! colored)" and `--json` need one list to print, not five differently
//! shaped error types each rendered by hand.
//!
//! Every failure mode `astrs validate` can encounter — the file not
//! existing, the YAML not parsing, a structural violation, a module that
//! fails to expand, a type mismatch, an unconsumed output — becomes one
//! [`CliDiagnostic`] rather than aborting the command outright. This is
//! deliberate: a diagnostic tool's job is to tell the user everything
//! that's wrong in one pass, not to stop at the first problem (the same
//! reasoning [`astrs_manifest::ValidationErrors`] itself documents).

use std::fmt;

pub use astrs_graph::Severity;

/// Which stage of the `validate` pipeline produced a [`CliDiagnostic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The manifest file could not be read from disk.
    Io,
    /// The manifest did not parse as YAML.
    Parse,
    /// [`astrs_manifest::Manifest::validate`] reported a structural
    /// violation.
    Structural,
    /// [`astrs_manifest::expand::expand`] failed.
    Expand,
    /// [`astrs_graph::DataflowGraph::diagnostics`] (or its construction
    /// pass) reported an observation.
    Graph,
    /// `--prove` was requested but formal verification is not enabled in
    /// this build.
    Prove,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Io => "io",
            Self::Parse => "parse",
            Self::Structural => "structural",
            Self::Expand => "expand",
            Self::Graph => "graph",
            Self::Prove => "prove",
        })
    }
}

/// One unified validation observation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CliDiagnostic {
    /// How serious this observation is.
    pub severity: Severity,
    /// Which stage produced it.
    pub source: Source,
    /// A document path (`nodes[3].inputs.frames`) when the underlying
    /// observation is anchored to one, e.g. every
    /// [`Source::Structural`] entry. `None` for whole-file observations
    /// ([`Source::Io`], [`Source::Parse`] without a known location,
    /// [`Source::Prove`]) and for [`Source::Graph`] entries (that crate's
    /// [`astrs_graph::Diagnostic`] identifies nodes/edges structurally
    /// rather than via a document path; see its own `Display`, folded
    /// into [`Self::message`] instead).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A human-readable rendering of the observation.
    pub message: String,
}

impl CliDiagnostic {
    /// The manifest file could not be read.
    #[must_use]
    pub fn io(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            source: Source::Io,
            path: None,
            message: message.into(),
        }
    }

    /// The manifest did not parse as YAML.
    #[must_use]
    pub fn parse(err: &astrs_manifest::ManifestError) -> Self {
        let path = match (err.line(), err.column()) {
            (Some(line), Some(column)) => Some(format!("line {line}, column {column}")),
            _ => None,
        };
        Self {
            severity: Severity::Error,
            source: Source::Parse,
            path,
            message: err.to_string(),
        }
    }

    /// One structural violation from [`astrs_manifest::Manifest::validate`].
    #[must_use]
    pub fn structural(err: &astrs_manifest::ValidationError) -> Self {
        Self {
            severity: Severity::Error,
            source: Source::Structural,
            path: Some(err.path.clone()),
            message: err.kind.to_string(),
        }
    }

    /// Module expansion failed.
    #[must_use]
    pub fn expand(err: &astrs_manifest::expand::ExpandError) -> Self {
        Self {
            severity: Severity::Error,
            source: Source::Expand,
            path: None,
            message: err.to_string(),
        }
    }

    /// [`astrs_graph::DataflowGraph::from_manifest`] rejected the
    /// manifest outright.
    ///
    /// Reachable in practice only when the manifest skipped
    /// [`astrs_manifest::Manifest::validate`] — see that function's own
    /// docs — which `validate`'s pipeline never does; kept as a
    /// diagnostic rather than an assumed-unreachable panic so a future
    /// regression in that pipeline degrades to a reported error instead
    /// of a crash.
    #[must_use]
    pub fn graph_build_error(err: &astrs_graph::GraphBuildError) -> Self {
        Self {
            severity: Severity::Error,
            source: Source::Graph,
            path: None,
            message: err.to_string(),
        }
    }

    /// One observation from [`astrs_graph::DataflowGraph::diagnostics`] (or
    /// its construction pass).
    #[must_use]
    pub fn graph(diag: &astrs_graph::Diagnostic) -> Self {
        Self {
            severity: diag.severity,
            source: Source::Graph,
            path: None,
            message: diag.to_string(),
        }
    }

    /// `--prove` was requested on a build without the `verify` feature.
    ///
    /// A warning rather than an error: the obligations were still encoded
    /// and printed, and the command reports the missing solver through
    /// [`crate::error::CliError::ProofUnavailable`]'s exit code rather
    /// than by inflating the diagnostic scale.
    #[must_use]
    pub fn prove_unavailable() -> Self {
        Self {
            severity: Severity::Warning,
            source: Source::Prove,
            path: None,
            message: "--prove encoded every obligation but this build has no solver; \
                      rebuild with `--features verify` to discharge them"
                .to_string(),
        }
    }

    /// One refuted obligation.
    #[must_use]
    pub fn prove_violation(heading: &str, headline: &str) -> Self {
        Self {
            severity: Severity::Error,
            source: Source::Prove,
            path: None,
            message: format!("{heading}: {headline}"),
        }
    }

    /// Some obligation left a hole in the proof, so the run rules out what
    /// it lists and no more.
    ///
    /// `missing_declaration` counts only *gaps* — obligations that needed a
    /// fact nobody supplied. An obligation with nothing to prove (no
    /// declared budget, no typed edge) is vacuously satisfied and is
    /// deliberately not counted here, even though the proof report lists it
    /// under "not attempted".
    #[must_use]
    pub fn prove_incomplete(missing_declaration: usize, inconclusive: usize) -> Self {
        Self {
            severity: Severity::Warning,
            source: Source::Prove,
            path: None,
            message: format!(
                "{missing_declaration} obligation(s) lack a declaration to decide them and \
                 {inconclusive} were left undecided; this proof rules out what it lists, and no more"
            ),
        }
    }

    /// The proof run could not be set up at all.
    #[must_use]
    pub fn prove_error(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            source: Source::Prove,
            path: None,
            message: message.into(),
        }
    }
}

/// The worst [`Severity`] across `diagnostics`, or `None` for an empty
/// list.
#[must_use]
pub fn max_severity(diagnostics: &[CliDiagnostic]) -> Option<Severity> {
    diagnostics.iter().map(|d| d.severity).max()
}

/// `astrs validate`'s exit code convention (blueprint §17): `0` clean,
/// `1` warnings only, `2` any error (including a file that never parsed).
#[must_use]
pub fn exit_code_for(diagnostics: &[CliDiagnostic]) -> i32 {
    match max_severity(diagnostics) {
        None | Some(Severity::Info) => 0,
        Some(Severity::Warning) => 1,
        Some(Severity::Error) => 2,
    }
}

const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

fn severity_color(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => RED,
        Severity::Warning => YELLOW,
        Severity::Info => CYAN,
    }
}

/// Render one diagnostic as a single human-readable line: `[severity]
/// (source) path: message`, with the severity tag and source both
/// wrapped in plain ANSI SGR codes when `use_color` is set (no `path`
/// segment when the diagnostic has none).
#[must_use]
pub fn render_line(diagnostic: &CliDiagnostic, use_color: bool) -> String {
    let severity_str = diagnostic.severity.to_string();
    let tag = if use_color {
        format!(
            "{BOLD}{}[{severity_str}]{RESET}",
            severity_color(diagnostic.severity)
        )
    } else {
        format!("[{severity_str}]")
    };
    let mut line = format!("{tag} ({}) ", diagnostic.source);
    if let Some(path) = &diagnostic.path {
        line.push_str(path);
        line.push_str(": ");
    }
    line.push_str(&diagnostic.message);
    line
}

/// Render every diagnostic, one per line, in list order.
#[must_use]
pub fn render_human(diagnostics: &[CliDiagnostic], use_color: bool) -> String {
    diagnostics
        .iter()
        .map(|d| render_line(d, use_color))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn max_severity_is_none_for_an_empty_list() {
        assert_eq!(max_severity(&[]), None);
        assert_eq!(exit_code_for(&[]), 0);
    }

    #[test]
    fn exit_code_reflects_the_worst_severity() {
        let info = vec![CliDiagnostic {
            severity: Severity::Info,
            source: Source::Graph,
            path: None,
            message: "m".to_string(),
        }];
        assert_eq!(exit_code_for(&info), 0);

        let warning = vec![CliDiagnostic {
            severity: Severity::Warning,
            source: Source::Graph,
            path: None,
            message: "m".to_string(),
        }];
        assert_eq!(exit_code_for(&warning), 1);

        let error = vec![CliDiagnostic {
            severity: Severity::Error,
            source: Source::Io,
            path: None,
            message: "m".to_string(),
        }];
        assert_eq!(exit_code_for(&error), 2);
    }

    #[test]
    fn exit_code_takes_the_worst_across_a_mixed_list() {
        let mixed = vec![
            CliDiagnostic {
                severity: Severity::Info,
                source: Source::Graph,
                path: None,
                message: "info".to_string(),
            },
            CliDiagnostic {
                severity: Severity::Warning,
                source: Source::Graph,
                path: None,
                message: "warning".to_string(),
            },
        ];
        assert_eq!(exit_code_for(&mixed), 1);
    }

    #[test]
    fn render_line_without_color_has_no_escape_codes() {
        let d = CliDiagnostic::io("cannot read file");
        let line = render_line(&d, false);
        assert!(!line.contains('\x1b'));
        assert!(line.contains("[error]"));
        assert!(line.contains("(io)"));
        assert!(line.contains("cannot read file"));
    }

    #[test]
    fn render_line_with_color_wraps_the_severity_tag() {
        let d = CliDiagnostic::io("cannot read file");
        let line = render_line(&d, true);
        assert!(line.contains('\x1b'));
        assert!(line.contains(RESET));
    }

    #[test]
    fn render_line_includes_path_when_present() {
        let d = CliDiagnostic {
            severity: Severity::Error,
            source: Source::Structural,
            path: Some("nodes[2].id".to_string()),
            message: "bad".to_string(),
        };
        let line = render_line(&d, false);
        assert!(line.contains("nodes[2].id: bad"));
    }

    #[test]
    fn structural_diagnostic_carries_the_validation_error_path() {
        let err = astrs_manifest::ValidationError {
            path: "nodes[0].id".to_string(),
            kind: astrs_manifest::ValidationErrorKind::InvalidIdCharset {
                id: "bad id".to_string(),
            },
        };
        let d = CliDiagnostic::structural(&err);
        assert_eq!(d.path.as_deref(), Some("nodes[0].id"));
        assert_eq!(d.severity, Severity::Error);
    }

    #[test]
    fn parse_diagnostic_captures_line_and_column_when_available() {
        let err = astrs_manifest::Manifest::from_yaml_str("nodes: [").unwrap_err();
        let d = CliDiagnostic::parse(&err);
        assert!(d.path.is_some(), "path was: {:?}", d.path);
    }

    #[test]
    fn graph_diagnostic_preserves_its_severity() {
        let diag = astrs_graph::Diagnostic::new(
            Severity::Warning,
            astrs_graph::DiagnosticKind::UnconsumedOutput {
                node: astrs_graph::NodeId::new("camera"),
                output: astrs_graph::PortName::new("frames"),
            },
        );
        let d = CliDiagnostic::graph(&diag);
        assert_eq!(d.severity, Severity::Warning);
        assert!(d.message.contains("camera"));
    }

    #[test]
    fn prove_unavailable_points_at_the_rebuild() {
        let d = CliDiagnostic::prove_unavailable();
        assert_eq!(d.severity, Severity::Warning);
        assert_eq!(d.source, Source::Prove);
        assert!(d.message.contains("--features verify"), "{}", d.message);
    }

    #[test]
    fn prove_violation_is_an_error_naming_both_halves() {
        let d = CliDiagnostic::prove_violation("deadlock freedom", "`a` can never fire");
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.source, Source::Prove);
        assert!(d.message.contains("deadlock freedom"));
        assert!(d.message.contains("can never fire"));
    }

    #[test]
    fn prove_incomplete_is_a_warning_counting_both_gaps() {
        let d = CliDiagnostic::prove_incomplete(2, 1);
        assert_eq!(d.severity, Severity::Warning);
        assert!(d.message.contains('2') && d.message.contains('1'));
    }

    #[test]
    fn prove_error_is_an_error() {
        let d = CliDiagnostic::prove_error("profile is inconsistent");
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.source, Source::Prove);
    }

    #[test]
    fn json_serialization_omits_absent_path() {
        let d = CliDiagnostic::io("nope");
        let json = serde_json::to_string(&d).unwrap();
        assert!(!json.contains("\"path\""));
        assert!(json.contains("\"severity\":\"error\""));
    }

    #[test]
    fn json_serialization_includes_present_path() {
        let d = CliDiagnostic {
            severity: Severity::Error,
            source: Source::Structural,
            path: Some("nodes[0]".to_string()),
            message: "m".to_string(),
        };
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("\"path\":\"nodes[0]\""));
    }
}
