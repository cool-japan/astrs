//! [`RuntimeError`] — everything that can keep an [`crate::RuntimeHost`] from
//! starting or finishing cleanly.
//!
//! Split from [`astrs_operator_api::OpError`] (an individual operator's own
//! failure, caught per-incarnation and folded into
//! [`crate::OperatorOutcome`], never fatal to the host) the same way this
//! crate splits "wiring the manifest's `operators:` list onto a connected
//! node" from "running the operators once wiring succeeded": everything in
//! this type is a reason the *host itself* cannot proceed, discovered before
//! (or independently of) any operator ever running.

use astrs_wire::IdError;

/// This crate's result alias.
pub type Result<T, E = RuntimeError> = core::result::Result<T, E>;

/// Why an [`crate::RuntimeHost`] could not be built or could not finish.
///
/// `#[non_exhaustive]`: the append-only evolution rule (blueprint §3.4)
/// applies here too.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RuntimeError {
    /// The connected node's session failed — the only way this crate ever
    /// wraps [`astrs_node_api::NodeError`].
    #[error("node session: {0}")]
    Node(#[from] astrs_node_api::NodeError),

    /// An operator id (`operators[].id` in the manifest) is not a valid
    /// [`astrs_wire::NodeId`] — the charset a synthetic [`astrs_wire::PortRef`]
    /// needs when this host reports an intra-runtime edge closing.
    #[error("operator id {id:?} is not a valid identifier: {source}")]
    InvalidOperatorId {
        /// The offending id, as written in the manifest.
        id: String,
        /// Why [`astrs_wire::NodeId::new`] rejected it.
        #[source]
        source: IdError,
    },

    /// An operator input or output name is not a valid
    /// [`astrs_wire::DataId`].
    #[error("operator {operator:?} port {port:?} is not a valid identifier: {source}")]
    InvalidPortName {
        /// The operator that declared it.
        operator: String,
        /// The offending port name.
        port: String,
        /// Why [`astrs_wire::DataId::new`] rejected it.
        #[source]
        source: IdError,
    },

    /// Two entries in `operators:` share an id.
    #[error("operator id {id:?} is declared more than once")]
    DuplicateOperatorId {
        /// The id that collided.
        id: String,
    },

    /// An operator's input names a sibling that does not exist, or a
    /// sibling output that sibling never declared.
    ///
    /// The field holding the manifest's own `source:` string is named
    /// `wanted` rather than `source` on purpose: `thiserror` treats a
    /// field literally named `source` as `Error::source()`, which requires
    /// an [`std::error::Error`] type — this field is plain manifest text,
    /// not a nested error.
    #[error(
        "operator {operator:?} input {input:?} names `{wanted}`, which is neither a sibling \
         operator's declared output nor one of this node's own inputs"
    )]
    UnresolvedOperatorInput {
        /// The operator that declared the input.
        operator: String,
        /// The input's own name.
        input: String,
        /// The `source:` string that could not be resolved.
        wanted: String,
    },

    /// [`astrs_scheduler::InputQueue`] (or the [`astrs_scheduler::EventMux`]
    /// registering it) refused an operator input's queue configuration.
    #[error("operator {operator:?} input {input:?}: {source}")]
    Scheduler {
        /// The operator that declared the input.
        operator: String,
        /// The input's own name.
        input: String,
        /// What the scheduler crate reported.
        #[source]
        source: astrs_scheduler::SchedulerError,
    },

    /// An operator's manifest `config:` value has no equivalent in the
    /// closed [`astrs_wire::Parameter`] vocabulary
    /// [`astrs_operator_api::Operator::configure`] receives (see
    /// [`crate::operator_config`]'s module docs for the mapping rules).
    #[error("operator {operator:?} config key {key:?}: {source}")]
    InvalidConfigValue {
        /// The operator whose `config:` map declared it.
        operator: String,
        /// The offending key.
        key: String,
        /// Why it has no [`astrs_wire::Parameter`] equivalent.
        #[source]
        source: crate::operator_config::ConfigValueError,
    },

    /// An operator's manifest `dylib:` entry could not be loaded — the
    /// shared library does not exist or cannot be opened, exports no
    /// `astrs_operator_descriptor` symbol, or declares an ABI version or
    /// operator name this build does not accept. See [`crate::dylib`]'s
    /// module docs.
    #[cfg(feature = "dylib-operators")]
    #[error("operator {operator:?}: {source}")]
    Dylib {
        /// The operator's manifest id (`operators[].id`).
        operator: String,
        /// Why the library could not be loaded.
        #[source]
        source: crate::dylib::DylibError,
    },

    /// An operator's manifest entry declares a `dylib:` source, but this
    /// build of `astrs-runtime` was compiled without the `dylib-operators`
    /// feature — there is no loader to resolve it with.
    #[error(
        "operator {operator:?} names a `dylib:` source, but this astrs-runtime build has no \
         `dylib-operators` feature"
    )]
    DylibOperatorsNotEnabled {
        /// The operator's manifest id (`operators[].id`).
        operator: String,
    },

    /// An operator's manifest `wasm:` entry could not be loaded — the
    /// module file does not exist or is not valid WebAssembly. See
    /// [`crate::wasm`]'s module docs.
    #[cfg(feature = "wasm-operators")]
    #[error("operator {operator:?}: {source}")]
    Wasm {
        /// The operator's manifest id (`operators[].id`).
        operator: String,
        /// Why the module could not be loaded.
        #[source]
        source: crate::wasm::WasmError,
    },

    /// An operator's manifest entry declares a `wasm:` source, but this
    /// build of `astrs-runtime` was compiled without the `wasm-operators`
    /// feature — there is no interpreter to run it on.
    #[error(
        "operator {operator:?} names a `wasm:` source, but this astrs-runtime build has no \
         `wasm-operators` feature"
    )]
    WasmOperatorsNotEnabled {
        /// The operator's manifest id (`operators[].id`).
        operator: String,
    },
}

