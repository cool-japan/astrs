//! Reading the machine-readable reports the `astrs` verbs print.
//!
//! `astrs run --json` streams every node's output first and prints its report
//! last, on the same stream, so a test cannot simply hand stdout to a JSON
//! parser. [`json_tail`] finds where the report starts — the last line that is
//! exactly `{`, since the report is pretty-printed at column zero and every
//! streamed line carries a timing prefix — and works backwards until one
//! candidate parses. That is deliberately more forgiving than matching a
//! marker string: a node is free to print anything, including a lone brace,
//! and the suite must not become a constraint on what example nodes may say.
//!
//! The structs here are *the suite's own* view of those reports. They
//! deliberately do not reuse `astrs-cli`'s types: a conformance test that
//! deserialised with the same definition the producer serialised with would
//! agree with itself no matter what the JSON actually contained.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::FixtureError;

/// The report `astrs run --json` prints when a dataflow ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliRunReport {
    /// `finished`, `failed`, `stopped` — the dataflow's terminal status.
    pub status: String,
    /// The process exit code the CLI mapped that status to.
    pub exit_code: i32,
    /// Whether any node failed.
    pub failed: bool,
    /// Whether the run gave up waiting and left nodes behind.
    pub abandoned: bool,
    /// The dataflow's generated identifier.
    pub dataflow: String,
    /// A human-readable note, empty on a clean run.
    #[serde(default)]
    pub message: String,
    /// How many streamed node lines reached the terminal.
    #[serde(default)]
    pub printed_lines: u64,
    /// How many streamed node lines were dropped by the level filter.
    #[serde(default)]
    pub dropped_lines: u64,
    /// One entry per node, keyed by node id.
    #[serde(default)]
    pub nodes: BTreeMap<String, CliNodeResult>,
}

/// How one node ended, as `astrs run --json` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliNodeResult {
    /// Why the node ended, in words.
    pub cause: String,
    /// Whether that counts as a failure.
    pub failed: bool,
}

impl CliRunReport {
    /// Parses the report out of a `astrs run --json` invocation's stdout.
    ///
    /// # Errors
    ///
    /// [`FixtureError::Output`] with the whole output attached when no JSON
    /// object in it parses as a run report.
    pub fn parse(command: &str, stdout: &str) -> Result<Self, FixtureError> {
        parse_tail(command, stdout, "no JSON run report was printed")
    }

    /// Whether the dataflow finished cleanly with every node succeeding.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.status == "finished"
            && self.exit_code == 0
            && !self.failed
            && !self.abandoned
            && self.nodes.values().all(|node| !node.failed)
    }

    /// The ids of the nodes that failed, in order.
    #[must_use]
    pub fn failed_nodes(&self) -> Vec<&str> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.failed)
            .map(|(id, _)| id.as_str())
            .collect()
    }
}

/// The report `astrs validate --json` prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliValidateReport {
    /// The manifest that was checked, as it was named on the command line.
    pub manifest_path: String,
    /// Everything the parser, expander and type checker found.
    #[serde(default)]
    pub diagnostics: Vec<CliDiagnostic>,
}

/// One diagnostic from `astrs validate --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliDiagnostic {
    /// `error` or `warning`.
    pub severity: String,
    /// Which stage produced it — `manifest`, `graph`, and so on.
    pub source: String,
    /// The rendered message.
    pub message: String,
}

impl CliValidateReport {
    /// Parses the report out of a `astrs validate --json` invocation's stdout.
    ///
    /// # Errors
    ///
    /// [`FixtureError::Output`] with the whole output attached when no JSON
    /// object in it parses as a validate report.
    pub fn parse(command: &str, stdout: &str) -> Result<Self, FixtureError> {
        parse_tail(command, stdout, "no JSON validation report was printed")
    }

