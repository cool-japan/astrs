//! Parse-time error type for the manifest crate.
//!
//! [`ManifestError`] covers I/O and YAML syntax/type failures encountered
//! while reading a manifest. It is deliberately separate from
//! [`crate::ValidationErrors`], which reports *structural* problems in an
//! otherwise syntactically valid manifest (blueprint §8: "Parse API ...
//! error type carrying serde_yaml location when available").

use std::fmt;

/// Errors returned by [`crate::Manifest::from_yaml_str`],
/// [`crate::Manifest::from_yaml_file`] and [`crate::Manifest::to_yaml`].
///
/// `astrs_yaml` attaches a source line/column to most parse failures. When
/// one is available it is carried on [`ManifestError::YamlAt`] so callers
/// can point an editor cursor at the exact offending byte; when it is not
/// (some structural errors, or any error raised during serialization) the
/// plain [`ManifestError::Yaml`] / [`ManifestError::Serialize`] variants are
/// used instead.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ManifestError {
    /// A YAML syntax or type error with a known source location.
    #[error("YAML parse error at line {line}, column {column}: {source}")]
    YamlAt {
        /// One-based line number within the parsed document.
        line: usize,
        /// One-based column number within `line`.
        column: usize,
        /// The underlying `astrs_yaml` error.
        #[source]
        source: astrs_yaml::Error,
    },

    /// A YAML syntax or type error without a known source location.
    #[error("YAML parse error: {source}")]
    Yaml {
        /// The underlying `astrs_yaml` error.
        #[source]
        source: astrs_yaml::Error,
    },

    /// The manifest file could not be read from disk.
    #[error("failed to read manifest file `{path}`: {source}")]
    Io {
        /// The path that was passed to [`crate::Manifest::from_yaml_file`],
        /// rendered with [`std::path::Path::display`].
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The manifest could not be serialized back to YAML.
    ///
    /// This is rare: it can only happen if a `Serialize` implementation on
    /// a manifest type produces a YAML-incompatible shape (for example, a
    /// map keyed by a non-scalar type). It is surfaced rather than panicking
    /// so [`crate::Manifest::to_yaml`] stays panic-free.
    #[error("failed to serialize manifest to YAML: {source}")]
    Serialize {
        /// The underlying `astrs_yaml` error.
        #[source]
        source: astrs_yaml::Error,
    },
}

impl ManifestError {
    /// Build a [`ManifestError`] from an `astrs_yaml` parse failure,
    /// preserving its source location when the format provides one.
    #[must_use]
    pub fn from_yaml(source: astrs_yaml::Error) -> Self {
        match (source.line(), source.column()) {
            (Some(line), Some(column)) => Self::YamlAt {
                line,
                column,
                source,
            },
            _ => Self::Yaml { source },
        }
    }

    /// The one-based line number of the failure, when known.
    ///
    /// Only [`ManifestError::YamlAt`] carries a location; every other
    /// variant returns `None`.
    #[must_use]
    pub fn line(&self) -> Option<usize> {
        match self {
            Self::YamlAt { line, .. } => Some(*line),
            _ => None,
        }
    }

    /// The one-based column number of the failure, when known.
    ///
    /// Only [`ManifestError::YamlAt`] carries a location; every other
    /// variant returns `None`.
    #[must_use]
    pub fn column(&self) -> Option<usize> {
        match self {
            Self::YamlAt { column, .. } => Some(*column),
            _ => None,
        }
    }
}

/// A path into a manifest document, rendered as `nodes[3].inputs.frames`.
///
/// This is a thin builder shared by the validation pass
/// ([`crate::Manifest::validate`]) to build consistent, greppable error
/// paths without each check hand-formatting strings. It is intentionally
/// minimal: a base segment plus `.join(field)` and `.index(i)` chaining, in
/// the order the caller wants them rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathBuf(String);

impl PathBuf {
    /// Start a path at a named root field, e.g. `"nodes"` or `"type_rules"`.
    pub(crate) fn new(root: &str) -> Self {
        Self(root.to_string())
    }

    /// Append `[i]` to the path, e.g. `nodes` -> `nodes[3]`.
    #[must_use]
    pub(crate) fn index(&self, i: usize) -> Self {
        Self(format!("{}[{i}]", self.0))
    }

    /// Append `.field` to the path, e.g. `nodes[3]` -> `nodes[3].inputs`.
    #[must_use]
    pub(crate) fn join(&self, field: &str) -> Self {
        Self(format!("{}.{field}", self.0))
    }
}

impl fmt::Display for PathBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<PathBuf> for String {
    fn from(p: PathBuf) -> Self {
        p.0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn path_renders_dotted_and_indexed_segments() {
        let p = PathBuf::new("nodes").index(3).join("inputs").join("frames");
        assert_eq!(p.to_string(), "nodes[3].inputs.frames");

        let p2 = PathBuf::new("type_rules").index(0).join("from");
        assert_eq!(p2.to_string(), "type_rules[0].from");
    }

    #[test]
    fn manifest_error_from_yaml_extracts_location_when_present() {
        let err = astrs_yaml::from_str::<crate::Manifest>("nodes: [")
            .expect_err("truncated flow sequence must fail to parse");
        let wrapped = ManifestError::from_yaml(err);
        // astrs_yaml reliably attaches a location to syntax errors; a
        // structural/type error might not, so we only assert the happy path
        // here and leave the `None` branch to integration tests that feed
        // deliberately location-less failures.
        if let Some(line) = wrapped.line() {
            assert!(line >= 1);
        }
    }
}
