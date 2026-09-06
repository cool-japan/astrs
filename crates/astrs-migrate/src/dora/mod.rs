//! `astrs migrate from-dora`: mechanically mapping a dora-rs dataflow
//! descriptor onto an AstRS manifest (blueprint §8.6).
//!
//! - `model` (crate-private) reads the dora YAML shape (ground-truthed
//!   against a real dora `dora-schema.json`, not reconstructed from
//!   memory).
//! - `convert` (crate-private) maps it onto
//!   [`astrs_manifest::Manifest`], collecting a [`MigrationNote`] for
//!   every construct with no automatic AstRS equivalent.
//! - `render` (crate-private) turns those notes into `TODO(astrs
//!   migrate)` YAML comments next to the node they describe, since a
//!   note has nowhere else to live once the manifest is serialized back
//!   to YAML.
//! - [`migrate_str`] / [`migrate_file`] wire the three together into the
//!   one entry point `astrs-cli`'s `migrate from-dora` verb calls.

mod byte_size;
mod convert;
mod model;
mod render;

pub use convert::{MigrationNote, NoteSeverity};

use std::path::Path;

use crate::error::DoraMigrateError;
use model::DoraManifest;

/// The result of migrating one dora dataflow descriptor.
#[derive(Debug, Clone, PartialEq)]
pub struct MigrationResult {
    /// The migrated manifest, rendered as YAML with `TODO(astrs migrate)`
    /// comments next to every construct that needed one. This is the text
    /// `astrs migrate from-dora` writes to stdout or the `--output` file —
    /// **not** [`astrs_manifest::Manifest::to_yaml`]'s bare output, which
    /// has nowhere to carry those comments.
    pub yaml: String,
    /// Every observation made during the mapping, in the order the
    /// mapping pass found them (root-level observations first, then each
    /// node in manifest order) — the same list [`Self::yaml`]'s comments
    /// were rendered from, for a caller that wants to inspect or filter
    /// them programmatically (e.g. `astrs migrate from-dora --json`)
    /// instead of grepping the YAML.
    pub notes: Vec<MigrationNote>,
}

impl MigrationResult {
    /// How many notes are [`NoteSeverity::NeedsAttention`] — the count a
    /// caller most likely wants to headline ("N constructs need manual
    /// review").
    #[must_use]
    pub fn needs_attention_count(&self) -> usize {
        self.notes
            .iter()
            .filter(|n| n.severity == NoteSeverity::NeedsAttention)
            .count()
    }

    /// How many notes are [`NoteSeverity::Dropped`] (informational: a
    /// construct that was already dead configuration in dora too).
    #[must_use]
    pub fn dropped_count(&self) -> usize {
        self.notes
            .iter()
            .filter(|n| n.severity == NoteSeverity::Dropped)
            .count()
    }
}

/// Migrate a dora dataflow descriptor, given as a YAML string.
///
/// # Errors
///
/// Returns [`DoraMigrateError::Yaml`] if `input` is not a well-formed dora
/// descriptor (a YAML mapping; `nodes:` may be absent — an empty
/// dataflow is unusual but not itself malformed, matching
/// [`astrs_manifest::Manifest::validate`]'s own stance), or
/// [`DoraMigrateError::Render`] if the mapped manifest could not be
/// serialized back to YAML (see that variant's docs for why this is not
/// expected to be reachable in practice).
pub fn migrate_str(input: &str) -> Result<MigrationResult, DoraMigrateError> {
    let dora: DoraManifest =
        astrs_yaml::from_str(input).map_err(|source| DoraMigrateError::Yaml { source })?;
    let (manifest, notes) = convert::convert(&dora);
    let base_yaml = manifest
        .to_yaml()
        .map_err(|source| DoraMigrateError::Render { source })?;
    let yaml = render::render(&base_yaml, &notes);
    Ok(MigrationResult { yaml, notes })
}

/// [`migrate_str`], reading the descriptor from a file first.
///
/// # Errors
///
/// Returns [`DoraMigrateError::Io`] if `path` cannot be read, or whatever
/// [`migrate_str`] returns for the file's contents.
pub fn migrate_file(path: impl AsRef<Path>) -> Result<MigrationResult, DoraMigrateError> {
    let path_ref = path.as_ref();
    let content = std::fs::read_to_string(path_ref)
        .map_err(|source| DoraMigrateError::io(path_ref, source))?;
    migrate_str(&content)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn migrate_str_rejects_non_yaml() {
        let err = migrate_str(": : :\nnot yaml at all [").unwrap_err();
        assert!(matches!(err, DoraMigrateError::Yaml { .. }));
    }

    #[test]
    fn migrate_str_on_a_minimal_descriptor_validates_clean() {
        let result = migrate_str("nodes:\n  - id: solo\n    path: ./solo\n").unwrap();
        assert_eq!(result.notes.len(), 0);
        let manifest = astrs_manifest::Manifest::from_yaml_str(&result.yaml).unwrap();
        manifest.validate().unwrap();
        assert!(result.yaml.starts_with("# Migrated from a dora-rs"));
    }

    #[test]
    fn migrate_file_reports_io_error_for_a_missing_path() {
        let path = std::env::temp_dir().join("astrs-migrate-test-does-not-exist.yml");
        let err = migrate_file(&path).unwrap_err();
        assert!(matches!(err, DoraMigrateError::Io { .. }));
    }

    #[test]
    fn migrate_file_round_trips_through_a_real_temp_file() {
        let dir = std::env::temp_dir().join(format!(
            "astrs-migrate-test-{}-round-trip",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dataflow.yml");
        std::fs::write(&path, "nodes:\n  - id: solo\n    path: ./solo\n").unwrap();

        let result = migrate_file(&path).unwrap();
        assert!(result.yaml.contains("solo"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn needs_attention_and_dropped_counts_are_disjoint() {
        let yaml = "\
nodes:
  - id: x
    path: ./x
    hub: dora-yolo@^0.5
    path_sha256: deadbeef
";
        let result = migrate_str(yaml).unwrap();
        assert_eq!(result.needs_attention_count(), 1);
        assert_eq!(result.dropped_count(), 1);
        assert_eq!(
            result.notes.len(),
            result.needs_attention_count() + result.dropped_count()
        );
    }
}