impl RuntimeError {
    /// Builds a [`RuntimeError::InvalidOperatorId`] without repeating the id.
    #[must_use]
    pub fn invalid_operator_id(id: impl Into<String>, source: IdError) -> Self {
        Self::InvalidOperatorId {
            id: id.into(),
            source,
        }
    }

    /// Builds a [`RuntimeError::InvalidPortName`] without repeating the
    /// operator and port names.
    #[must_use]
    pub fn invalid_port_name(
        operator: impl Into<String>,
        port: impl Into<String>,
        source: IdError,
    ) -> Self {
        Self::InvalidPortName {
            operator: operator.into(),
            port: port.into(),
            source,
        }
    }

    /// Builds a [`RuntimeError::InvalidConfigValue`] without repeating the
    /// operator and key names.
    #[must_use]
    pub(crate) fn invalid_config_value(
        operator: impl Into<String>,
        key: impl Into<String>,
        source: crate::operator_config::ConfigValueError,
    ) -> Self {
        Self::InvalidConfigValue {
            operator: operator.into(),
            key: key.into(),
            source,
        }
    }

    /// Builds a [`RuntimeError::Dylib`] without repeating the operator id.
    #[cfg(feature = "dylib-operators")]
    #[must_use]
    pub(crate) fn dylib(operator: impl Into<String>, source: crate::dylib::DylibError) -> Self {
        Self::Dylib {
            operator: operator.into(),
            source,
        }
    }

    /// Builds a [`RuntimeError::DylibOperatorsNotEnabled`] without
    /// repeating the operator id.
    ///
    /// Only ever constructed by `crate::host::build_one_constructor`'s
    /// `#[cfg(not(feature = "dylib-operators"))]` half — hence the matching
    /// `cfg` here, rather than an unconditional `pub(crate) fn` that would
    /// go unused (and so trip `dead_code`) whenever that feature *is* on.
    #[cfg(not(feature = "dylib-operators"))]
    #[must_use]
    pub(crate) fn dylib_operators_not_enabled(operator: impl Into<String>) -> Self {
        Self::DylibOperatorsNotEnabled {
            operator: operator.into(),
        }
    }

