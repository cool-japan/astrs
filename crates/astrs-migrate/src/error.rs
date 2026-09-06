//! Error types shared by this crate's importers.

use std::path::PathBuf;

/// Errors returned by [`crate::dora::migrate_str`] / [`crate::dora::migrate_file`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DoraMigrateError {
    /// The input could not be read from disk.
    #[error("failed to read dora descriptor `{path}`: {source}")]
    Io {
        /// The path that was passed to [`crate::dora::migrate_file`].
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The input was not well-formed YAML, or did not match the shape this
    /// importer understands (a dora dataflow descriptor's root document is
    /// a mapping with at least a `nodes:` sequence).
    #[error("failed to parse dora descriptor as YAML: {source}")]
    Yaml {
        /// The underlying `astrs_yaml` error.
        #[source]
        source: astrs_yaml::Error,
    },

    /// The mapped [`astrs_manifest::Manifest`] could not be rendered back
    /// to YAML.
    ///
    /// Unreachable in practice ([`astrs_manifest::Manifest::to_yaml`]
    /// documents the same) but surfaced rather than unwrapped so this
    /// crate stays panic-free end to end.
    #[error("failed to render the migrated manifest to YAML: {source}")]
    Render {
        /// The underlying manifest error.
        #[source]
        source: astrs_manifest::ManifestError,
    },
}

impl DoraMigrateError {
    /// Build an [`DoraMigrateError::Io`] from a path and the I/O failure
    /// reading it.
    #[must_use]
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into().display().to_string(),
            source,
        }
    }
}

/// Errors returned by [`crate::ros2::migrate_ros2_launch_str`] /
/// [`crate::ros2::migrate_ros2_launch_file`].
///
/// Deliberately **not** `Clone`/`PartialEq`/`Eq` (unlike the old stub this
/// replaced): [`Self::Io`] wraps a [`std::io::Error`], which implements
/// neither -- matching [`DoraMigrateError`]'s own derive list exactly, for
/// the same reason. Tests match on variants with `matches!` rather than
/// `assert_eq!`.
///
/// Every variant here is a **top-level** failure -- the input file itself
/// (or the string passed to `migrate_ros2_launch_str`) could not be read or
/// did not parse as XML at all, or the include graph it names is
/// structurally broken (a cycle, or nesting past the depth limit). A
/// problem confined to one `<include>`d file reachable from a
/// well-formed root document -- it does not exist, cannot be read, is
/// itself not well-formed XML -- is deliberately **not** one of these: it
/// degrades to a [`crate::ros2::MigrationNote`] instead (blueprint §8.6:
/// "never silently drop", not "abort the whole migration over one broken
/// branch of it"), exactly like every other construct this importer cannot
/// automatically handle.
#[derive(Debug, thiserror::Error)]
pub enum Ros2MigrateError {
    /// The top-level input could not be read from disk.
    #[error("failed to read ROS 2 launch file `{path}`: {source}")]
    Io {
        /// The path that was passed to [`crate::ros2::migrate_ros2_launch_file`].
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The top-level input was not well-formed XML.
    #[error("failed to parse `{path}` as XML: {message}")]
    Xml {
        /// The path this input came from, or `"<input>"` for
        /// [`crate::ros2::migrate_ros2_launch_str`], which has none.
        path: String,
        /// A human-readable description of the syntax problem, taken
        /// directly from the parser rather than wrapping its error type
        /// (matching `astrs-idl`'s `IdlError::PackageXmlParse`, the
        /// existing in-repo precedent for reporting a `quick_xml` parse
        /// failure without exposing that crate's error type in this
        /// crate's own public API).
        message: String,
    },

    /// An `<include>` chain nested deeper than [`crate::ros2::MAX_INCLUDE_DEPTH`].
    ///
    /// Mirrors `astrs-manifest`'s own `ExpandError::DepthExceeded` for
    /// `module:` inclusion -- the same failure mode (a long, acyclic chain
    /// of distinct files) in a structurally identical recursive-inclusion
    /// problem.
    #[error(
        "include nesting exceeded the limit of {max_depth} (pass through a shallower launch \
         tree, or split it up)"
    )]
    IncludeDepthExceeded {
        /// The depth limit that was exceeded.
        max_depth: usize,
    },

    /// An `<include>` chain re-entered a file already being processed.
    ///
    /// Mirrors `astrs-manifest`'s own `ExpandError::Cycle`.
    #[error("include cycle detected: {}", chain.join(" -> "))]
    IncludeCycle {
        /// The chain of resolved include paths, root first, ending with
        /// the path that would re-enter the cycle.
        chain: Vec<String>,
    },

    /// The mapped [`astrs_manifest::Manifest`] could not be rendered back
    /// to YAML.
    ///
    /// Unreachable in practice (mirrors [`DoraMigrateError::Render`]'s own
    /// docs) but surfaced rather than unwrapped so this crate stays
    /// panic-free end to end.
    #[error("failed to render the migrated manifest to YAML: {source}")]
    Render {
        /// The underlying manifest error.
        #[source]
        source: astrs_manifest::ManifestError,
    },
}

impl Ros2MigrateError {
    /// Build a [`Ros2MigrateError::Io`] from a path and the I/O failure
    /// reading it.
    #[must_use]
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into().display().to_string(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn io_error_carries_the_display_path() {
        let err = DoraMigrateError::io(
            "/tmp/does-not-exist.yml",
            std::io::Error::new(std::io::ErrorKind::NotFound, "nope"),
        );
        let rendered = err.to_string();
        assert!(rendered.contains("/tmp/does-not-exist.yml"), "{rendered}");
    }

    #[test]
    fn ros2_io_error_carries_the_display_path() {
        let err = Ros2MigrateError::io(
            "/tmp/does-not-exist.launch.xml",
            std::io::Error::new(std::io::ErrorKind::NotFound, "nope"),
        );
        assert!(matches!(err, Ros2MigrateError::Io { .. }));
        assert!(err.to_string().contains("/tmp/does-not-exist.launch.xml"));
    }

    #[test]
    fn ros2_xml_error_names_the_path_and_message() {
        let err = Ros2MigrateError::Xml {
            path: "bad.launch.xml".to_string(),
            message: "unexpected EOF".to_string(),
        };
        let rendered = err.to_string();
        assert!(rendered.contains("bad.launch.xml"));
        assert!(rendered.contains("unexpected EOF"));
    }

    #[test]
    fn ros2_include_depth_exceeded_names_the_limit() {
        let err = Ros2MigrateError::IncludeDepthExceeded { max_depth: 32 };
        assert!(err.to_string().contains('3') && err.to_string().contains('2'));
    }

    #[test]
    fn ros2_include_cycle_joins_the_chain() {
        let err = Ros2MigrateError::IncludeCycle {
            chain: vec![
                "a.xml".to_string(),
                "b.xml".to_string(),
                "a.xml".to_string(),
            ],
        };
        assert_eq!(
            err.to_string(),
            "include cycle detected: a.xml -> b.xml -> a.xml"
        );
    }
}
