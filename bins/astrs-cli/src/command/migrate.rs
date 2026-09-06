//! `astrs migrate from-dora` / `from-ros2` (blueprint §8.6, §17):
//! delegates the actual mapping to `astrs-migrate`; this module is just
//! the CLI-facing wiring (argument handling, output routing, human/JSON
//! rendering).

use std::io::Write;
use std::path::PathBuf;

use serde::Serialize;

use crate::error::CliError;

/// Arguments for `astrs migrate from-dora`.
#[derive(Debug, Clone)]
pub struct FromDoraArgs {
    /// The dora dataflow descriptor to migrate.
    pub input: PathBuf,
    /// Write the migrated manifest here instead of the output sink.
    pub output: Option<PathBuf>,
    /// Emit a structured JSON report (migrated YAML plus every
    /// [`astrs_migrate::MigrationNote`]) instead of plain YAML/a summary
    /// line.
    pub json: bool,
}

/// Arguments for `astrs migrate from-ros2`.
#[derive(Debug, Clone)]
pub struct FromRos2Args {
    /// The ROS 2 launch file to skim (XML or Python -- dispatched by
    /// extension, see [`astrs_migrate::migrate_ros2_launch_file`]).
    pub input: PathBuf,
    /// Write the migrated manifest here instead of the output sink.
    pub output: Option<PathBuf>,
    /// Emit a structured JSON report (migrated YAML plus every
    /// [`astrs_migrate::Ros2MigrationNote`]) instead of plain YAML/a
    /// summary line.
    pub json: bool,
}

/// The result of `astrs migrate from-dora`.
#[derive(Debug, Clone, Serialize)]
pub struct FromDoraReport {
    /// The migrated manifest, rendered as YAML with `TODO(astrs migrate)`
    /// comments (see [`astrs_migrate::MigrationResult::yaml`]).
    pub yaml: String,
    /// Every observation made during the mapping.
    pub notes: Vec<astrs_migrate::MigrationNote>,
    /// The file it was written to, if [`FromDoraArgs::output`] was set.
    pub written_to: Option<PathBuf>,
}

impl FromDoraReport {
    /// `2` if any note is [`astrs_migrate::NoteSeverity::NeedsAttention`]
    /// (the migrated manifest almost certainly needs a human before it
    /// runs), `0` otherwise — mirroring `validate`'s "clean vs. needs a
    /// look" convention, without reusing its exact 0/1/2 scale (a
    /// migration note is never merely a warning: every
    /// `NeedsAttention` note names something this importer could not
    /// automatically translate at all).
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if self
            .notes
            .iter()
            .any(|n| n.severity == astrs_migrate::NoteSeverity::NeedsAttention)
        {
            2
        } else {
            0
        }
    }
}

/// The result of `astrs migrate from-ros2`.
#[derive(Debug, Clone, Serialize)]
pub struct FromRos2Report {
    /// The migrated manifest, rendered as YAML with `TODO(astrs migrate)`
    /// comments (see [`astrs_migrate::Ros2MigrationResult::yaml`]).
    pub yaml: String,
    /// Every observation made during the mapping.
    pub notes: Vec<astrs_migrate::Ros2MigrationNote>,
    /// The file it was written to, if [`FromRos2Args::output`] was set.
    pub written_to: Option<PathBuf>,
}

impl FromRos2Report {
    /// `2` if any note is [`astrs_migrate::Ros2NoteSeverity::NeedsAttention`],
    /// `0` otherwise -- see [`FromDoraReport::exit_code`] for the
    /// underlying convention this mirrors.
    ///
    /// For the XML launch-file path this is effectively **always `2`**
    /// once at least one `<node>` was discovered: a bridge's
    /// `message_type`/`direction` can never be inferred from a launch
    /// file alone (`astrs_migrate::ros2::convert`'s own docs), so every
    /// discovered node contributes at least one `NeedsAttention` note.
    /// That is expected and honest, not a bug -- a `from-ros2` run that
    /// reports `0` either found no nodes at all, or migrated a Python
    /// launch file whose skim found no candidates either (its general
    /// "Python is not parsed" note is root-level but still
    /// `NeedsAttention`, so even that path is realistically never `0`
    /// with real content).
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        if self
            .notes
            .iter()
            .any(|n| n.severity == astrs_migrate::Ros2NoteSeverity::NeedsAttention)
        {
            2
        } else {
            0
        }
    }
}

