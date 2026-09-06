//! [`NodeError`] — every way a node can fail, named.
//!
//! One error type for the whole surface, because a node author writes
//! `-> Result<(), NodeError>` on `main` exactly once and never wants to think
//! about it again (blueprint §9.1). The variants are grouped by *who is at
//! fault*, which is what a caller actually branches on:
//!
//! | Group | Variants | Typical response |
//! |---|---|---|
//! | Environment | [`NodeError::Config`], [`NodeError::MissingEnv`], [`NodeError::BadEnv`] | The node was launched wrong; exit non-zero |
//! | Connection | [`NodeError::Connect`], [`NodeError::Transport`], [`NodeError::Handshake`], [`NodeError::DaemonGone`] | The daemon is unreachable; exit and let supervision restart |
//! | Lifecycle | [`NodeError::Stopped`], [`NodeError::Orphaned`] | Wind down cleanly; these are *expected* endings |
//! | Usage | [`NodeError::UnknownOutput`], [`NodeError::UnknownInput`], [`NodeError::TypeMismatch`], [`NodeError::BlockingInAsync`] | A bug in the node; fix the call |
//! | Payload | [`NodeError::Data`], [`NodeError::PayloadTooLarge`], [`NodeError::Shm`] | Bad or oversized data |
//!
//! # Examples
//!
//! ```
//! use astrs_node_api::NodeError;
//!
//! let error = NodeError::Stopped;
//! assert!(error.is_shutdown());
//! assert!(!error.is_retryable());
//! assert_eq!(error.exit_code(), 0, "a requested stop is a clean exit");
//! ```

use astrs_data::ipc::IpcError;
use astrs_data::{DataError, TypeUrnError};
use astrs_scheduler::SchedulerError;
use astrs_shm::ShmError;
use astrs_transport::TransportError;
use astrs_wire::{DataId, IdError, NodeConfigError, StopCause, WireError};

/// The node API's result alias.
pub type Result<T, E = NodeError> = core::result::Result<T, E>;

