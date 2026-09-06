//! The single error type every conformance helper reports through.
//!
//! A conformance suite is only worth what its failures say. Each variant here
//! is written so the message alone tells a reader what to do next — the exact
//! `cargo build -p …` line, the directories that were searched, the command
//! that timed out and for how long — because the alternative is a maintainer
//! re-running the suite by hand to find out.

use std::path::PathBuf;
use std::time::Duration;

/// Why a conformance fixture could not be prepared or driven.
#[derive(Debug)]
#[non_exhaustive]
pub enum FixtureError {
    /// An example binary has not been built.
    MissingBinary {
        /// The binary's name.
        name: String,
        /// Where it was looked for.
        searched: Vec<PathBuf>,
        /// The command that produces it.
        build: String,
    },
    /// The manifest could not be read, parsed, re-serialised or validated.
    Manifest {
        /// The manifest in question.
        path: PathBuf,
        /// What went wrong.
        reason: String,
    },
    /// The staging directory or file could not be written.
    Io {
        /// What was being attempted.
        what: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A child process could not be started at all.
    Spawn {
        /// The command line that was attempted.
        command: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A child process outlived the budget the test gave it and was killed.
    Timeout {
        /// The command line that hung.
        command: String,
        /// How long it was given.
        after: Duration,
        /// Whatever it had printed by the time it was killed.
        output: String,
    },
    /// A child process produced output the test could not interpret.
    Output {
        /// The command line that produced it.
        command: String,
        /// What was wrong with the output.
        reason: String,
        /// The output itself.
        output: String,
    },
}

impl core::fmt::Display for FixtureError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingBinary {
                name,
                searched,
                build,
            } => {
                writeln!(f, "the example binary `{name}` has not been built.")?;
                writeln!(f, "run: {build}")?;
                writeln!(f, "looked in:")?;
                for dir in searched {
                    writeln!(f, "  {}", dir.display())?;
                }
                Ok(())
            }
            Self::Manifest { path, reason } => {
                write!(f, "{}: {reason}", path.display())
            }
            Self::Io { what, source } => write!(f, "{what}: {source}"),
            Self::Spawn { command, source } => {
                write!(f, "could not start `{command}`: {source}")
            }
            Self::Timeout {
                command,
                after,
                output,
            } => {
                writeln!(
                    f,
                    "`{command}` was still running after {after:?} and was killed."
                )?;
                writeln!(f, "what it had printed:")?;
                f.write_str(output)
            }
            Self::Output {
                command,
                reason,
                output,
            } => {
                writeln!(f, "`{command}`: {reason}")?;
                writeln!(f, "what it printed:")?;
                f.write_str(output)
            }
        }
    }
}

impl std::error::Error for FixtureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::Spawn { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// A missing binary names the command that produces it, and every
    /// directory that was tried — the whole point of the variant.
    #[test]
    fn a_missing_binary_names_its_build_command() {
        let error = FixtureError::MissingBinary {
            name: "camera-sim".to_owned(),
            searched: vec![PathBuf::from("/t/debug"), PathBuf::from("/t/release")],
            build: "cargo build -p rust-pipeline".to_owned(),
        };
        let text = error.to_string();
        assert!(text.contains("camera-sim"), "{text}");
        assert!(text.contains("cargo build -p rust-pipeline"), "{text}");
        assert!(text.contains("/t/debug"), "{text}");
        assert!(text.contains("/t/release"), "{text}");
    }

    /// A timeout carries the output the process had produced, so a hang is
    /// diagnosable without re-running it.
    #[test]
    fn a_timeout_carries_the_partial_output() {
        let error = FixtureError::Timeout {
            command: "astrs run x.yml".to_owned(),
            after: Duration::from_secs(3),
            output: "[camera-sim] up\n".to_owned(),
        };
        let text = error.to_string();
        assert!(text.contains("astrs run x.yml"), "{text}");
        assert!(text.contains("3s"), "{text}");
        assert!(text.contains("[camera-sim] up"), "{text}");
    }

    /// The IO and spawn variants keep their cause reachable through
    /// [`std::error::Error::source`].
    #[test]
    fn the_io_variants_keep_their_source() {
        use std::error::Error as _;

        let io = FixtureError::Io {
            what: "create /tmp/x".to_owned(),
            source: std::io::Error::other("disk on fire"),
        };
        assert!(io.source().is_some());
        assert!(io.to_string().contains("disk on fire"));

        let spawn = FixtureError::Spawn {
            command: "astrs".to_owned(),
            source: std::io::Error::other("no such file"),
        };
        assert!(spawn.source().is_some());

        let manifest = FixtureError::Manifest {
            path: PathBuf::from("/t/a.yml"),
            reason: "bad".to_owned(),
        };
        assert!(manifest.source().is_none());
        assert!(manifest.to_string().contains("/t/a.yml"));
    }

    /// Unreadable output is reported with the output attached rather than as a
    /// bare parse error.
    #[test]
    fn unreadable_output_is_shown_not_summarised() {
        let error = FixtureError::Output {
            command: "astrs run --json x.yml".to_owned(),
            reason: "no JSON report was printed".to_owned(),
            output: "the graph said nothing\n".to_owned(),
        };
        let text = error.to_string();
        assert!(text.contains("no JSON report"), "{text}");
        assert!(text.contains("the graph said nothing"), "{text}");
    }
}
