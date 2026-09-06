//! `astrs schema` (blueprint §17): emit the manifest JSON schema.
//!
//! Delegates entirely to [`astrs_manifest::emit_schema`] — this module's
//! only job is wiring that library function to stdout or a file, exactly
//! so `xtask schema` (out of this stage's scope; xtask is off-limits per
//! this wave's brief) can later call the same library function this
//! command calls, rather than either place re-deriving the schema.

use std::io::Write;
use std::path::PathBuf;

use crate::error::CliError;

/// Arguments for `astrs schema`.
#[derive(Debug, Clone, Default)]
pub struct SchemaArgs {
    /// Write the schema to this file instead of the output sink.
    pub output: Option<PathBuf>,
}

/// The result of emitting the schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaReport {
    /// The emitted schema text.
    pub schema: String,
    /// The file it was written to, if [`SchemaArgs::output`] was set.
    pub written_to: Option<PathBuf>,
}

/// Emit the manifest JSON schema to `out`, or to [`SchemaArgs::output`]
/// when set.
///
/// # Errors
///
/// Returns [`CliError::Io`] if writing to [`SchemaArgs::output`] (or, with
/// no `output`, to `out` itself) fails.
pub fn run(out: &mut dyn Write, args: &SchemaArgs) -> Result<SchemaReport, CliError> {
    let schema = astrs_manifest::emit_schema();
    match &args.output {
        Some(path) => {
            std::fs::write(path, &schema).map_err(|err| CliError::io(path, err))?;
        }
        None => {
            writeln!(out, "{schema}").map_err(|e| CliError::io("<output>", e))?;
        }
    }
    Ok(SchemaReport {
        schema,
        written_to: args.output.clone(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn default_output_writes_valid_json_to_the_sink() {
        let mut out = Vec::new();
        let report = run(&mut out, &SchemaArgs::default()).unwrap();
        assert!(report.written_to.is_none());
        let printed = String::from_utf8(out).unwrap();
        let value: serde_json::Value = serde_json::from_str(printed.trim_end()).unwrap();
        assert!(value.is_object());
        assert_eq!(printed.trim_end(), report.schema);
    }

    #[test]
    fn output_path_writes_to_the_file_not_the_sink() {
        let dir =
            std::env::temp_dir().join(format!("astrs-cli-schema-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("schema.json");

        let mut out = Vec::new();
        let report = run(
            &mut out,
            &SchemaArgs {
                output: Some(path.clone()),
            },
        )
        .unwrap();
        assert!(out.is_empty(), "nothing should be written to the sink");
        assert_eq!(report.written_to, Some(path.clone()));
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, report.schema);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn schema_never_mentions_output_framing() {
        // Blueprint §2.2: this field must not exist anywhere.
        let mut out = Vec::new();
        let report = run(&mut out, &SchemaArgs::default()).unwrap();
        assert!(!report.schema.contains("output_framing"));
    }

    #[test]
    fn unwritable_output_path_is_an_io_error() {
        let path = PathBuf::from("/this/path/does/not/exist/schema.json");
        let mut out = Vec::new();
        let err = run(&mut out, &SchemaArgs { output: Some(path) }).unwrap_err();
        assert!(matches!(err, CliError::Io { .. }));
    }
}