/// Everything that can go wrong in a node.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NodeError {
    /// The `ASTRS_NODE_CONFIG` blob was missing, corrupt or unusable (§24.2).
    #[error("node configuration: {0}")]
    Config(#[source] NodeConfigError),

    /// A required environment variable was unset.
    #[error("environment variable {name} is not set (was this node spawned by an AstRS daemon?)")]
    MissingEnv {
        /// The variable that was looked for.
        name: &'static str,
    },

    /// An environment variable held something this build cannot parse.
    #[error("environment variable {name}={value} is invalid: {reason}")]
    BadEnv {
        /// The variable.
        name: &'static str,
        /// What it held.
        value: String,
        /// Why that is not usable.
        reason: String,
    },

    /// No daemon endpoint could be dialled.
    ///
    /// Carries every attempt, because "connection refused" on the Unix socket
    /// and "no route to host" on the TCP fallback are two different
    /// diagnoses and a node that reports only the last one sends its operator
    /// looking in the wrong place.
    #[error("could not reach the daemon: {}", format_attempts(.attempts))]
    Connect {
        /// One `(endpoint, reason)` pair per attempt, in the order tried.
        attempts: Vec<(String, String)>,
    },

    /// The daemon refused the greeting, or the greeting could not be
    /// completed (§7.2).
    #[error("handshake with the daemon failed: {0}")]
    Handshake(String),

    /// The daemon refused the registration (§7.3 `Register`).
    #[error("the daemon refused this node's registration: {0}")]
    Registration(String),

    /// The connection to the daemon ended.
    #[error("the daemon connection closed")]
    DaemonGone,

    /// A transport-level failure.
    #[error("transport: {0}")]
    Transport(#[source] TransportError),

    /// A protocol-level failure.
    #[error("wire protocol: {0}")]
    Wire(#[source] WireError),

    /// A columnar payload could not be built, encoded or decoded.
    #[error("payload: {0}")]
    Data(#[source] DataError),

    /// An Arrow IPC stream could not be encoded or decoded (§6.1).
    #[error("payload encoding: {0}")]
    Ipc(#[source] IpcError),

    /// A type URN was malformed, unknown, or did not match a port's declared
    /// layout.
    #[error("type: {0}")]
    TypeUrn(#[source] TypeUrnError),

    /// A shared-memory operation failed (§6.2).
    #[error("shared memory: {0}")]
    Shm(#[source] ShmError),

    /// An identifier failed its grammar.
    #[error("identifier: {0}")]
    Id(#[source] IdError),

    /// An input queue could not be created or registered (§11.2).
    #[error("scheduler: {0}")]
    Scheduler(#[source] SchedulerError),

    /// The node has no such output.
    #[error("this node has no output named `{output}`")]
    UnknownOutput {
        /// The name that was asked for.
        output: DataId,
    },

    /// The node has no such input.
    #[error("this node has no input named `{input}`")]
    UnknownInput {
        /// The name that was asked for.
        input: DataId,
    },

    /// A typed handle was opened on a port whose declared URN says something
    /// else (§9.2, `ASTRS_TYPE_CHECK=error`).
    #[error("port `{port}` is declared as `{declared}`, but the handle is typed `{requested}`")]
    TypeMismatch {
        /// The port.
        port: DataId,
        /// What the manifest says.
        declared: String,
        /// What the code asked for.
        requested: String,
    },

    /// A payload exceeded the §6.1 cap or the negotiated frame limit.
    #[error("payload of {len} bytes exceeds the {max}-byte limit")]
    PayloadTooLarge {
        /// The payload size.
        len: usize,
        /// The applicable ceiling.
        max: usize,
    },

    /// A blocking call was made from a single-threaded async context, where
    /// blocking would deadlock the only worker.
    ///
    /// The fix is always the same: call the `_async` twin of whatever was
    /// called, or run the node's loop on a multi-thread runtime.
    #[error(
        "{method} blocks, and this is a current-thread tokio runtime; call {alternative} instead"
    )]
    BlockingInAsync {
        /// The blocking method that was called.
        method: &'static str,
        /// The method to call instead.
        alternative: &'static str,
    },

    /// The node was asked to stop (§7.3 `Stop`).
    ///
    /// Returned by operations attempted after the event stream fused. It is
    /// an *expected* ending, not a failure — see [`NodeError::is_shutdown`].
    #[error("the dataflow stopped this node")]
    Stopped,

    /// The supervising process disappeared, so this node is an orphan (§4.2).
    #[error("the supervising process {parent_pid} exited; this node is orphaned")]
    Orphaned {
        /// The process that went away.
        parent_pid: u32,
    },

    /// An operation ran out of time.
    #[error("{operation} timed out after {millis} ms")]
    Timeout {
        /// What was being waited for.
        operation: &'static str,
        /// How long it was waited for.
        millis: u64,
    },

    /// The request channel to the daemon is full and the caller asked not to
    /// wait.
    ///
    /// The channel is bounded on purpose: an unbounded one turns a wedged
    /// daemon into unbounded memory growth in every node at once.
    #[error("the daemon request queue is full ({depth} messages)")]
    Backpressure {
        /// The channel's capacity.
        depth: usize,
    },

    /// A tokio runtime could not be built for a node created outside one.
    #[error("could not start a runtime for this node: {0}")]
    Runtime(String),

    /// A pattern helper was used out of order (§9.4).
    #[error("{0}")]
    Pattern(String),

    /// The testing harness could not be driven as asked.
    #[error("test harness: {0}")]
    Testing(String),
}

impl NodeError {
    /// Whether this error means "shut down cleanly" rather than "something
    /// broke".
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_node_api::NodeError;
    ///
    /// assert!(NodeError::Stopped.is_shutdown());
    /// assert!(NodeError::Orphaned { parent_pid: 1 }.is_shutdown());
    /// assert!(!NodeError::DaemonGone.is_shutdown());
    /// ```
    #[must_use]
    pub const fn is_shutdown(&self) -> bool {
        matches!(self, Self::Stopped | Self::Orphaned { .. })
    }

    /// Whether retrying the same operation could plausibly succeed.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_node_api::NodeError;
    ///
    /// assert!(NodeError::Backpressure { depth: 64 }.is_retryable());
    /// assert!(!NodeError::UnknownInput { input: "x".parse()? }.is_retryable());
    /// # Ok::<(), astrs_wire::IdError>(())
    /// ```
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Backpressure { .. } | Self::Timeout { .. } | Self::Connect { .. }
        )
    }

    /// Whether this error is the node's own fault — a call that can never
    /// succeed as written.
    #[must_use]
    pub const fn is_usage_error(&self) -> bool {
        matches!(
            self,
            Self::UnknownOutput { .. }
                | Self::UnknownInput { .. }
                | Self::TypeMismatch { .. }
                | Self::BlockingInAsync { .. }
                | Self::Pattern(_)
        )
    }

    /// The process exit status a `main` should use for this error.
    ///
    /// A clean shutdown is `0`; everything else is `1`, which is what the
    /// daemon's restart policy (§12) reads as a failure.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_node_api::NodeError;
    ///
    /// assert_eq!(NodeError::Stopped.exit_code(), 0);
    /// assert_eq!(NodeError::DaemonGone.exit_code(), 1);
    /// ```
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        if self.is_shutdown() { 0 } else { 1 }
    }

    /// A short, stable slug for metrics labels and structured logs.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Config(_) => "config",
            Self::MissingEnv { .. } => "missing_env",
            Self::BadEnv { .. } => "bad_env",
            Self::Connect { .. } => "connect",
            Self::Handshake(_) => "handshake",
            Self::Registration(_) => "registration",
            Self::DaemonGone => "daemon_gone",
            Self::Transport(_) => "transport",
            Self::Wire(_) => "wire",
            Self::Data(_) => "data",
            Self::Ipc(_) => "ipc",
            Self::TypeUrn(_) => "type_urn",
            Self::Shm(_) => "shm",
            Self::Id(_) => "id",
            Self::Scheduler(_) => "scheduler",
            Self::UnknownOutput { .. } => "unknown_output",
            Self::UnknownInput { .. } => "unknown_input",
            Self::TypeMismatch { .. } => "type_mismatch",
            Self::PayloadTooLarge { .. } => "payload_too_large",
            Self::BlockingInAsync { .. } => "blocking_in_async",
            Self::Stopped => "stopped",
            Self::Orphaned { .. } => "orphaned",
            Self::Timeout { .. } => "timeout",
            Self::Backpressure { .. } => "backpressure",
            Self::Runtime(_) => "runtime",
            Self::Pattern(_) => "pattern",
            Self::Testing(_) => "testing",
        }
    }

    /// The stop cause a node should report for this error, when it reports
    /// one.
    #[must_use]
    pub const fn stop_cause(&self) -> Option<StopCause> {
        match self {
            Self::Stopped => Some(StopCause::Requested),
            Self::Orphaned { .. } | Self::DaemonGone => Some(StopCause::DaemonShutdown),
            _ => None,
        }
    }
}

/// Renders the attempt list of [`NodeError::Connect`].
fn format_attempts(attempts: &[(String, String)]) -> String {
    if attempts.is_empty() {
        return "no endpoints were configured".to_owned();
    }
    attempts
        .iter()
        .map(|(endpoint, reason)| format!("{endpoint} ({reason})"))
        .collect::<Vec<_>>()
        .join("; ")
}

impl From<NodeConfigError> for NodeError {
    fn from(error: NodeConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<TransportError> for NodeError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl From<WireError> for NodeError {
    fn from(error: WireError) -> Self {
        Self::Wire(error)
    }
}

impl From<DataError> for NodeError {
    fn from(error: DataError) -> Self {
        Self::Data(error)
    }
}

impl From<IpcError> for NodeError {
    fn from(error: IpcError) -> Self {
        Self::Ipc(error)
    }
}

impl From<TypeUrnError> for NodeError {
    fn from(error: TypeUrnError) -> Self {
        Self::TypeUrn(error)
    }
}

impl From<ShmError> for NodeError {
    fn from(error: ShmError) -> Self {
        Self::Shm(error)
    }
}

impl From<IdError> for NodeError {
    fn from(error: IdError) -> Self {
        Self::Id(error)
    }
}

impl From<SchedulerError> for NodeError {
    fn from(error: SchedulerError) -> Self {
        Self::Scheduler(error)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn every_variant() -> Vec<NodeError> {
        vec![
            NodeError::Config(NodeConfigError::Base64(
                astrs_wire::Base64Error::NonCanonicalTail,
            )),
            NodeError::MissingEnv {
                name: "ASTRS_NODE_CONFIG",
            },
            NodeError::BadEnv {
                name: "ASTRS_TYPE_CHECK",
                value: "loud".to_owned(),
                reason: "expected off, warn or error".to_owned(),
            },
            NodeError::Connect {
                attempts: vec![("uds:/tmp/x".to_owned(), "refused".to_owned())],
            },
            NodeError::Handshake("refused".to_owned()),
            NodeError::Registration("wrong generation".to_owned()),
            NodeError::DaemonGone,
            NodeError::Transport(TransportError::Closed {
                reason: astrs_transport::CloseReason::Eof,
            }),
            NodeError::Wire(WireError::MissingCrc),
            NodeError::Data(DataError::AllocationFailed { bytes: 8 }),
            NodeError::Ipc(IpcError::EmptyStream),
            NodeError::TypeUrn(TypeUrnError::UnknownType {
                urn: "std/core/v1/Nope".to_owned(),
            }),
            NodeError::Shm(ShmError::Closed {
                name: "astrs-test".to_owned(),
            }),
            NodeError::Id(astrs_wire::DataId::new("bad name").unwrap_err()),
            NodeError::Scheduler(SchedulerError::DuplicateInput(
                astrs_wire::DataId::new("x").unwrap(),
            )),
            NodeError::UnknownOutput {
                output: astrs_wire::DataId::new("nope").unwrap(),
            },
            NodeError::UnknownInput {
                input: astrs_wire::DataId::new("nope").unwrap(),
            },
            NodeError::TypeMismatch {
                port: astrs_wire::DataId::new("image").unwrap(),
                declared: "std/media/v1/Image".to_owned(),
                requested: "std/core/v1/Float64".to_owned(),
            },
            NodeError::PayloadTooLarge { len: 1, max: 0 },
            NodeError::BlockingInAsync {
                method: "EventStream::recv",
                alternative: "EventStream::recv_async",
            },
            NodeError::Stopped,
            NodeError::Orphaned { parent_pid: 7 },
            NodeError::Timeout {
                operation: "ext_load",
                millis: 500,
            },
            NodeError::Backpressure { depth: 64 },
            NodeError::Runtime("no threads".to_owned()),
            NodeError::Pattern("respond() without a request_id".to_owned()),
            NodeError::Testing("no such node".to_owned()),
        ]
    }

    #[test]
    fn every_variant_renders_and_classifies() {
        for error in every_variant() {
            assert!(!error.to_string().is_empty(), "{error:?}");
            assert!(!error.kind_name().is_empty(), "{error:?}");
            assert!(error.exit_code() == 0 || error.exit_code() == 1);
            let _: &dyn std::error::Error = &error;
        }
    }

    #[test]
    fn kind_names_are_unique() {
        let mut names: Vec<&str> = every_variant().iter().map(NodeError::kind_name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "kind names must identify a variant");
    }

    #[test]
    fn shutdown_errors_exit_zero() {
        assert!(NodeError::Stopped.is_shutdown());
        assert_eq!(NodeError::Stopped.exit_code(), 0);
        assert!(NodeError::Orphaned { parent_pid: 3 }.is_shutdown());
        assert_eq!(NodeError::Orphaned { parent_pid: 3 }.exit_code(), 0);
        assert!(!NodeError::DaemonGone.is_shutdown());
        assert_eq!(NodeError::DaemonGone.exit_code(), 1);
    }

    #[test]
    fn retryable_and_usage_classes_are_disjoint() {
        for error in every_variant() {
            assert!(
                !(error.is_retryable() && error.is_usage_error()),
                "{error:?} cannot be both"
            );
        }
        assert!(NodeError::Backpressure { depth: 1 }.is_retryable());
        assert!(
            NodeError::BlockingInAsync {
                method: "a",
                alternative: "b"
            }
            .is_usage_error()
        );
    }

    #[test]
    fn stop_causes_are_reported_only_for_endings() {
        assert_eq!(NodeError::Stopped.stop_cause(), Some(StopCause::Requested));
        assert_eq!(
            NodeError::DaemonGone.stop_cause(),
            Some(StopCause::DaemonShutdown)
        );
        assert_eq!(NodeError::Handshake(String::new()).stop_cause(), None);
    }

    #[test]
    fn connect_errors_list_every_attempt() {
        let error = NodeError::Connect {
            attempts: vec![
                (
                    "uds:/run/astrs/daemon.sock".to_owned(),
                    "refused".to_owned(),
                ),
                ("tcp:127.0.0.1:7408".to_owned(), "timed out".to_owned()),
            ],
        };
        let text = error.to_string();
        assert!(text.contains("daemon.sock"), "{text}");
        assert!(text.contains("127.0.0.1:7408"), "{text}");
        assert!(
            text.contains("refused") && text.contains("timed out"),
            "{text}"
        );

        let empty = NodeError::Connect {
            attempts: Vec::new(),
        };
        assert!(empty.to_string().contains("no endpoints"));
    }

    #[test]
    fn conversions_wrap_rather_than_flatten() {
        let wire: NodeError = WireError::MissingCrc.into();
        assert_eq!(wire.kind_name(), "wire");
        let id: NodeError = astrs_wire::DataId::new("").unwrap_err().into();
        assert_eq!(id.kind_name(), "id");
        let shm: NodeError = ShmError::Closed {
            name: "astrs-test".to_owned(),
        }
        .into();
        assert_eq!(shm.kind_name(), "shm");
        let data: NodeError = DataError::AllocationFailed { bytes: 8 }.into();
        assert_eq!(data.kind_name(), "data");
    }
}
