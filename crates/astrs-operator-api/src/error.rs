//! [`OpError`] — everything an operator or its host can report.
//!
//! Kept separate from [`astrs_data::DataError`] and [`astrs_data::ipc::IpcError`]
//! the same way `astrs-data`'s own IPC layer keeps its error type separate
//! from the columnar core's (see that crate's `ipc::error` docs): an
//! operator failure and a malformed payload are different problems with
//! different recovery paths, and wrapping keeps each one's context intact
//! instead of flattening everything into strings.

use astrs_wire::IdError;

/// The result type every [`crate::Operator`] method and every
/// [`crate::OpOutput`] send returns.
pub type OpResult<T> = Result<T, OpError>;

/// A failure inside the operator host's boundary (blueprint §9.3).
///
/// `#[non_exhaustive]`: the append-only evolution rule (blueprint §3.4)
/// applies to this crate's error type too.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OpError {
    /// [`crate::OpOutput::send_bytes`], [`crate::OpOutput::send_batch`] or
    /// [`crate::OpOutput::send`] was given a name that is not a valid
    /// [`astrs_wire::DataId`].
    #[error("output {id:?} is not a valid data id: {source}")]
    InvalidOutputId {
        /// The offending name, as given.
        id: String,
        /// Why [`astrs_wire::DataId::new`] rejected it.
        #[source]
        source: IdError,
    },

    /// [`crate::OpOutput::send`] or [`crate::OpOutput::send_batch`] failed to
    /// build the outgoing [`astrs_data::RecordBatch`].
    #[error(transparent)]
    Encode(#[from] astrs_data::DataError),

    /// [`crate::OpOutput::send`] or [`crate::OpOutput::send_batch`] failed to
    /// encode the outgoing batch as an Arrow IPC payload.
    #[error(transparent)]
    Ipc(#[from] astrs_data::ipc::IpcError),

    /// [`crate::OperatorRegistry::register`] was given a name already
    /// registered.
    #[error("operator {name:?} is already registered")]
    DuplicateOperator {
        /// The name that collided.
        name: String,
    },

    /// [`crate::OperatorRegistry::build`] was asked for a name with no
    /// registered constructor.
    #[error("no operator registered as {name:?}")]
    UnknownOperator {
        /// The name that was requested.
        name: String,
    },

    /// An operator reported its own domain failure.
    ///
    /// The escape hatch for operator authors whose failure does not fit any
    /// of the structured variants above — still typed (a `String`, not a
    /// panic), and still routed through the same `NodeFailed` reporting path
    /// (blueprint §9.3: "panics are caught, reported as `NodeFailed`").
    #[error("operator failed: {message}")]
    Failed {
        /// A human-readable description of the failure.
        message: String,
    },
}

impl OpError {
    /// Builds an [`OpError::InvalidOutputId`] without repeating the id at
    /// every call site.
    #[must_use]
    pub fn invalid_output_id(id: impl Into<String>, source: IdError) -> Self {
        Self::InvalidOutputId {
            id: id.into(),
            source,
        }
    }

    /// Builds an [`OpError::Failed`] from any displayable message.
    ///
    /// ```
    /// use astrs_operator_api::OpError;
    ///
    /// let err = OpError::failed("model did not load");
    /// assert_eq!(err.to_string(), "operator failed: model did not load");
    /// ```
    #[must_use]
    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn invalid_output_id_reports_the_offending_name() {
        let source = astrs_wire::DataId::new("bad id").unwrap_err();
        let err = OpError::invalid_output_id("bad id", source);
        let message = err.to_string();
        assert!(message.starts_with("output \"bad id\" is not a valid data id: "));
        assert!(matches!(err, OpError::InvalidOutputId { .. }));
    }

    #[test]
    fn duplicate_and_unknown_operator_messages() {
        assert_eq!(
            OpError::DuplicateOperator {
                name: "crop".to_owned()
            }
            .to_string(),
            "operator \"crop\" is already registered"
        );
        assert_eq!(
            OpError::UnknownOperator {
                name: "nope".to_owned()
            }
            .to_string(),
            "no operator registered as \"nope\""
        );
    }

    #[test]
    fn encode_and_ipc_errors_convert_via_from() {
        let data_err: OpError = astrs_data::DataError::MessageRowCount { actual: 2 }.into();
        assert!(matches!(data_err, OpError::Encode(_)));
    }

    #[test]
    fn failed_builds_a_readable_message() {
        assert_eq!(OpError::failed("boom").to_string(), "operator failed: boom");
    }
}
