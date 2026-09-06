//! `xtask`'s single error type.
//!
//! Mirrors `astrs-cli`'s `CliError` (`bins/astrs-cli/src/error.rs`): every
//! library-testable function in this crate returns `Result<_, XtaskError>`,
//! and [`crate::main`] maps the `Err` case to a one-line message on stderr
//! and a non-zero [`std::process::ExitCode`].
//!
//! `XtaskError` is reserved for failures in *xtask itself* -- a Cargo.toml
//! that will not parse, a file that cannot be read, a subprocess that could
//! not even be spawned. A subprocess that spawns fine and exits non-zero
//! (`cargo clippy` found a warning, `cargo test` found a failure) is not an
//! `XtaskError`: it is the expected, reportable outcome of a preflight step,
//! carried in that step's own result type instead.

use std::path::{Path, PathBuf};

/// Everything that can abort an `xtask` command outright.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum XtaskError {
    /// A file could not be read or written.
    #[error("failed to access `{path}`: {source}")]
    Io {
        /// The path that could not be accessed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A Cargo.toml did not parse as TOML, or was missing a field this
    /// crate needs (`[package].name`, `[workspace].members`, ...).
    #[error("failed to parse `{path}` as a Cargo manifest: {source}")]
    TomlParse {
        /// The manifest that failed to parse.
        path: PathBuf,
        /// The underlying TOML error.
        #[source]
        source: Box<toml::de::Error>,
    },

    /// A subprocess (`cargo fmt`, `cargo clippy`, ...) could not be
    /// spawned at all -- the program was not found, or the OS refused to
    /// start it. A subprocess that starts and exits non-zero is not this
    /// variant; see the module docs.
    #[error("failed to run `{program}`: {source}")]
    Spawn {
        /// The program that could not be started, e.g. `"cargo"`.
        program: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// [`crate::workspace::topological_order`] could not order every
    /// member it was given -- structurally unreachable for `[dependencies]`
    /// and `[build-dependencies]` edges alone (Cargo itself refuses to
    /// build a workspace with a regular-dependency cycle), so this fires
    /// only if that invariant is ever broken.
    #[error(
        "dependency graph among {{{}}} has no valid publish order (a cycle among \
         regular/build dependencies -- Cargo would already refuse to build this workspace)",
        remaining.join(", ")
    )]
    CyclicDependencies {
        /// The crates that could never be scheduled.
        remaining: Vec<String>,
    },

    /// A subprocess this crate needs to *read the output of* (unlike the
    /// fire-and-check-exit-status steps in [`crate::preflight`]) spawned
    /// fine but exited non-zero -- currently only [`crate::sys_sweep`]'s
    /// `cargo tree` calls. Left nothing meaningful in its stdout to search,
    /// so the check that needed it could not run at all; that makes this
    /// xtask's own machinery failing (like [`Self::TomlParse`]), not a
    /// discovered policy violation, which is why it is an `XtaskError`
    /// rather than a `Fail` carried in the step's own report type -- see
    /// that distinction in this module's own doc comment.
    #[error("`{command}` exited non-zero: {stderr}")]
    CommandFailed {
        /// The program and arguments that were run, e.g. `"cargo tree -e
        /// normal ..."`.
        command: String,
        /// Its stderr, truncated to a manageable length for the error
        /// message.
        stderr: String,
    },
}

impl XtaskError {
    /// Build an [`XtaskError::Io`] naming `path`.
    pub(crate) fn io(path: impl AsRef<Path>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.as_ref().to_path_buf(),
            source,
        }
    }

    /// Build an [`XtaskError::TomlParse`] naming `path`.
    pub(crate) fn toml_parse(path: impl AsRef<Path>, source: toml::de::Error) -> Self {
        Self::TomlParse {
            path: path.as_ref().to_path_buf(),
            source: Box::new(source),
        }
    }

    /// Build an [`XtaskError::Spawn`] naming `program`.
    pub(crate) fn spawn(program: impl Into<String>, source: std::io::Error) -> Self {
        Self::Spawn {
            program: program.into(),
            source,
        }
    }

    /// Build an [`XtaskError::CommandFailed`] naming `command`, truncating
    /// `stderr` (`.chars().take(..)`, so the cut is always on a character
    /// boundary) so one runaway subprocess cannot blow up an error message.
    pub(crate) fn command_failed(command: impl Into<String>, stderr: &str) -> Self {
        const MAX_STDERR_CHARS: usize = 2000;
        let mut truncated: String = stderr.chars().take(MAX_STDERR_CHARS).collect();
        if stderr.chars().count() > MAX_STDERR_CHARS {
            truncated.push_str(" ... (truncated)");
        }
        Self::CommandFailed {
            command: command.into(),
            stderr: truncated,
        }
    }
}
