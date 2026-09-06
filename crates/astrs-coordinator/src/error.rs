//! The coordinator's internal error type and its mapping onto the wire's
//! [`ErrorCode`](astrs_wire::ErrorCode).
//!
//! Everything that can go wrong inside the coordinator funnels through
//! [`CoordinatorError`] before it ever reaches a CLI session: a manifest that
//! fails to parse, a store write that fails, a daemon that never answers, an
//! operation this build does not yet implement server-side. The point of one
//! error type is that [`CoordinatorError::code`] is the *only* place that
//! decides which [`astrs_wire::ErrorCode`] a failure becomes — a
//! [`crate::handlers`] function never has to invent one at the call site.

use astrs_wire::ErrorCode;

/// Everything that can fail inside the coordinator.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CoordinatorError {
    /// The manifest failed to parse.
    #[error("manifest error: {0}")]
    Manifest(#[from] astrs_manifest::ManifestError),
    /// The manifest failed structural validation.
    #[error("manifest validation failed: {0}")]
    Validation(#[from] astrs_manifest::ValidationErrors),
    /// Module expansion failed.
    #[error("module expansion failed: {0}")]
    Expand(#[from] astrs_manifest::expand::ExpandError),
    /// The graph model could not be built from the manifest.
    #[error("graph construction failed: {0}")]
    GraphBuild(#[from] astrs_graph::GraphBuildError),
    /// Applying a dynamic topology change to the tracked graph failed.
    #[error("topology change rejected: {0}")]
    Apply(#[from] astrs_graph::ApplyError),
    /// The durable store failed.
    #[error("store error: {0}")]
    Store(#[from] astrs_store::Error),
    /// An identifier failed validation when crossing from a graph/manifest
    /// string into a wire-typed id.
    #[error("invalid identifier: {0}")]
    Id(#[from] astrs_wire::IdError),
    /// The wire codec failed.
    #[error("wire codec error: {0}")]
    Wire(#[from] astrs_wire::WireError),
    /// The framed-transport layer failed.
    #[error("transport error: {0}")]
    Transport(#[from] astrs_transport::TransportError),
    /// Binding or accepting on the coordinator's listening socket failed —
    /// below the handshake, so it has no [`astrs_transport::TransportError`]
    /// of its own to carry.
    #[error("network I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A command line in the manifest (`build:`) could not be split into
    /// argv (blueprint §16: no shell by default).
    #[error("cannot split command line {command:?}: {reason}")]
    BadCommandLine {
        /// The offending command line.
        command: String,
        /// Why `shlex` rejected it (typically an unterminated quote).
        reason: String,
    },
    /// The request named a dataflow that is not registered.
    #[error("no such dataflow: {0}")]
    NoSuchDataflow(astrs_wire::DataflowId),
    /// The request named a dataflow by a name no active run was started
    /// under.
    #[error("no dataflow named {0:?}")]
    NoSuchDataflowName(String),
    /// The request named a node that does not exist in the dataflow.
    #[error("no such node: {dataflow}/{node}")]
    NoSuchNode {
        /// The dataflow that was searched.
        dataflow: astrs_wire::DataflowId,
        /// The node that was not found.
        node: astrs_wire::NodeId,
    },
    /// The request named a build that is not tracked.
    #[error("no such build: {0}")]
    NoSuchBuild(astrs_wire::BuildId),
    /// The node names a machine that has no daemon registered under it.
    #[error("no daemon registered for machine {0:?}")]
    NoDaemonForMachine(String),
    /// The dataflow has no daemon at all to place work on (no daemons
    /// connected, and no machine names it a coordinator-local run either).
    #[error("no daemons are connected")]
    NoDaemonsConnected,
    /// The named daemon is not currently connected.
    #[error("daemon {0} is not connected")]
    DaemonNotConnected(astrs_wire::DaemonId),
    /// The requested operation is precondition-checked and the dataflow is
    /// not in a state that allows it.
    #[error("dataflow {dataflow} is {status}, which does not allow {action}")]
    WrongDataflowState {
        /// The dataflow.
        dataflow: astrs_wire::DataflowId,
        /// Its current status.
        status: astrs_wire::DataflowStatus,
        /// The action that was refused.
        action: &'static str,
    },
    /// Waiting for a build or a spawn to finish ran past its deadline.
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    /// A dataflow, node or param name collided with one that already
    /// exists.
    #[error("{kind} {name:?} already exists")]
    AlreadyExists {
        /// A short label for what collided (`"node"`, `"dataflow"`, ...).
        kind: &'static str,
        /// The colliding name.
        name: String,
    },
    /// The caller asked for something this build's coordinator-side
    /// protocol has no dispatch for yet (blueprint's documented W4 gap:
    /// dynamic edge rewiring on an already-running node has no
    /// `CoordinatorEvent` verb to carry it to a daemon).
    #[error("{0} is not implemented yet: {1}")]
    NotYetSupported(&'static str, &'static str),
    /// A caller-supplied argument was self-contradictory or malformed in a
    /// way none of the above cover.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

impl CoordinatorError {
    /// An [`CoordinatorError::InvalidArgument`] built from a formatted
    /// message.
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidArgument(message.into())
    }

    /// The [`ErrorCode`] a `crate::handlers` function reports for this
    /// failure.
    ///
    /// This is the one place that decides the mapping, so two call sites
    /// reporting "the same kind of problem" can never silently disagree
    /// about which code an operator's tooling sees.
    #[must_use]
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::Manifest(_)
            | Self::Expand(_)
            | Self::BadCommandLine { .. }
            | Self::InvalidArgument(_)
            | Self::Id(_) => ErrorCode::InvalidArgument,
            Self::Validation(_) | Self::GraphBuild(_) => ErrorCode::ValidationFailed,
            Self::Apply(_) => ErrorCode::FailedPrecondition,
            Self::Store(_) | Self::Wire(_) => ErrorCode::Internal,
            Self::Transport(_) | Self::Io(_) => ErrorCode::Unavailable,
            Self::NoSuchDataflow(_)
            | Self::NoSuchDataflowName(_)
            | Self::NoSuchNode { .. }
            | Self::NoSuchBuild(_)
            | Self::NoDaemonForMachine(_) => ErrorCode::NotFound,
            Self::NoDaemonsConnected | Self::DaemonNotConnected(_) => ErrorCode::Unavailable,
            Self::WrongDataflowState { .. } => ErrorCode::FailedPrecondition,
            Self::Timeout(_) => ErrorCode::Timeout,
            Self::AlreadyExists { .. } => ErrorCode::AlreadyExists,
            Self::NotYetSupported(..) => ErrorCode::Unsupported,
        }
    }

    /// Turns this error into a [`astrs_wire::ControlReply::Error`].
    ///
    /// [`Self::Apply`] wrapping [`astrs_graph::ApplyError::TypeUnsafe`] is
    /// the one variant with more to say than its own `Display` renders:
    /// that message names only *how many* edges would newly violate
    /// strict-mode type checking, since [`astrs_graph::ApplyError`] is a
    /// Layer 2 type with no wire-level context field to put the detail
    /// in. This carries each rejected edge's own [`astrs_graph::Diagnostic`]
    /// (already `Display`-rendered, the same text `astrs validate` prints)
    /// into the reply's `context` — a caller reading `dynamic add/replace/
    /// connect was rejected` should see *which* edge and *why*, not just a
    /// count, so a type-unsafe request never reads as a bare "no".
    #[must_use]
    pub fn into_reply(self) -> astrs_wire::ControlReply {
        let context: Vec<String> = match &self {
            Self::Apply(astrs_graph::ApplyError::TypeUnsafe { mismatches, .. }) => mismatches
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
            _ => Vec::new(),
        };
        if context.is_empty() {
            astrs_wire::ControlReply::error(self.code(), self.to_string())
        } else {
            astrs_wire::ControlReply::error_with_context(self.code(), self.to_string(), context)
        }
    }
}

/// This crate's `Result` alias.
pub type Result<T> = std::result::Result<T, CoordinatorError>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn every_variant_maps_to_a_sensible_error_code() {
        let dataflow = astrs_wire::DataflowId::from_u128(1);
        let cases: Vec<(CoordinatorError, ErrorCode)> = vec![
            (CoordinatorError::invalid("bad"), ErrorCode::InvalidArgument),
            (
                CoordinatorError::NoSuchDataflow(dataflow),
                ErrorCode::NotFound,
            ),
            (CoordinatorError::NoDaemonsConnected, ErrorCode::Unavailable),
            (
                CoordinatorError::WrongDataflowState {
                    dataflow,
                    status: astrs_wire::DataflowStatus::Running,
                    action: "start",
                },
                ErrorCode::FailedPrecondition,
            ),
            (CoordinatorError::Timeout("build"), ErrorCode::Timeout),
            (
                CoordinatorError::AlreadyExists {
                    kind: "node",
                    name: "camera".into(),
                },
                ErrorCode::AlreadyExists,
            ),
            (
                CoordinatorError::Io(std::io::Error::from(std::io::ErrorKind::AddrInUse)),
                ErrorCode::Unavailable,
            ),
            (
                CoordinatorError::NotYetSupported("AddEdge", "no wire verb yet"),
                ErrorCode::Unsupported,
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.code(), expected, "{err}");
        }
    }

    #[test]
    fn into_reply_carries_the_message_and_code() {
        let err = CoordinatorError::NoSuchBuild(astrs_wire::BuildId::from_u128(9));
        let message = err.to_string();
        let reply = err.into_reply();
        match reply {
            astrs_wire::ControlReply::Error {
                code, message: got, ..
            } => {
                assert_eq!(code, ErrorCode::NotFound);
                assert_eq!(got, message);
            }
            other => panic!("expected an Error reply, got {other:?}"),
        }
    }

    #[test]
    fn a_type_unsafe_apply_error_carries_each_mismatch_as_reply_context() {
        let edge = astrs_graph::EdgeKey::new(
            astrs_graph::NodeId::new("detector"),
            astrs_graph::PortName::new("frames"),
        );
        let mismatch = astrs_graph::Diagnostic::new(
            astrs_graph::Severity::Error,
            astrs_graph::DiagnosticKind::TypeMismatch {
                edge: edge.clone(),
                expected: astrs_manifest::Urn::new("std/core/v1/Float64"),
                found: astrs_manifest::Urn::new("std/core/v1/Float32"),
            },
        );
        let err = CoordinatorError::Apply(astrs_graph::ApplyError::TypeUnsafe {
            count: 1,
            mismatches: vec![mismatch.clone()],
        });
        assert_eq!(err.code(), ErrorCode::FailedPrecondition);
        let reply = err.into_reply();
        match reply {
            astrs_wire::ControlReply::Error { context, .. } => {
                assert_eq!(context, vec![mismatch.to_string()]);
            }
            other => panic!("expected an Error reply, got {other:?}"),
        }
    }

    #[test]
    fn an_ordinary_apply_error_has_no_context_to_add() {
        let err = CoordinatorError::Apply(astrs_graph::ApplyError::NodeAlreadyExists {
            id: astrs_graph::NodeId::new("camera"),
        });
        match err.into_reply() {
            astrs_wire::ControlReply::Error { context, .. } => assert!(context.is_empty()),
            other => panic!("expected an Error reply, got {other:?}"),
        }
    }

    #[test]
    fn store_errors_are_reported_as_internal_not_leaked_verbatim_as_a_caller_fault() {
        let store_err = astrs_store::Error::InvalidCompactionTarget {
            requested: astrs_store::record::MutationSeq::new(5),
            current_max: astrs_store::record::MutationSeq::new(1),
        };
        let err = CoordinatorError::from(store_err);
        assert_eq!(err.code(), ErrorCode::Internal);
    }
}