/// Run `astrs migrate from-dora`.
///
/// With neither `--output` nor `--json`, the migrated YAML (comments and
/// all) is written to `out` directly — `astrs migrate from-dora
/// dataflow.yml > astrs-dataflow.yml` works exactly like every other
/// Unix text-generating tool. `--json` always prints the full structured
/// report instead (even alongside `--output`, so a script gets a
/// programmatic summary of what needs attention without re-parsing YAML
/// comments) regardless of whether `--output` was also given.
///
/// # Errors
///
/// Returns [`CliError::DoraMigrate`] if the input cannot be read or
/// parsed, or [`CliError::Io`] if `--output` cannot be written.
pub fn run_from_dora(out: &mut dyn Write, args: &FromDoraArgs) -> Result<FromDoraReport, CliError> {
    let result = astrs_migrate::migrate_file(&args.input)?;

    if let Some(path) = &args.output {
        std::fs::write(path, &result.yaml).map_err(|err| CliError::io(path, err))?;
    }

    let report = FromDoraReport {
        yaml: result.yaml,
        notes: result.notes,
        written_to: args.output.clone(),
    };

    match (&args.output, args.json) {
        (_, true) => {
            let json = serde_json::to_string_pretty(&report).unwrap_or_else(|err| {
                format!("{{\"error\": \"failed to serialize migrate report: {err}\"}}")
            });
            writeln!(out, "{json}").map_err(|e| CliError::io("<output>", e))?;
        }
        (None, false) => {
            writeln!(out, "{}", report.yaml).map_err(|e| CliError::io("<output>", e))?;
        }
        (Some(path), false) => {
            let needs_attention = report
                .notes
                .iter()
                .filter(|n| n.severity == astrs_migrate::NoteSeverity::NeedsAttention)
                .count();
            let dropped = report.notes.len() - needs_attention;
            writeln!(
                out,
                "migrated `{}` -> `{}` ({needs_attention} note(s) need attention, {dropped} dropped)",
                args.input.display(),
                path.display(),
            )
            .map_err(|e| CliError::io("<output>", e))?;
        }
    }

    Ok(report)
}

