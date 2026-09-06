//! The `astrs` CLI's single error type.
//!
//! Every library-testable command function (`command::validate::run`,
//! `command::expand::run`, ...) returns `Result<Report, CliError>`.
//! [`crate::dispatch`] maps the `Err` case to [`CliError::exit_code`] and a
//! one-line message on the caller's error sink — the only two things
//! `main.rs` needs to turn into a process exit code (blueprint: "main maps
//! to exit codes — no `process::exit` inside lib paths").
//!
//! Most commands realistically never *return* this as an `Err`: a
//! diagnostic tool like `validate` or `doctor` embeds every failure mode
//! (a manifest that fails to parse, a port that is not bindable) as
//! *content* of its typed report instead, so the report itself is what a
//! test asserts against. `CliError` exists for the failure modes that
//! really do abort a command outright — an artifact generator (`expand`,
//! `graph`, `schema`) with nothing sensible to print because its input
//! never became a valid manifest, a refusal from the coordinator, a verb
//! whose engine belongs to a later wave, or a path-safety violation in
//! `new`.

use std::path::Path;

/// `EX_UNAVAILABLE` (sysexits.h §3): the requested command exists but the
/// service it needs is not available. Used for
/// [`CliError::NotImplementedYet`] and [`CliError::FeatureNotEnabled`] so
/// a script can tell "this AstRS build doesn't have this yet" apart from
/// an ordinary usage or data error (exit code `1`) without parsing the
/// message text.
pub const EXIT_UNAVAILABLE: i32 = 69;