    /// Whether the manifest produced nothing at all.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.diagnostics.is_empty()
    }

    /// Every diagnostic rendered on its own line, for an assertion message.
    #[must_use]
    pub fn rendered(&self) -> String {
        self.diagnostics
            .iter()
            .map(|diagnostic| {
                format!(
                    "{}/{}: {}",
                    diagnostic.severity, diagnostic.source, diagnostic.message
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Parses the last JSON object in `text` as `T`.
fn parse_tail<T: serde::de::DeserializeOwned>(
    command: &str,
    text: &str,
    reason: &str,
) -> Result<T, FixtureError> {
    for candidate in json_tail(text) {
        if let Ok(value) = serde_json::from_str::<T>(candidate) {
            return Ok(value);
        }
    }
    Err(FixtureError::Output {
        command: command.to_owned(),
        reason: reason.to_owned(),
        output: text.to_owned(),
    })
}

/// Every suffix of `text` that starts at a line which is exactly `{`, latest
/// first.
///
/// The latest is tried first because a verb prints its report last; the
/// earlier ones are kept so a node that printed a lone brace cannot make the
/// report unreadable.
#[must_use]
pub fn json_tail(text: &str) -> Vec<&str> {
    let mut starts = Vec::new();
    let mut offset = 0_usize;
    for line in text.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "{" {
            starts.push(offset);
        }
        offset += line.len();
    }
    starts
        .into_iter()
        .rev()
        .filter_map(|start| text.get(start..))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// The real shape `astrs run --json` prints, streamed output and all.
    const RUN_OUTPUT: &str = "\
   0.13s [greeter] hello from greeter
   0.21s [greeter] tick 1 at hlc 1787103828606927000-0
{
  \"abandoned\": false,
  \"dataflow\": \"01a017b0-59af-7cb2-81f9-476978ef9ff4\",
  \"dropped_lines\": 0,
  \"exit_code\": 0,
  \"failed\": false,
  \"message\": \"\",
  \"nodes\": {
    \"greeter\": {
      \"cause\": \"exited successfully\",
      \"failed\": false
    }
  },
  \"printed_lines\": 14,
  \"status\": \"finished\"
}
";

    #[test]
    fn a_run_report_is_read_out_of_streamed_output() {
        let report = CliRunReport::parse("astrs run --json x.yml", RUN_OUTPUT).unwrap();
        assert_eq!(report.status, "finished");
        assert_eq!(report.exit_code, 0);
        assert!(report.is_clean());
        assert_eq!(report.nodes.len(), 1);
        assert_eq!(
            report.nodes.get("greeter").map(|node| node.cause.as_str()),
            Some("exited successfully")
        );
        assert!(report.failed_nodes().is_empty());
    }

    /// A node that prints a lone `{` does not make the report unreadable.
    #[test]
    fn a_node_printing_a_brace_does_not_hide_the_report() {
        let noisy = format!("{{\nnot json at all\n{RUN_OUTPUT}");
        let report = CliRunReport::parse("astrs run --json x.yml", &noisy).unwrap();
        assert_eq!(report.status, "finished");
    }

    /// A failed run is reported as failed, with the node named.
    #[test]
    fn a_failed_run_names_the_node() {
        let text = "\
{
  \"abandoned\": false,
  \"dataflow\": \"d\",
  \"exit_code\": 1,
  \"failed\": true,
  \"message\": \"1 node failed\",
  \"nodes\": {\"greeter\": {\"cause\": \"spawn failed\", \"failed\": true}},
  \"status\": \"failed\"
}
";
        let report = CliRunReport::parse("astrs run --json x.yml", text).unwrap();
        assert!(!report.is_clean());
        assert_eq!(report.failed_nodes(), vec!["greeter"]);
        assert_eq!(report.exit_code, 1);
    }

    /// Output with no report at all is an error that shows the output.
    #[test]
    fn missing_json_is_an_error_that_shows_the_output() {
        let error =
            CliRunReport::parse("astrs run --json x.yml", "the graph said nothing\n").unwrap_err();
        let text = error.to_string();
        assert!(text.contains("no JSON run report"), "{text}");
        assert!(text.contains("the graph said nothing"), "{text}");
    }

    /// A clean validation and a dirty one both round trip.
    #[test]
    fn a_validate_report_reads_back() {
        let clean = "{\n  \"manifest_path\": \"a.yml\",\n  \"diagnostics\": []\n}\n";
        let report = CliValidateReport::parse("astrs validate --json a.yml", clean).unwrap();
        assert!(report.is_clean());
        assert_eq!(report.rendered(), "");

        let dirty = "{\n  \"manifest_path\": \"a.yml\",\n  \"diagnostics\": [\
            {\"severity\": \"warning\", \"source\": \"graph\", \"message\": \"type mismatch\"}]\n}\n";
        let report = CliValidateReport::parse("astrs validate --json a.yml", dirty).unwrap();
        assert!(!report.is_clean());
        assert_eq!(report.rendered(), "warning/graph: type mismatch");
    }

    /// The tail scanner returns candidates latest first.
    #[test]
    fn the_tail_scanner_walks_backwards() {
        let text = "a\n{\nfirst\n{\nsecond\n";
        let candidates = json_tail(text);
        assert_eq!(candidates.len(), 2);
        assert!(candidates[0].starts_with("{\nsecond"), "{candidates:?}");
        assert!(candidates[1].starts_with("{\nfirst"), "{candidates:?}");
        assert!(json_tail("no braces here").is_empty());
    }

    /// The suite's own structs round trip, so a failure message can quote a
    /// report it constructed.
    #[test]
    fn the_report_structs_round_trip() {
        let report = CliRunReport {
            status: "finished".to_owned(),
            exit_code: 0,
            failed: false,
            abandoned: false,
            dataflow: "d".to_owned(),
            message: String::new(),
            printed_lines: 3,
            dropped_lines: 0,
            nodes: BTreeMap::from([(
                "n".to_owned(),
                CliNodeResult {
                    cause: "exited successfully".to_owned(),
                    failed: false,
                },
            )]),
        };
        let json = serde_json::to_string(&report).unwrap();
        let parsed: CliRunReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }
}