/// Run `astrs migrate from-ros2`.
///
/// Mirrors [`run_from_dora`] exactly -- same `--output`/`--json`
/// precedence, same plain-text summary shape -- see that function's docs
/// for the full behavior; the only difference is the underlying importer
/// ([`astrs_migrate::migrate_ros2_launch_file`], which also dispatches
/// XML vs. Python launch files by extension).
///
/// # Errors
///
/// Returns [`CliError::Ros2Migrate`] if the input cannot be read or
/// parsed (or, for XML input, if its `<include>` graph is a cycle or
/// nests past the depth limit), or [`CliError::Io`] if `--output` cannot
/// be written.
pub fn run_from_ros2(out: &mut dyn Write, args: &FromRos2Args) -> Result<FromRos2Report, CliError> {
    let result = astrs_migrate::migrate_ros2_launch_file(&args.input)?;

    if let Some(path) = &args.output {
        std::fs::write(path, &result.yaml).map_err(|err| CliError::io(path, err))?;
    }

    let report = FromRos2Report {
        yaml: result.yaml,
        notes: result.notes,
        written_to: args.output.clone(),
    };

    match (&args.output, args.json) {
        (_, true) => {
            let json = serde_json::to_string_pretty(&report).unwrap_or_else(|err| {
                format!("{{\"error\": \"failed to serialize migrate report: {err}\"}}")
            });
            writeln!(out, "{json}").map_err(|e| CliError::io("<output>", e))?;
        }
        (None, false) => {
            writeln!(out, "{}", report.yaml).map_err(|e| CliError::io("<output>", e))?;
        }
        (Some(path), false) => {
            let needs_attention = report
                .notes
                .iter()
                .filter(|n| n.severity == astrs_migrate::Ros2NoteSeverity::NeedsAttention)
                .count();
            let dropped = report.notes.len() - needs_attention;
            writeln!(
                out,
                "migrated `{}` -> `{}` ({needs_attention} note(s) need attention, {dropped} dropped)",
                args.input.display(),
                path.display(),
            )
            .map_err(|e| CliError::io("<output>", e))?;
        }
    }

    Ok(report)
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
            "astrs-cli-migrate-test-{}-{name}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dataflow.yml");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn plain_run_prints_yaml_directly_to_the_sink() {
        let path = write_temp("plain", "nodes:\n  - id: solo\n    path: ./solo\n");
        let mut out = Vec::new();
        let report = run_from_dora(
            &mut out,
            &FromDoraArgs {
                input: path,
                output: None,
                json: false,
            },
        )
        .unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert_eq!(printed.trim_end(), report.yaml.trim_end());
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn needs_attention_notes_produce_exit_code_two() {
        let path = write_temp("hub", "nodes:\n  - id: x\n    hub: dora-yolo@^0.5\n");
        let mut out = Vec::new();
        let report = run_from_dora(
            &mut out,
            &FromDoraArgs {
                input: path,
                output: None,
                json: false,
            },
        )
        .unwrap();
        assert_eq!(report.exit_code(), 2);
    }

    #[test]
    fn output_flag_writes_the_file_and_prints_a_summary() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-cli-migrate-test-{}-output",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("dataflow.yml");
        std::fs::write(&input, "nodes:\n  - id: solo\n    path: ./solo\n").unwrap();
        let output = dir.join("migrated.yaml");

        let mut out = Vec::new();
        let report = run_from_dora(
            &mut out,
            &FromDoraArgs {
                input,
                output: Some(output.clone()),
                json: false,
            },
        )
        .unwrap();
        let on_disk = std::fs::read_to_string(&output).unwrap();
        assert_eq!(on_disk, report.yaml);
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("migrated"));
        assert!(
            !printed.contains("nodes:"),
            "the sink should get a summary, not the YAML: {printed}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn json_flag_emits_a_structured_report_with_notes() {
        let path = write_temp(
            "json",
            "nodes:\n  - id: x\n    path: ./x\n    path_sha256: deadbeef\n",
        );
        let mut out = Vec::new();
        let report = run_from_dora(
            &mut out,
            &FromDoraArgs {
                input: path,
                output: None,
                json: true,
            },
        )
        .unwrap();
        let printed = String::from_utf8(out).unwrap();
        let value: serde_json::Value = serde_json::from_str(&printed).unwrap();
        assert_eq!(value["notes"].as_array().map(Vec::len), Some(1));
        assert_eq!(report.notes.len(), 1);
    }

    #[test]
    fn missing_input_is_a_dora_migrate_error() {
        let mut out = Vec::new();
        let err = run_from_dora(
            &mut out,
            &FromDoraArgs {
                input: std::env::temp_dir().join("astrs-cli-migrate-does-not-exist.yml"),
                output: None,
                json: false,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CliError::DoraMigrate(_)));
    }

    fn write_temp_ros2(name: &str, content: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "astrs-cli-migrate-ros2-test-{}-{name}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("robot.launch.xml");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn ros2_plain_run_prints_yaml_directly_to_the_sink() {
        let path = write_temp_ros2("plain", "<launch/>");
        let mut out = Vec::new();
        let report = run_from_ros2(
            &mut out,
            &FromRos2Args {
                input: path,
                output: None,
                json: false,
            },
        )
        .unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert_eq!(printed.trim_end(), report.yaml.trim_end());
        assert_eq!(
            report.exit_code(),
            0,
            "a nodeless launch file has no notes at all"
        );
    }

    #[test]
    fn ros2_a_discovered_node_always_needs_attention() {
        let path = write_temp_ros2(
            "node",
            r#"<launch><node pkg="p" exec="e" name="n"/></launch>"#,
        );
        let mut out = Vec::new();
        let report = run_from_ros2(
            &mut out,
            &FromRos2Args {
                input: path,
                output: None,
                json: false,
            },
        )
        .unwrap();
        assert_eq!(report.exit_code(), 2, "message_type is never inferable");
    }

    #[test]
    fn ros2_output_flag_writes_the_file_and_prints_a_summary() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-cli-migrate-ros2-test-{}-output",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("robot.launch.xml");
        std::fs::write(&input, "<launch/>").unwrap();
        let output = dir.join("migrated.yaml");

        let mut out = Vec::new();
        let report = run_from_ros2(
            &mut out,
            &FromRos2Args {
                input,
                output: Some(output.clone()),
                json: false,
            },
        )
        .unwrap();
        let on_disk = std::fs::read_to_string(&output).unwrap();
        assert_eq!(on_disk, report.yaml);
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("migrated"));
        assert!(
            !printed.contains("nodes:"),
            "the sink should get a summary, not the YAML: {printed}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ros2_json_flag_emits_a_structured_report_with_notes() {
        let path = write_temp_ros2(
            "json",
            r#"<launch><node pkg="p" exec="e" name="n"/></launch>"#,
        );
        let mut out = Vec::new();
        let report = run_from_ros2(
            &mut out,
            &FromRos2Args {
                input: path,
                output: None,
                json: true,
            },
        )
        .unwrap();
        let printed = String::from_utf8(out).unwrap();
        let value: serde_json::Value = serde_json::from_str(&printed).unwrap();
        assert_eq!(value["notes"].as_array().map(Vec::len), Some(1));
        assert_eq!(report.notes.len(), 1);
    }

    #[test]
    fn ros2_missing_input_is_a_ros2_migrate_error() {
        let mut out = Vec::new();
        let err = run_from_ros2(
            &mut out,
            &FromRos2Args {
                input: std::env::temp_dir().join("astrs-cli-migrate-ros2-does-not-exist.xml"),
                output: None,
                json: false,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CliError::Ros2Migrate(_)));
        assert_eq!(
            err.exit_code(),
            1,
            "a missing file is an ordinary failure, not `this feature is unavailable`"
        );
    }

    #[test]
    fn ros2_malformed_xml_is_a_ros2_migrate_error() {
        let path = write_temp_ros2("malformed", "<launch><node>");
        let mut out = Vec::new();
        let err = run_from_ros2(
            &mut out,
            &FromRos2Args {
                input: path,
                output: None,
                json: false,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CliError::Ros2Migrate(_)));
    }

    #[test]
    fn ros2_dispatches_python_launch_files_by_extension() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-cli-migrate-ros2-test-{}-python",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("robot.launch.py");
        std::fs::write(&path, "Node(name='n')").unwrap();

        let mut out = Vec::new();
        let report = run_from_ros2(
            &mut out,
            &FromRos2Args {
                input: path,
                output: None,
                json: false,
            },
        )
        .unwrap();
        assert!(report.yaml.starts_with("# Skeleton scaffold"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