/// Everything that can abort an `astrs` command outright.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CliError {
    /// A file could not be read or written.
    #[error("failed to access `{path}`: {source}")]
    Io {
        /// The path that could not be accessed.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The manifest did not even parse as YAML.
    #[error(transparent)]
    Manifest(#[from] astrs_manifest::ManifestError),

    /// The manifest parsed but failed structural validation.
    #[error("manifest failed validation:\n{0}")]
    Validation(#[from] astrs_manifest::ValidationErrors),

    /// Module expansion failed.
    #[error(transparent)]
    Expand(#[from] astrs_manifest::expand::ExpandError),

    /// The (validated, expanded) manifest could not become a dataflow
    /// graph.
    #[error(transparent)]
    Graph(#[from] astrs_graph::GraphBuildError),

    /// `migrate from-dora` failed.
    #[error(transparent)]
    DoraMigrate(#[from] astrs_migrate::DoraMigrateError),

    /// `migrate from-ros2` failed.
    #[error(transparent)]
    Ros2Migrate(#[from] astrs_migrate::Ros2MigrateError),

    /// The embedded daemon (`astrs run`) or the hidden `astrs daemon`
    /// process failed.
    #[error(transparent)]
    Daemon(#[from] astrs_daemon::DaemonError),

    /// The hidden `astrs coordinator` process failed.
    #[error(transparent)]
    Coordinator(#[from] astrs_coordinator::CoordinatorError),

    /// The coordinator's parameter/state store could not be opened.
    #[error("the coordinator store is unusable: {0}")]
    Store(#[from] astrs_store::Error),

    /// A connection to the coordinator failed.
    #[error(transparent)]
    Transport(#[from] astrs_transport::TransportError),

    /// The hidden `astrs runtime` process failed.
    #[error(transparent)]
    Runtime(#[from] astrs_runtime::RuntimeError),

    /// A node could not connect to its daemon (`astrs runtime`).
    #[error(transparent)]
    Node(#[from] astrs_node_api::NodeError),

    /// An `.arec` recording could not be read (`astrs bag info`, `astrs
    /// replay`).
    #[error(transparent)]
    Recording(#[from] astrs_recording::RecordingError),

    /// A rosbag2 `.db3`/`.mcap` file could not be read or written, or a
    /// requested `astrs bag convert` direction is not one this build
    /// bridges (`astrs bag info`, `astrs bag convert`).
    #[error(transparent)]
    Rosbag(#[from] astrs_rosbag::RosbagError),

    /// The `astrs ros2` probe could not join the DDS domain, or its report
    /// could not be rendered (blueprint §17, §10.2).
    ///
    /// A *finding* — an empty domain, a refused multicast join — is never
    /// this: `ros2 doctor` reports those as checks. This is the case where
    /// there is nothing to report on because the participant itself would
    /// not start.
    #[error("the ROS 2 probe failed: {0}")]
    Ros2(String),

    /// `astrs top` could not enter or run the terminal (blueprint §17).
    #[error(transparent)]
    Tui(#[from] astrs_tui::TuiError),

    /// `astrs top` could not connect to the coordinator.
    #[error(transparent)]
    CoordinatorSource(#[from] astrs_tui::CoordinatorSourceError),

    /// `astrs top --replay` could not open its recording.
    #[error(transparent)]
    Replay(#[from] astrs_tui::ReplaySourceError),

    /// The coordinator answered a request with an error reply.
    #[error("the coordinator refused `{request}`: {message}")]
    Refused {
        /// The request that was refused, e.g. `"list"`.
        request: &'static str,
        /// The coordinator's own message.
        message: String,
    },

    /// The coordinator answered with a reply this verb cannot use — a
    /// protocol violation rather than a refusal.
    #[error("the coordinator answered `{request}` with an unexpected `{reply}`")]
    UnexpectedReply {
        /// The request that was sent.
        request: &'static str,
        /// The reply variant that came back.
        reply: &'static str,
    },

    /// No cluster token could be found, and the verb needs one (§16).
    #[error(
        "no cluster token: pass `--token`/`--token-file`, set `{env}`, or run `astrs up` in a directory whose `{file}` this process can read"
    )]
    NoToken {
        /// The environment variable that would have supplied it.
        env: &'static str,
        /// The conventional file name (§16).
        file: &'static str,
    },

    /// A token was found but is not a 64-hex value (§16).
    #[error("the cluster token in `{path}` is not a 64-hex value: {source}")]
    BadToken {
        /// Where the bad token came from.
        path: String,
        /// Why it was rejected.
        #[source]
        source: astrs_wire::AuthTokenError,
    },

    /// A `host[:port]` (or bare port) argument could not be resolved.
    #[error("`{input}` is not a usable coordinator address: {reason}")]
    BadAddress {
        /// What the user typed.
        input: String,
        /// Why it could not be used.
        reason: String,
    },

    /// A flag's value was not of the shape the flag needs.
    #[error("`--{flag} {value}` is not valid: {reason}")]
    BadArgument {
        /// The flag, without its leading dashes.
        flag: &'static str,
        /// The value that was rejected.
        value: String,
        /// Why it was rejected.
        reason: String,
    },

    /// A cluster process could not be started or stopped.
    #[error("could not {action} the {process}: {reason}")]
    Cluster {
        /// What was attempted, e.g. `"start"`.
        action: &'static str,
        /// Which process, e.g. `"coordinator"`.
        process: &'static str,
        /// Why it failed.
        reason: String,
    },

    /// A verb that needs a live cluster found none.
    #[error("no cluster is running here (`{detail}`); start one with `astrs up`")]
    NoCluster {
        /// What the probe actually observed.
        detail: String,
    },

    /// A dataflow reference named nothing the coordinator knows (or named
    /// more than one thing).
    #[error("`{reference}` does not name one dataflow: {reason}")]
    UnknownDataflow {
        /// What the user typed.
        reference: String,
        /// What was found instead.
        reason: String,
    },

    /// A cluster process is already running here, and the verb refuses to
    /// start a second one over it.
    #[error("a {process} is already running here (pid {pid}{address}); stop it with `astrs down`")]
    AlreadyRunning {
        /// Which process, e.g. `"coordinator"`.
        process: &'static str,
        /// Its process id, from the pidfile (§24.2).
        pid: u32,
        /// Where it listens, rendered with a leading separator, or empty.
        address: String,
    },

    /// A verb (or a flag on one) whose implementation belongs to a wave
    /// this build predates.
    #[error(
        "`{verb}` is not implemented yet (scheduled for {wave}); every other verb `astrs --help` lists is available in this build"
    )]
    NotImplementedYet {
        /// The verb (or `group subcommand`) that was invoked, e.g.
        /// `"node add"` or `"ros2 doctor"`.
        verb: &'static str,
        /// The wave this verb is scheduled for, e.g.
        /// [`crate::command::stub::WAVE_BAG`].
        wave: &'static str,
    },

    /// An optional feature exists as a stub but is gated behind a Cargo
    /// feature this build does not enable.
    #[error(
        "{feature} is not enabled in this build (scheduled for {wave}); rebuild with `--features {feature}` once it ships"
    )]
    FeatureNotEnabled {
        /// The Cargo feature name, e.g. `"verify"`.
        feature: &'static str,
        /// The wave this feature is scheduled for, e.g. `"W5/W6"`.
        wave: &'static str,
    },

    /// `astrs validate --prove` ran on a build without the `verify`
    /// feature, so no solver was available to discharge the obligations.
    ///
    /// Distinct from [`Self::FeatureNotEnabled`], which says a feature has
    /// not shipped yet: `verify` has shipped, and the obligations were
    /// still *encoded* and printed — only the verdicts are missing.
    #[error(
        "--prove needs the `verify` feature, which this build does not enable; the obligations above were encoded but not solved. Rebuild with `cargo install astrs-cli --features verify` (or `cargo build -p astrs-cli --features verify`)."
    )]
    ProofUnavailable,

    /// `run --deterministic` was given without `--from-recording`.
    ///
    /// Refused before anything is built or spawned (blueprint §14) — a user
    /// who asks for a deterministic run never gets a non-deterministic one
    /// wearing the label, because there is nothing to fix the timer wheel
    /// to without a recording.
    #[error(
        "--deterministic needs --from-recording <path>: a deterministic run fixes the timer wheel to a recording's HLC stream, and none was given"
    )]
    DeterministicNeedsRecording,

    /// `new` refused to write outside its target directory.
    #[error("refusing to write outside the target directory: `{path}`")]
    UnsafePath {
        /// The offending resolved path.
        path: String,
    },

    /// `new` was asked to write into a directory that already has
    /// conflicting files in it.
    #[error("`{path}` already exists; pass `--force` to overwrite, or choose an empty directory")]
    TargetExists {
        /// The offending path.
        path: String,
    },
}

impl CliError {
    /// Build a [`CliError::Io`] from a path and the I/O failure accessing
    /// it.
    #[must_use]
    pub fn io(path: impl AsRef<Path>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.as_ref().display().to_string(),
            source,
        }
    }

    /// The process exit code this error should produce.
    ///
    /// [`Self::NotImplementedYet`], [`Self::FeatureNotEnabled`] and
    /// [`Self::ProofUnavailable`] use [`EXIT_UNAVAILABLE`] so scripts can
    /// tell "this build cannot do that" apart from an ordinary failure;
    /// everything else is a plain `1` (`EXIT_FAILURE`), matching every
    /// other Unix CLI tool's default.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::NotImplementedYet { .. }
            | Self::FeatureNotEnabled { .. }
            | Self::ProofUnavailable => EXIT_UNAVAILABLE,
            _ => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn proof_unavailable_uses_the_unavailable_exit_code() {
        let err = CliError::ProofUnavailable;
        assert_eq!(err.exit_code(), EXIT_UNAVAILABLE);
        assert!(err.to_string().contains("--features verify"));
    }

    #[test]
    fn not_implemented_yet_uses_the_unavailable_exit_code() {
        let err = CliError::NotImplementedYet {
            verb: "bag info",
            wave: crate::command::stub::WAVE_BAG,
        };
        assert_eq!(err.exit_code(), EXIT_UNAVAILABLE);
        assert!(err.to_string().contains("bag info"));
        assert!(err.to_string().contains("rosbag"));
    }

    #[test]
    fn feature_not_enabled_uses_the_unavailable_exit_code() {
        let err = CliError::FeatureNotEnabled {
            feature: "verify",
            wave: "W5/W6",
        };
        assert_eq!(err.exit_code(), EXIT_UNAVAILABLE);
        assert!(err.to_string().contains("verify"));
    }

    #[test]
    fn ordinary_errors_use_exit_code_one() {
        let err = CliError::UnsafePath {
            path: "/tmp/x".to_string(),
        };
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn io_helper_carries_the_display_path() {
        let err = CliError::io(
            "/tmp/nope",
            std::io::Error::new(std::io::ErrorKind::NotFound, "nope"),
        );
        assert!(err.to_string().contains("/tmp/nope"));
    }

    #[test]
    fn manifest_error_converts_via_from() {
        let source = astrs_manifest::Manifest::from_yaml_str("not: [a, manifest").unwrap_err();
        let err: CliError = source.into();
        assert!(matches!(err, CliError::Manifest(_)));
    }
}
