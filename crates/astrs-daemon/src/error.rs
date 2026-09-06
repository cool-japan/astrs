//! [`DaemonError`] — every way the local core can refuse.
//!
//! One error type for the whole crate, with a variant per *cause the caller
//! can act on* rather than per call site. A daemon is a long-running process
//! whose errors mostly become log lines and [`astrs_wire::NodeExitCause`]
//! values, so the interesting distinction is "is this the operator's fault,
//! the manifest's fault, the node's fault, or the machine's fault" — not which
//! function returned it.
//!
//! # Examples
//!
//! ```
//! use astrs_daemon::{DaemonError, DaemonResult};
//!
//! fn refuse() -> DaemonResult<()> {
//!     Err(DaemonError::UnknownDataflow {
//!         dataflow: astrs_wire::DataflowId::from_u128(7),
//!     })
//! }
//!
//! let error = refuse().unwrap_err();
//! assert!(error.is_client_error(), "the caller named a dataflow that is gone");
//! assert_eq!(error.kind_name(), "unknown_dataflow");
//! ```

use std::path::PathBuf;

use astrs_wire::{DataId, DataflowId, NodeId, SessionId};

/// The crate's result alias.
pub type DaemonResult<T> = Result<T, DaemonError>;

/// Everything the daemon's local core can refuse to do.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DaemonError {
    /// An I/O operation failed, with the operation named.
    #[error("{operation} failed: {source}")]
    Io {
        /// What was being attempted.
        operation: &'static str,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// A path the configuration points at is unusable.
    #[error("{what} path {} is unusable: {reason}", path.display())]
    BadPath {
        /// Which path this is.
        what: &'static str,
        /// The offending path.
        path: PathBuf,
        /// Why it cannot be used.
        reason: String,
    },

    /// The daemon has no listener configured at all.
    #[error("the daemon has neither a Unix socket nor a TCP port to listen on")]
    NoListener,

    /// A configured value cannot be used — an unresolvable coordinator
    /// address, a peer address that is not a `TransportAddr`, a machine name
    /// the wire's charset refuses.
    #[error("configuration: {0}")]
    Configuration(String),

    /// A transport-level failure.
    #[error("transport: {0}")]
    Transport(#[from] astrs_transport::TransportError),

    /// A protocol-level failure.
    #[error("protocol: {0}")]
    Wire(#[from] astrs_wire::WireError),

    /// A manifest could not be read or validated.
    #[error("manifest: {0}")]
    Manifest(String),

    /// The graph could not be built from the manifest.
    #[error("graph: {0}")]
    Graph(String),

    /// A node's `args:` could not be split into an argv.
    #[error("node {node}: cannot split arguments {input:?} using shell word rules")]
    BadArgv {
        /// The node whose arguments these are.
        node: NodeId,
        /// The offending string.
        input: String,
    },

    /// A node's environment could not be expanded.
    #[error("node {node}: cannot expand environment: {reason}")]
    BadEnv {
        /// The node whose environment this is.
        node: NodeId,
        /// What went wrong.
        reason: String,
    },

    /// A node process could not be started.
    #[error("node {node}: cannot spawn {program:?}: {source}")]
    Spawn {
        /// The node that could not start.
        node: NodeId,
        /// The program that was going to run.
        program: String,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// A build line failed.
    #[error("node {node}: build step {step:?} exited with {code}")]
    BuildFailed {
        /// The node whose build failed.
        node: NodeId,
        /// The command line that failed.
        step: String,
        /// How it failed.
        code: String,
    },

    /// The daemon does not know this dataflow.
    #[error("dataflow {dataflow} is not known to this daemon")]
    UnknownDataflow {
        /// The identifier that was presented.
        dataflow: DataflowId,
    },

    /// The dataflow does not contain this node.
    #[error("dataflow {dataflow} has no node {node}")]
    UnknownNode {
        /// The dataflow.
        dataflow: DataflowId,
        /// The node that was named.
        node: NodeId,
    },

    /// A session spoke before registering, or about something it does not own.
    #[error("session {session} is not registered")]
    UnregisteredSession {
        /// The session that spoke out of turn.
        session: SessionId,
    },

    /// A node registered twice, or two processes claimed the same node.
    #[error("node {node} of dataflow {dataflow} is already registered at generation {generation}")]
    DuplicateRegistration {
        /// The dataflow.
        dataflow: DataflowId,
        /// The node.
        node: NodeId,
        /// The generation already holding the slot.
        generation: u64,
    },

    /// A node registered with a generation that is not the current one.
    #[error(
        "node {node} of dataflow {dataflow} registered at generation {presented}, \
         but the daemon is running generation {current}"
    )]
    StaleGeneration {
        /// The dataflow.
        dataflow: DataflowId,
        /// The node.
        node: NodeId,
        /// What the node claimed.
        presented: u64,
        /// What the daemon expects.
        current: u64,
    },

    /// A node published on an output its specification does not declare.
    #[error("node {node} of dataflow {dataflow} does not declare output {output}")]
    UnknownOutput {
        /// The dataflow.
        dataflow: DataflowId,
        /// The node.
        node: NodeId,
        /// The output it tried to use.
        output: DataId,
    },

    /// A node subscribed to an input its specification does not declare.
    #[error("node {node} of dataflow {dataflow} does not declare input {input}")]
    UnknownInput {
        /// The dataflow.
        dataflow: DataflowId,
        /// The node.
        node: NodeId,
        /// The input it tried to use.
        input: DataId,
    },

    /// A node tried to write a reserved extension namespace (§2.1).
    #[error("extension namespace {namespace} is reserved for the daemon")]
    ReservedNamespace {
        /// The namespace that was refused.
        namespace: astrs_wire::ExtensionNamespace,
    },

    /// The dataflow is not in a state where this operation makes sense.
    #[error("dataflow {dataflow} is {state}, which does not allow {operation}")]
    BadState {
        /// The dataflow.
        dataflow: DataflowId,
        /// Its current state.
        state: &'static str,
        /// What was attempted.
        operation: &'static str,
    },

    /// The daemon is shutting down and will not take new work.
    #[error("the daemon is shutting down")]
    ShuttingDown,

    /// An internal channel closed, which means the peer task is gone.
    #[error("the {what} channel closed")]
    ChannelClosed {
        /// Which channel.
        what: &'static str,
    },

    /// A lock was poisoned by a panicking thread.
    #[error("the {what} lock was poisoned by a panicking thread")]
    LockPoisoned {
        /// Which lock.
        what: &'static str,
    },

    /// An operation exceeded its budget.
    #[error("{operation} timed out after {millis} ms")]
    Timeout {
        /// What was being waited for.
        operation: &'static str,
        /// The budget that elapsed.
        millis: u64,
    },

    /// This daemon has no shared-memory plane (§6.2).
    ///
    /// Not a fault: the platform may not implement one, or the operator may
    /// have turned it off. Every route simply stays on the reliable daemon
    /// path.
    #[error("the shared-memory plane is unavailable: {message}")]
    ShmUnavailable {
        /// Why there is no plane.
        message: String,
    },

    /// A shared-memory segment could not be created or reached.
    #[error("shared-memory segment {segment}: {message}")]
    ShmSegment {
        /// The segment's canonical key.
        segment: String,
        /// What the plane reported.
        message: String,
    },

    /// A peer daemon is not connected (§6.4).
    #[error("daemon {daemon} is not connected")]
    UnknownPeer {
        /// The peer that was addressed.
        daemon: astrs_wire::DaemonId,
    },

    /// A route handle does not name an established route (§6.4).
    #[error("route {route} is not established with daemon {daemon}")]
    UnknownRoute {
        /// The peer the route was expected on.
        daemon: astrs_wire::DaemonId,
        /// The handle.
        route: astrs_wire::RouteId,
    },

    /// A peer refused a route setup (§6.4).
    #[error("daemon {daemon} refused route {route}: {reason}")]
    RouteRefused {
        /// The peer that refused.
        daemon: astrs_wire::DaemonId,
        /// The handle it refused.
        route: astrs_wire::RouteId,
        /// Why.
        reason: String,
    },
}

impl DaemonError {
    /// Wraps an I/O failure with the operation that produced it.
    #[must_use]
    pub const fn io(operation: &'static str, source: std::io::Error) -> Self {
        Self::Io { operation, source }
    }

    /// A stable, lower-case name for logs and metric labels.
    ///
    /// Kept exhaustive on purpose: a new variant must be named here, which is
    /// how a metric label never silently becomes `"other"`.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Io { .. } => "io",
            Self::BadPath { .. } => "bad_path",
            Self::NoListener => "no_listener",
            Self::Configuration(_) => "configuration",
            Self::Transport(_) => "transport",
            Self::Wire(_) => "wire",
            Self::Manifest(_) => "manifest",
            Self::Graph(_) => "graph",
            Self::BadArgv { .. } => "bad_argv",
            Self::BadEnv { .. } => "bad_env",
            Self::Spawn { .. } => "spawn",
            Self::BuildFailed { .. } => "build_failed",
            Self::UnknownDataflow { .. } => "unknown_dataflow",
            Self::UnknownNode { .. } => "unknown_node",
            Self::UnregisteredSession { .. } => "unregistered_session",
            Self::DuplicateRegistration { .. } => "duplicate_registration",
            Self::StaleGeneration { .. } => "stale_generation",
            Self::UnknownOutput { .. } => "unknown_output",
            Self::UnknownInput { .. } => "unknown_input",
            Self::ReservedNamespace { .. } => "reserved_namespace",
            Self::BadState { .. } => "bad_state",
            Self::ShuttingDown => "shutting_down",
            Self::ChannelClosed { .. } => "channel_closed",
            Self::LockPoisoned { .. } => "lock_poisoned",
            Self::Timeout { .. } => "timeout",
            Self::ShmUnavailable { .. } => "shm_unavailable",
            Self::ShmSegment { .. } => "shm_segment",
            Self::UnknownPeer { .. } => "unknown_peer",
            Self::UnknownRoute { .. } => "unknown_route",
            Self::RouteRefused { .. } => "route_refused",
        }
    }

    /// Whether the caller asked for something that cannot exist, as opposed to
    /// something that went wrong while doing it.
    ///
    /// Drives the daemon's answer to a control request: a client error is
    /// reported and forgotten; anything else is also logged as an incident.
    #[must_use]
    pub const fn is_client_error(&self) -> bool {
        matches!(
            self,
            Self::UnknownDataflow { .. }
                | Self::UnknownNode { .. }
                | Self::UnknownOutput { .. }
                | Self::UnknownInput { .. }
                | Self::UnregisteredSession { .. }
                | Self::DuplicateRegistration { .. }
                | Self::StaleGeneration { .. }
                | Self::ReservedNamespace { .. }
                | Self::BadState { .. }
                | Self::BadArgv { .. }
                | Self::BadEnv { .. }
                | Self::UnknownPeer { .. }
                | Self::UnknownRoute { .. }
                | Self::Configuration(_)
        )
    }

    /// Whether retrying the same operation could plausibly succeed.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Io { .. } | Self::Timeout { .. } | Self::ShmSegment { .. }
        )
    }

    /// Turns a poisoned-lock error into this crate's error rather than a
    /// panic, so the `unwrap`-free policy survives contact with `Mutex`.
    #[must_use]
    pub const fn poisoned(what: &'static str) -> Self {
        Self::LockPoisoned { what }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn sample_errors() -> Vec<DaemonError> {
        vec![
            DaemonError::io("bind", std::io::Error::other("nope")),
            DaemonError::Configuration("no address resolves".into()),
            DaemonError::BadPath {
                what: "runtime dir",
                path: PathBuf::from("/nope"),
                reason: "not a directory".into(),
            },
            DaemonError::NoListener,
            DaemonError::Manifest("bad".into()),
            DaemonError::Graph("cycle".into()),
            DaemonError::BadArgv {
                node: NodeId::new("a").unwrap(),
                input: "'unterminated".into(),
            },
            DaemonError::BadEnv {
                node: NodeId::new("a").unwrap(),
                reason: "missing $HOME".into(),
            },
            DaemonError::Spawn {
                node: NodeId::new("a").unwrap(),
                program: "./missing".into(),
                source: std::io::Error::other("nope"),
            },
            DaemonError::BuildFailed {
                node: NodeId::new("a").unwrap(),
                step: "cargo build".into(),
                code: "exit 1".into(),
            },
            DaemonError::UnknownDataflow {
                dataflow: DataflowId::from_u128(1),
            },
            DaemonError::UnknownNode {
                dataflow: DataflowId::from_u128(1),
                node: NodeId::new("a").unwrap(),
            },
            DaemonError::UnregisteredSession {
                session: SessionId::from_u128(3),
            },
            DaemonError::DuplicateRegistration {
                dataflow: DataflowId::from_u128(1),
                node: NodeId::new("a").unwrap(),
                generation: 2,
            },
            DaemonError::StaleGeneration {
                dataflow: DataflowId::from_u128(1),
                node: NodeId::new("a").unwrap(),
                presented: 1,
                current: 2,
            },
            DaemonError::UnknownOutput {
                dataflow: DataflowId::from_u128(1),
                node: NodeId::new("a").unwrap(),
                output: DataId::new("out").unwrap(),
            },
            DaemonError::UnknownInput {
                dataflow: DataflowId::from_u128(1),
                node: NodeId::new("a").unwrap(),
                input: DataId::new("in").unwrap(),
            },
            DaemonError::ReservedNamespace {
                namespace: astrs_wire::ExtensionNamespace::Internal,
            },
            DaemonError::BadState {
                dataflow: DataflowId::from_u128(1),
                state: "finished",
                operation: "start",
            },
            DaemonError::ShuttingDown,
            DaemonError::ChannelClosed { what: "internal" },
            DaemonError::poisoned("state"),
            DaemonError::Timeout {
                operation: "stop",
                millis: 500,
            },
        ]
    }

    #[test]
    fn every_variant_renders_and_names_itself() {
        for error in sample_errors() {
            let rendered = error.to_string();
            assert!(!rendered.is_empty(), "{error:?}");
            let name = error.kind_name();
            assert!(!name.is_empty());
            assert_eq!(name.to_lowercase(), name, "{name} should be lower case");
            assert!(
                !name.contains(' '),
                "{name} should be a single metric-safe token"
            );
        }
    }

    #[test]
    fn kind_names_are_unique_per_variant() {
        let mut names: Vec<&str> = sample_errors().iter().map(DaemonError::kind_name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "kind names collide");
    }

    #[test]
    fn client_errors_are_the_ones_a_caller_can_fix() {
        assert!(
            DaemonError::UnknownDataflow {
                dataflow: DataflowId::from_u128(1)
            }
            .is_client_error()
        );
        assert!(
            !DaemonError::io("bind", std::io::Error::other("nope")).is_client_error(),
            "an I/O failure is the machine's problem, not the caller's"
        );
    }

    #[test]
    fn only_transient_failures_are_retryable() {
        assert!(DaemonError::io("read", std::io::Error::other("eintr")).is_retryable());
        assert!(
            DaemonError::Timeout {
                operation: "stop",
                millis: 1
            }
            .is_retryable()
        );
        assert!(!DaemonError::NoListener.is_retryable());
    }

    #[test]
    fn io_errors_keep_their_operation_and_source() {
        let error = DaemonError::io("bind", std::io::Error::other("address in use"));
        let rendered = error.to_string();
        assert!(rendered.starts_with("bind failed"), "{rendered}");
        assert!(rendered.contains("address in use"), "{rendered}");
        assert!(std::error::Error::source(&error).is_some());
    }
}