    /// Builds a [`RuntimeError::Wasm`] without repeating the operator id.
    #[cfg(feature = "wasm-operators")]
    #[must_use]
    pub(crate) fn wasm(operator: impl Into<String>, source: crate::wasm::WasmError) -> Self {
        Self::Wasm {
            operator: operator.into(),
            source,
        }
    }

    /// Builds a [`RuntimeError::WasmOperatorsNotEnabled`] without repeating
    /// the operator id.
    ///
    /// `cfg`-gated the same way, and for the same reason, as
    /// [`RuntimeError::dylib_operators_not_enabled`] above.
    #[cfg(not(feature = "wasm-operators"))]
    #[must_use]
    pub(crate) fn wasm_operators_not_enabled(operator: impl Into<String>) -> Self {
        Self::WasmOperatorsNotEnabled {
            operator: operator.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// The one variant whose constructor exists only under
    /// `dylib-operators` ([`RuntimeError::dylib`]) or only without it
    /// ([`RuntimeError::dylib_operators_not_enabled`]) — see either
    /// constructor's own docs for why each is `cfg`-gated to the one
    /// production call site that ever needs it.
    #[cfg(feature = "dylib-operators")]
    fn dylib_locator_variant() -> RuntimeError {
        RuntimeError::dylib(
            "detect",
            crate::dylib::DylibError::AbiVersionMismatch {
                path: std::path::PathBuf::from("./libdetect.so"),
                found: 99,
                expected: astrs_operator_api::dylib::ASTRS_OPERATOR_ABI_VERSION,
            },
        )
    }

    /// See [`dylib_locator_variant`]'s docs above (the `not` twin).
    #[cfg(not(feature = "dylib-operators"))]
    fn dylib_locator_variant() -> RuntimeError {
        RuntimeError::dylib_operators_not_enabled("detect")
    }

    /// The `wasm:` locator's own equivalent of [`dylib_locator_variant`].
    #[cfg(feature = "wasm-operators")]
    fn wasm_locator_variant() -> RuntimeError {
        RuntimeError::wasm(
            "filter",
            crate::wasm::WasmError::Read {
                path: std::path::PathBuf::from("./filter.wasm"),
                source: std::io::Error::other("not found"),
            },
        )
    }

    /// See [`wasm_locator_variant`]'s docs above (the `not` twin).
    #[cfg(not(feature = "wasm-operators"))]
    fn wasm_locator_variant() -> RuntimeError {
        RuntimeError::wasm_operators_not_enabled("filter")
    }

    #[test]
    fn every_variant_renders_and_implements_error() {
        let errors: Vec<RuntimeError> = vec![
            RuntimeError::Node(astrs_node_api::NodeError::DaemonGone),
            RuntimeError::invalid_operator_id(
                "bad id",
                astrs_wire::NodeId::new("bad id").unwrap_err(),
            ),
            RuntimeError::invalid_port_name(
                "crop",
                "bad port",
                astrs_wire::DataId::new("bad port").unwrap_err(),
            ),
            RuntimeError::DuplicateOperatorId {
                id: "crop".to_owned(),
            },
            RuntimeError::UnresolvedOperatorInput {
                operator: "nms".to_owned(),
                input: "boxes".to_owned(),
                wanted: "camera/frames".to_owned(),
            },
            RuntimeError::Scheduler {
                operator: "crop".to_owned(),
                input: "frames".to_owned(),
                source: astrs_scheduler::SchedulerError::ZeroCapacity,
            },
            RuntimeError::invalid_config_value(
                "crop",
                "threshold",
                crate::operator_config::ConfigValueError::Null,
            ),
            dylib_locator_variant(),
            wasm_locator_variant(),
        ];
        for error in errors {
            assert!(!error.to_string().is_empty(), "{error:?}");
            let _: &dyn std::error::Error = &error;
        }
    }

    #[test]
    fn node_errors_convert_via_from() {
        let error: RuntimeError = astrs_node_api::NodeError::Stopped.into();
        assert!(matches!(error, RuntimeError::Node(_)));
    }
}
